//! Preferences live in the durable event log; the run memory limit lives in systemd.

use super::history::{newest_first, pick, restore, versions_of};
use crate::{AppState, Fail};
use axum::Json;
use axum::extract::{Query, State};
use dvb_core::api::{
    PublicationChange, PublishingIdentity, ResponseStyle, Settings, SettingsHistory,
    SettingsHistoryQuery, SettingsKey, SettingsRestore,
};
use dvb_core::{Actor, EventKind, Refusal, RefusalCode};
use serde_json::Value;

pub(super) fn snapshot(e: &AppState) -> Settings {
    with_defaults(&e.proj().settings, &e.pool)
}

fn with_defaults(settings: &Settings, pool: &dvb_core::ModelPool) -> Settings {
    let mut settings = settings.clone();
    settings
        .values
        .entry(SettingsKey::ModelPool)
        .or_insert_with(|| {
            serde_json::json!(
                pool.models
                    .iter()
                    .map(|model| &model.id)
                    .collect::<Vec<_>>()
            )
        });
    settings
}

pub(super) fn valid(e: &AppState, key: SettingsKey, value: &Value) -> bool {
    let choice = |choices: &[&str]| value.as_str().is_some_and(|v| choices.contains(&v));
    match key {
        SettingsKey::MaxRuns => value.as_u64().is_some_and(|n| n >= 1),
        SettingsKey::RunsMemoryGb => value.as_u64().is_some_and(|n| n >= 2),
        SettingsKey::ResponseStyle => {
            serde_json::from_value::<ResponseStyle>(value.clone()).is_ok()
        }
        SettingsKey::QuestionFormat => choice(&["simple", "quick_compare", "swot"]),
        SettingsKey::NotifyFaults
        | SettingsKey::NotifyCompleted
        | SettingsKey::TaskFilesRight
        | SettingsKey::VendorFallback => value.is_boolean(),
        SettingsKey::ModelMode => {
            serde_json::from_value::<dvb_core::api::ModelMode>(value.clone()).is_ok()
        }
        SettingsKey::TextSize => choice(&["S", "M", "L", "XL"]),
        SettingsKey::Motion => choice(&["Follow system", "On", "Off"]),
        SettingsKey::Font => value
            .as_str()
            .is_some_and(|s| !s.is_empty() && s.len() <= 200 && !s.chars().any(char::is_control)),
        SettingsKey::Theme => value.as_str().is_some_and(|s| {
            !s.is_empty()
                && s.len() <= 100
                && s.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        }),
        SettingsKey::ModelPool => value.as_array().is_some_and(|ids| {
            !ids.is_empty()
                && ids
                    .iter()
                    .all(|id| id.as_str().is_some_and(|id| e.pool.find(id).is_some()))
        }),
        SettingsKey::TaskPanels => value.as_object().is_some_and(|v| {
            v.len() == 2
                && v.get("review").is_some_and(Value::is_boolean)
                && v.get("checks").is_some_and(Value::is_boolean)
        }),
        SettingsKey::AutonomousProjects => value.as_array().is_some_and(|names| {
            let proj = e.proj();
            names.iter().all(|name| {
                name.as_str()
                    .is_some_and(|name| proj.projects.values().any(|p| p.view.name == name))
            })
        }),
    }
}

#[utoipa::path(
    get,
    path = "/settings",
    operation_id = "get_settings",
    tag = "settings",
    responses(crate::api::ErrorResponses, (status = 200, description = "Settings", body = dvb_core::api::Settings))
)]
pub async fn get_settings(State(e): State<AppState>) -> Result<Json<Settings>, Fail> {
    let mut settings = snapshot(&e);
    read_memory(&e, &mut settings).await;
    Ok(Json(settings))
}

async fn read_memory(e: &AppState, settings: &mut Settings) {
    if let Err(error) = crate::runs_memory::read(&e.cfg, settings).await {
        settings.values.remove(&SettingsKey::RunsMemoryGb);
        eprintln!("dvb engine: cannot read run memory settings: {error}");
    }
}

#[utoipa::path(
    put,
    path = "/settings",
    operation_id = "put_settings",
    tag = "settings",
    request_body = dvb_core::api::Settings,
    responses(crate::api::ErrorResponses, (status = 200, description = "Settings after the change", body = dvb_core::api::Settings))
)]
pub async fn put_settings(
    State(e): State<AppState>,
    body: Result<Json<Settings>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<Settings>, Fail> {
    let Json(mut body) = body.map_err(|error| {
        e.refuse(
            None,
            Actor::Owner,
            "settings",
            Refusal::new(RefusalCode::InvalidValue, error.body_text()),
        )
    })?;
    // Validate the whole patch before writing any key.
    for (key, value) in &body.values {
        if !valid(&e, *key, value) {
            return Err(e.refuse(
                None,
                Actor::Owner,
                "settings",
                Refusal::new(
                    RefusalCode::InvalidValue,
                    format!("unsupported value for setting {}", key.as_str()),
                ),
            ));
        }
    }
    if let Some(gb) = body
        .values
        .remove(&SettingsKey::RunsMemoryGb)
        .and_then(|new| new.as_u64())
    {
        let mut current = snapshot(&e);
        crate::runs_memory::read(&e.cfg, &mut current).await?;
        let error = if !current.values.contains_key(&SettingsKey::RunsMemoryGb) {
            Some("this engine has no dvb.slice; install the engine units before saving runs_memory_gb".into())
        } else {
            current
                .ram_gb
                .filter(|ram| gb > *ram)
                .map(|ram| format!("runs_memory_gb must be between 2 and {ram} GiB (PC RAM)"))
        };
        if let Some(error) = error {
            return Err(e.refuse(
                None,
                Actor::Owner,
                "settings",
                Refusal::new(RefusalCode::InvalidValue, error),
            ));
        }
        crate::runs_memory::save(&e.cfg, gb).await?;
    }
    let mut saved = None;
    e.append_checked(None, Actor::Owner, |proj, _| {
        let mut settings = with_defaults(&proj.settings, &e.pool);
        let mut kinds = Vec::new();
        for (key, new) in body.values {
            let old = settings.values.get(&key).cloned().unwrap_or(Value::Null);
            if old != new {
                kinds.push(EventKind::SettingsChanged {
                    key: key.as_str().into(),
                    old,
                    new: new.clone(),
                    restored_from: None,
                });
                settings.values.insert(key, new);
            }
        }
        saved = Some(settings);
        Ok(kinds)
    })?;
    let mut settings = saved.ok_or_else(|| Fail::internal("settings result missing"))?;
    read_memory(&e, &mut settings).await;
    Ok(Json(settings))
}

#[utoipa::path(
    get,
    path = "/settings/history",
    operation_id = "settings_history",
    tag = "settings",
    params(dvb_core::api::SettingsHistoryQuery),
    responses(crate::api::ErrorResponses, (status = 200, description = "The setting's versions, newest first", body = dvb_core::api::SettingsHistory))
)]
pub async fn settings_history(
    State(e): State<AppState>,
    Query(q): Query<SettingsHistoryQuery>,
) -> Result<Json<SettingsHistory>, Fail> {
    let found = versions_of(&e, |k| setting_of(k, q.key.as_str()))?;
    Ok(Json(SettingsHistory {
        key: q.key,
        versions: newest_first(found, |v| v),
    }))
}

#[utoipa::path(
    post,
    path = "/settings/restore",
    operation_id = "restore_settings",
    tag = "settings",
    request_body = dvb_core::api::SettingsRestore,
    responses(crate::api::ErrorResponses, (status = 200, description = "Settings after the restore", body = dvb_core::api::Settings))
)]
pub async fn restore_settings(
    State(e): State<AppState>,
    Json(body): Json<SettingsRestore>,
) -> Result<Json<Settings>, Fail> {
    if body.key == SettingsKey::RunsMemoryGb {
        return Err(super::invalid(
            "runs_memory_gb is live systemd state and has no saved history",
        ));
    }
    let key = body.key.as_str();
    restore(&e, None, "settings.restore", |_| {
        let found = versions_of(&e, |k| setting_of(k, key))?;
        let new = pick(
            &found,
            &format!("setting {key}"),
            (body.event, body.current),
        )?;
        if body.key == dvb_core::api::SettingsKey::MaxRuns && !valid(&e, body.key, &new) {
            return Err(Refusal::new(
                RefusalCode::InvalidValue,
                "unsupported value for setting max_runs",
            )
            .into());
        }
        let old = found.last().map(|(_, v)| v.clone()).unwrap_or_default();
        Ok(vec![EventKind::SettingsChanged {
            key: key.into(),
            old,
            new,
            restored_from: Some(body.event),
        }])
    })?;
    get_settings(State(e)).await
}

#[utoipa::path(
    get,
    path = "/settings/publication",
    operation_id = "get_publication",
    tag = "settings",
    responses(crate::api::ErrorResponses, (status = 200, description = "The identity dvb publishes commits as", body = dvb_core::api::PublishingIdentity))
)]
pub async fn get_publication(State(e): State<AppState>) -> Json<PublishingIdentity> {
    Json(e.publication().identity())
}

/// Saved in `state/publication.json` like the owner identity App setup
/// stores; the next commit dvb publishes uses it. Without `as_owner`, an
/// empty name or email is dvb's default. A refusal's detail names the field
/// and the rule it broke.
#[utoipa::path(
    put,
    path = "/settings/publication",
    operation_id = "set_publication",
    tag = "settings",
    request_body = dvb_core::api::PublicationChange,
    responses(crate::api::ErrorResponses, (status = 200, description = "The identity dvb now publishes commits as", body = dvb_core::api::PublishingIdentity))
)]
pub async fn set_publication(
    State(e): State<AppState>,
    Json(body): Json<PublicationChange>,
) -> Result<Json<PublishingIdentity>, Fail> {
    let _effect = e.update.effect().await;
    // GitHub setup rewrites the same file with the owner identity.
    let _setup = e.github_setup.lock().await;
    let given = |value: &str, default: &str| match value.trim() {
        "" if !body.as_owner => default.to_owned(),
        value => value.to_owned(),
    };
    let mut publication = e.publication();
    publication.name = given(&body.name, crate::publish::NEUTRAL_NAME);
    publication.email = given(&body.email, crate::publish::NEUTRAL_EMAIL);
    publication.as_owner = body.as_owner;
    publication
        .validate()
        .map_err(|error| super::invalid(error.to_string()))?;
    publication
        .save(&e.cfg.paths.state)
        .map_err(|error| Fail::context("cannot save publication settings", error))?;
    let identity = publication.identity();
    *e.publication.lock().unwrap_or_else(dvb_core::sync::recover) = publication;
    Ok(Json(identity))
}

pub(super) fn setting_of(kind: &EventKind, key: &str) -> Option<serde_json::Value> {
    let EventKind::SettingsChanged { key: k, new, .. } = kind else {
        return None;
    };
    (k == key).then(|| new.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Config, Engine};
    use serde_json::json;

    #[tokio::test]
    async fn runs_memory_is_live_validated_warned_and_written_only_on_save() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::dev(dir.path(), "dvb".into());
        let systemctl = dir.path().join("systemctl");
        cfg.systemctl = Some(systemctl.clone());
        cfg.meminfo = dir.path().join("meminfo");
        std::fs::write(&cfg.meminfo, "MemTotal: 33423360 kB\n").unwrap();
        std::fs::write(
            &systemctl,
            r##"#!/bin/sh
set -eu
cd "$(dirname "$0")"
printf '%s\n' "$*" >> calls
[ "$1" = --no-ask-password ]; shift
case "$1" in
show)
  [ ! -e fail-read ] || { echo 'read denied' >&2; exit 1; }
  cat show ;;
set-property)
  [ ! -e fail ] || { echo 'denied' >&2; exit 1; }
  n=${3#MemoryMax=}; n=${n%G}
  printf 'LoadState=loaded\nFragmentPath=/etc/systemd/system/dvb.slice\nDropInPaths=\nMemoryMax=%s\n' "$((n * 1073741824))" > show ;;
*) exit 1 ;;
esac
"##,
        )
        .unwrap();
        std::fs::set_permissions(&systemctl, std::fs::Permissions::from_mode(0o755)).unwrap();
        let show = dir.path().join("show");
        let calls = dir.path().join("calls");
        std::fs::write(
            &show,
            "LoadState=loaded\nFragmentPath=/etc/systemd/system/dvb.slice\nDropInPaths=\nMemoryMax=8589934592\n",
        )
        .unwrap();
        let e = Engine::open(cfg.clone(), Default::default()).unwrap();
        assert!(!calls.exists());
        let read = get_settings(State(e.clone())).await.unwrap().0;
        assert_eq!(
            std::fs::read_to_string(&calls).unwrap().trim(),
            "--no-ask-password show dvb.slice -p FragmentPath -p DropInPaths -p MemoryMax"
        );
        assert_eq!(read.values["runs_memory_gb"], 8);
        assert_eq!(read.ram_gb, Some(31));
        assert_eq!(read.half_ram_gb, Some(15));
        assert_eq!(read.runs_memory_warning(), None);
        for value in [
            json!(1),
            json!(32),
            json!(-1),
            json!(1.5),
            json!("8"),
            json!(null),
            json!(true),
        ] {
            let patch = serde_json::from_value(json!({"values": {
                "runs_memory_gb": value, "notify_completed": true
            }}))
            .unwrap();
            assert!(
                matches!(put_settings(State(e.clone()), Ok(Json(patch))).await,
                Err(Fail::Refused(r)) if r.code == RefusalCode::InvalidValue)
            );
            assert_eq!(e.proj().settings.values["notify_completed"], false);
        }
        for (limit, warning) in [(2, false), (15, false), (16, true), (31, true)] {
            let before = e.store.last_id().unwrap();
            let patch =
                serde_json::from_value(json!({"values": {"runs_memory_gb": limit}})).unwrap();
            let saved = put_settings(State(e.clone()), Ok(Json(patch)))
                .await
                .unwrap()
                .0;
            assert_eq!(saved.values["runs_memory_gb"], limit);
            assert_eq!(saved.runs_memory_warning().is_some(), warning);
            if warning {
                assert_eq!(
                    saved.runs_memory_warning(),
                    Some(
                        "Several runs building at once can use this much memory and freeze the desktop."
                    )
                );
            }
            assert_eq!(e.store.last_id().unwrap(), before);
            assert!(!e.proj().settings.values.contains_key("runs_memory_gb"));
        }
        let expected_saves = [2, 15, 16, 31].map(|n| {
            format!(
                "--no-ask-password set-property dvb.slice MemoryMax={n}G MemoryHigh={}M",
                n * 896
            )
        });
        let writes = || {
            std::fs::read_to_string(&calls)
                .unwrap()
                .lines()
                .filter(|line| line.contains("set-property"))
                .map(str::to_owned)
                .collect::<Vec<_>>()
        };
        assert_eq!(writes(), expected_saves);
        std::fs::write(
            &show,
            "LoadState=loaded\nFragmentPath=\nDropInPaths=/etc/systemd/system/dvb.slice.d/50-memory.conf\nMemoryMax=7516192768\n",
        )
        .unwrap();
        assert_eq!(
            get_settings(State(e.clone())).await.unwrap().0.values["runs_memory_gb"],
            7
        );
        let patch = serde_json::from_value(json!({"values": {"notify_completed": true}})).unwrap();
        let _ = put_settings(State(e.clone()), Ok(Json(patch)))
            .await
            .unwrap();
        assert_eq!(writes(), expected_saves);
        drop(e);
        let e = Engine::open(cfg, Default::default()).unwrap();
        assert_eq!(
            get_settings(State(e.clone())).await.unwrap().0.values["runs_memory_gb"],
            7
        );
        assert_eq!(writes(), expected_saves);
        let patch = || serde_json::from_value(json!({"values": {"runs_memory_gb": 8}})).unwrap();
        std::fs::write(dir.path().join("fail"), "").unwrap();
        assert!(
            put_settings(State(e.clone()), Ok(Json(patch())))
                .await
                .is_err()
        );
        assert_eq!(
            get_settings(State(e.clone())).await.unwrap().0.values["runs_memory_gb"],
            7
        );
        let before = writes();
        for missing_program in [false, true] {
            if missing_program {
                std::fs::rename(&systemctl, dir.path().join("systemctl-away")).unwrap();
            } else {
                std::fs::write(dir.path().join("fail-read"), "").unwrap();
            }
            let read = get_settings(State(e.clone())).await.unwrap().0;
            assert!(!read.values.contains_key("runs_memory_gb"));
            assert_eq!(read.values["notify_completed"], true);
            let notification_patch = serde_json::from_value(json!({
                "values": {"notify_completed": !missing_program}
            }))
            .unwrap();
            let saved = put_settings(State(e.clone()), Ok(Json(notification_patch)))
                .await
                .unwrap()
                .0;
            assert!(!saved.values.contains_key("runs_memory_gb"));
            assert_eq!(saved.values["notify_completed"], !missing_program);
            assert_eq!(
                e.proj().settings.values["notify_completed"],
                !missing_program
            );
            let error = put_settings(State(e.clone()), Ok(Json(patch())))
                .await
                .unwrap_err();
            assert!(!error.to_string().contains("no dvb.slice"));
            if missing_program {
                std::fs::rename(dir.path().join("systemctl-away"), &systemctl).unwrap();
            } else {
                std::fs::remove_file(dir.path().join("fail-read")).unwrap();
            }
        }
        assert_eq!(writes(), before);
        std::fs::write(
            &show,
            "LoadState=loaded\nFragmentPath=\nDropInPaths=\nMemoryMax=infinity\n",
        )
        .unwrap();
        assert!(
            !get_settings(State(e.clone()))
                .await
                .unwrap()
                .0
                .values
                .contains_key("runs_memory_gb")
        );
        let error = put_settings(State(e), Ok(Json(patch()))).await.unwrap_err();
        assert!(
            matches!(error, Fail::Refused(r) if r.code == RefusalCode::InvalidValue && r.detail.contains("no dvb.slice"))
        );
        assert_eq!(writes(), before);
    }

    #[test]
    fn folded_settings_replay_and_current_reads_do_not_scan_retained_history() {
        use dvb_core::{EventId, ProjectId, RunId};
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::dev(dir.path(), "unused".into());
        let e = Engine::open(cfg.clone(), Default::default()).unwrap();
        let kinds = [
            ("model_mode", json!("Auto")),
            ("model_pool", json!(["fake-1"])),
            ("question_format", json!("swot")),
            ("response_style", json!("Expert")),
            ("github_activity", json!(false)),
        ]
        .into_iter()
        .map(|(key, new)| EventKind::SettingsChanged {
            key: key.into(),
            old: Value::Null,
            new,
            restored_from: None,
        })
        .collect();
        e.append_checked(None, Actor::Owner, |_, _| Ok(kinds))
            .unwrap();
        let p = ProjectId::new("p");
        let output = e
            .append_checked(Some(&p), Actor::Engine, |_, _| {
                Ok(vec![EventKind::RunOutput {
                    resource: None,
                    run: RunId::new("old"),
                    item: dvb_core::event::OutputItem::Text("old".into()),
                }])
            })
            .unwrap()[0]
            .id;
        let before = snapshot(&e);
        drop(e);
        let e = Engine::open(cfg, Default::default()).unwrap();
        assert_eq!(snapshot(&e), before);
        assert_eq!(
            e.proj().settings.model_mode(),
            dvb_core::api::ModelMode::Auto
        );
        assert_eq!(
            e.proj().settings.question_format(),
            dvb_core::event::QuestionFormat::Swot
        );
        assert!(!before.values.contains_key("github_activity"));
        let conn = rusqlite::Connection::open(e.cfg.paths.state.join("dvb.sqlite")).unwrap();
        conn.execute(
            "UPDATE events SET payload = 'invalid' WHERE id = ?1",
            [i64::try_from(output.0).unwrap()],
        )
        .unwrap();
        assert!(e.store.after(EventId(0)).is_err());
        assert_eq!(snapshot(&e), before);
        assert_eq!(e.selected_models(&e.proj())[0].id, "fake-1");
    }

    #[test]
    fn response_style_default_agrees_with_prompt_before_any_setting_event() {
        let dir = tempfile::tempdir().unwrap();
        let e = Engine::open(
            Config::dev(dir.path(), "unused-dvb".into()),
            Default::default(),
        )
        .unwrap();
        assert_eq!(
            snapshot(&e).values["response_style"],
            crate::prompt::DEFAULT_STYLE
        );
        assert!(!crate::prompt::style_instruction(crate::prompt::DEFAULT_STYLE).is_empty());
    }

    #[tokio::test]
    async fn max_runs_defaults_validates_before_writes_and_replays() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::dev(dir.path(), "dvb".into());
        let e = Engine::open(cfg.clone(), Default::default()).unwrap();
        assert_eq!(snapshot(&e).values["max_runs"], 3);
        for limit in [1, 6, u64::MAX] {
            let patch = serde_json::from_value(json!({"values": {"max_runs": limit}})).unwrap();
            let saved = put_settings(State(e.clone()), Ok(Json(patch)))
                .await
                .unwrap()
                .0;
            assert_eq!(saved.values["max_runs"], limit);
            assert_eq!(e.proj().max_runs(), limit);
        }
        let before = snapshot(&e);
        for limit in [
            json!(0),
            json!(-1),
            json!(1.5),
            json!("6"),
            json!(null),
            json!(true),
        ] {
            let patch = serde_json::from_value(
                json!({"values": {"max_runs": limit, "notify_completed": true}}),
            )
            .unwrap();
            assert!(
                matches!(put_settings(State(e.clone()), Ok(Json(patch))).await, Err(Fail::Refused(r)) if r.code == RefusalCode::InvalidValue)
            );
            assert_eq!(snapshot(&e), before);
        }
        drop(e);
        let e = Engine::open(cfg, Default::default()).unwrap();
        assert_eq!(snapshot(&e), before);
        assert_eq!(e.proj().max_runs(), u64::MAX);
    }

    #[tokio::test]
    async fn max_runs_history_restore_validates_before_writing() {
        use dvb_core::api::SettingsRestore;
        let dir = tempfile::tempdir().unwrap();
        let e = Engine::open(Config::dev(dir.path(), "dvb".into()), Default::default()).unwrap();
        let invalid = e
            .append_checked(None, Actor::Owner, |_, _| {
                Ok(vec![EventKind::SettingsChanged {
                    key: "max_runs".into(),
                    old: json!(3),
                    new: json!(0),
                    restored_from: None,
                }])
            })
            .unwrap()[0]
            .id;
        let patch = serde_json::from_value(json!({"values": {"max_runs": 6}})).unwrap();
        assert_eq!(
            put_settings(State(e.clone()), Ok(Json(patch)))
                .await
                .unwrap()
                .0
                .values["max_runs"],
            6
        );
        let current = e.store.last_id().unwrap();
        let result = restore_settings(
            State(e.clone()),
            Json(SettingsRestore {
                key: SettingsKey::MaxRuns,
                event: invalid,
                current,
            }),
        )
        .await;
        assert!(matches!(result, Err(Fail::Refused(r)) if r.code == RefusalCode::InvalidValue));
        assert_eq!(snapshot(&e).values["max_runs"], 6);
        assert_eq!(e.proj().max_runs(), 6);
        let patch = serde_json::from_value(json!({"values": {"max_runs": 1}})).unwrap();
        assert_eq!(
            put_settings(State(e.clone()), Ok(Json(patch)))
                .await
                .unwrap()
                .0
                .values["max_runs"],
            1
        );
        let latest = e.store.last_id().unwrap();
        let restored = restore_settings(
            State(e.clone()),
            Json(SettingsRestore {
                key: SettingsKey::MaxRuns,
                event: current,
                current: latest,
            }),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(restored.values["max_runs"], 6);
        assert_eq!(e.proj().max_runs(), 6);
        assert!(
            matches!(&e.store.after(latest).unwrap()[0].kind, EventKind::SettingsChanged { restored_from: Some(id), .. } if *id == current)
        );
    }

    #[tokio::test]
    async fn text_size_defaults_validates_and_survives_restart() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::dev(dir.path(), "dvb".into());
        let e = Engine::open(cfg.clone(), Default::default()).unwrap();
        assert_eq!(snapshot(&e).values["text_size"], "L");
        for size in ["S", "M", "L", "XL"] {
            let patch = serde_json::from_value(json!({"values": {"text_size": size}})).unwrap();
            let saved = put_settings(State(e.clone()), Ok(Json(patch)))
                .await
                .unwrap()
                .0;
            assert_eq!(saved.values["text_size"], size);
        }
        let before = snapshot(&e);
        for size in [json!("XS"), json!("l"), json!(1), json!(null), json!(true)] {
            let patch = serde_json::from_value(json!({"values": {"text_size": size}})).unwrap();
            assert!(
                put_settings(State(e.clone()), Ok(Json(patch)))
                    .await
                    .is_err()
            );
            assert_eq!(snapshot(&e), before);
        }
        drop(e);
        assert_eq!(
            snapshot(&Engine::open(cfg, Default::default()).unwrap()).values["text_size"],
            "XL"
        );
    }

    #[tokio::test]
    async fn partial_settings_survive_restart_and_invalid_batches_do_not_partly_write() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::dev(dir.path(), "dvb".into());
        let e = Engine::open(cfg.clone(), Default::default()).unwrap();
        let patch: Settings = serde_json::from_value(
            json!({"values": {"response_style": "Technical", "task_files_right": true}}),
        )
        .unwrap();
        let saved = put_settings(State(e.clone()), Ok(Json(patch.clone())))
            .await
            .unwrap()
            .0;
        assert_eq!(saved.values["response_style"], "Technical");
        let before = e.store.last_id().unwrap();
        let _ = put_settings(State(e.clone()), Ok(Json(patch)))
            .await
            .unwrap();
        assert_eq!(e.store.last_id().unwrap(), before);
        let bad = serde_json::from_value(
            json!({"values": {"response_style": "Expert", "task_files_right": "yes"}}),
        )
        .unwrap();
        assert!(put_settings(State(e.clone()), Ok(Json(bad))).await.is_err());
        assert_eq!(snapshot(&e).values, saved.values);
        drop(e);
        let reopened = Engine::open(cfg, Default::default()).unwrap();
        assert_eq!(get_settings(State(reopened)).await.unwrap().0, saved);
    }

    #[tokio::test]
    async fn accepts_auto_and_rejects_unknown_models_and_modes() {
        let dir = tempfile::tempdir().unwrap();
        let e = Engine::open(Config::dev(dir.path(), "dvb".into()), Default::default()).unwrap();
        for values in [
            json!({"model_pool": ["other"]}),
            json!({"model_mode": "Smart"}),
            json!({"vendor_fallback": "yes"}),
        ] {
            let patch = serde_json::from_value(json!({"values": values})).unwrap();
            assert!(
                put_settings(State(e.clone()), Ok(Json(patch)))
                    .await
                    .is_err()
            );
        }
        let ok = serde_json::from_value(
            json!({"values": {"model_mode": "Auto", "vendor_fallback": false}}),
        )
        .unwrap();
        let Json(saved) = put_settings(State(e.clone()), Ok(Json(ok))).await.unwrap();
        assert_eq!(saved.values[&SettingsKey::VendorFallback], json!(false));
    }

    #[test]
    fn a_stored_custom_palette_change_still_folds() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::dev(dir.path(), "dvb".into());
        let e = Engine::open(cfg.clone(), Default::default()).unwrap();
        e.append_checked(None, dvb_core::Actor::Owner, |_, _| {
            Ok(vec![
                EventKind::SettingsChanged {
                    key: "custom_palette".into(),
                    old: Value::Null,
                    new: json!({"mode": "dark", "accent": "#112233"}),
                    restored_from: None,
                },
                EventKind::SettingsChanged {
                    key: "text_size".into(),
                    old: Value::Null,
                    new: json!("XL"),
                    restored_from: None,
                },
            ])
        })
        .unwrap();
        drop(e);
        let reopened = Engine::open(cfg, Default::default()).unwrap();
        assert_eq!(
            snapshot(&reopened).values[&SettingsKey::TextSize],
            json!("XL")
        );
    }
    #[tokio::test]
    async fn publication_identity_changes_at_once_is_kept_and_defaults_when_neutral_and_empty() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config::dev(dir.path(), "dvb".into());
        std::fs::create_dir_all(&cfg.paths.state).unwrap();
        crate::publish::PublicationSettings {
            owner_name: "Example Owner".into(),
            owner_email: "owner@example.invalid".into(),
            ..Default::default()
        }
        .save(&cfg.paths.state)
        .unwrap();
        let e = Engine::open(cfg.clone(), Default::default()).unwrap();
        let default = PublishingIdentity {
            name: "dvb".into(),
            email: "dvb@dvb.invalid".into(),
            is_default: true,
            as_owner: false,
        };
        assert_eq!(get_publication(State(e.clone())).await.0, default);
        let set = |e: &AppState, name: &str, email: &str, as_owner| {
            set_publication(
                State(e.clone()),
                Json(PublicationChange {
                    name: name.into(),
                    email: email.into(),
                    as_owner,
                }),
            )
        };
        let owned = PublishingIdentity {
            name: "Example Owner".into(),
            email: "owner@example.invalid".into(),
            is_default: false,
            as_owner: true,
        };
        assert_eq!(
            set(&e, " Example Owner ", "owner@example.invalid", true)
                .await
                .unwrap()
                .0,
            owned
        );
        let Err(Fail::Refused(r)) = set(&e, "Example Owner", "owner@example.invalid", false).await
        else {
            panic!("the owner's name is refused without as_owner");
        };
        assert_eq!(
            (r.code, r.detail.as_str()),
            (
                RefusalCode::InvalidValue,
                "The commit author name can't contain your name (Example Owner) unless you \
                 publish as yourself."
            )
        );
        assert_eq!(get_publication(State(e.clone())).await.0, owned);
        drop(e);
        let e = Engine::open(cfg, Default::default()).unwrap();
        assert_eq!(get_publication(State(e.clone())).await.0, owned);
        assert_eq!(set(&e, "", "", false).await.unwrap().0, default);
        assert_eq!(get_publication(State(e.clone())).await.0, default);
    }

    #[tokio::test]
    async fn unknown_keys_are_coded_refusals_and_do_not_change_settings() {
        use tower::ServiceExt;
        let dir = tempfile::tempdir().unwrap();
        let e = Engine::open(Config::dev(dir.path(), "dvb".into()), Default::default()).unwrap();
        let before = snapshot(&e);
        let response = crate::api::router(e.clone())
            .oneshot(
                axum::http::Request::put("/settings")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        r#"{"values":{"response_style":"Expert","unknown":true}}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::CONFLICT);
        let bytes = axum::body::to_bytes(response.into_body(), 16384)
            .await
            .unwrap();
        let refusal: Refusal = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(refusal.code, RefusalCode::InvalidValue);
        assert_eq!(snapshot(&e), before);
    }
}
