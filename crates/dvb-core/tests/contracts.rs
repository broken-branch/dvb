//! The committed contract files match the code.

use std::collections::BTreeSet;

fn repo_file(path: &str) -> String {
    let full = format!("{}/../../{path}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&full).unwrap_or_else(|e| panic!("{full}: {e}"))
}

#[test]
fn events_md_lists_exactly_the_event_kinds() {
    let doc: BTreeSet<String> = repo_file("contracts/events.md")
        .lines()
        .filter_map(|l| l.strip_prefix("| `"))
        .filter_map(|l| l.split('`').next())
        .map(str::to_owned)
        .collect();

    let api: serde_json::Value =
        serde_json::from_str(&repo_file("contracts/openapi.json")).unwrap();
    let kind_schema = &api["components"]["schemas"]["EventKind"];
    let code: BTreeSet<String> = kind_schema["oneOf"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| {
            v["properties"]["kind"]["enum"][0]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();

    let kinds: BTreeSet<String> = dvb_core::EventKind::ALL_KINDS
        .iter()
        .map(|kind| (*kind).to_owned())
        .collect();
    assert_eq!(kinds.len(), dvb_core::EventKind::ALL_KINDS.len());
    assert_eq!(doc, kinds);
    assert_eq!(kinds, code);
}

#[test]
fn provider_schema_and_client_wording_are_generated_from_core() {
    assert_eq!(
        repo_file("prompts/verdict.schema.json"),
        dvb_core::run_verbs::verdict_schema_json().unwrap()
    );
    for file in ["contracts/wording.json", "ui/src/api/wording.json"] {
        assert_eq!(repo_file(file), dvb_core::wording::SOURCE);
    }
    let words: serde_json::Value = serde_json::from_str(dvb_core::wording::SOURCE).unwrap();
    let api =
        serde_json::from_str::<serde_json::Value>(&repo_file("contracts/openapi.json")).unwrap();
    for variant in api["components"]["schemas"]["TaskState"]["oneOf"]
        .as_array()
        .unwrap()
    {
        let name = variant["properties"]["state"]["enum"][0].as_str().unwrap();
        let state: dvb_core::TaskState = serde_json::from_value(serde_json::json!({
            "state":name, "reason":"test reason", "effect":"push", "attempt":1,
            "was":{"state":"planned"}, "last":{"state":"planned"}
        }))
        .unwrap();
        assert!(
            words.get(format!("state.{name}")).is_some(),
            "{name} lacks wording"
        );
        assert!(!dvb_core::wording::state_word(&state).is_empty());
    }
    for name in api["components"]["schemas"]["GoalState"]["enum"]
        .as_array()
        .unwrap()
    {
        let state: dvb_core::api::GoalState = serde_json::from_value(name.clone()).unwrap();
        assert!(!dvb_core::wording::goal_word(state).is_empty());
    }
    let fixtures: Vec<dvb_core::EventKind> =
        serde_json::from_str(include_str!("fixtures/event-kinds.json")).unwrap();
    for event in fixtures {
        assert!(
            words.get(event.kind()).is_some(),
            "{} lacks wording",
            event.kind()
        );
        let rendered = dvb_core::wording::event_text(&event, |t| t.0.to_string());
        assert_eq!(
            rendered.is_none(),
            words[event.kind()].is_null(),
            "{} visibility",
            event.kind()
        );
        if let Some(text) = rendered {
            assert!(!text.contains('{'), "{}: {text}", event.kind());
        }
    }
}

#[test]
fn goal_dependencies_round_trip_without_changing_historical_payloads() {
    let historical = serde_json::json!({"kind":"goal.stated", "payload":{"goal":"g", "text":"Old goal", "from_issue":null}});
    let old: dvb_core::EventKind = serde_json::from_value(historical.clone()).unwrap();
    assert_eq!(serde_json::to_value(old).unwrap(), historical);
    let ordered = serde_json::json!({"kind":"goal.stated", "payload":{"goal":"next", "text":"Next goal", "from_issue":null, "after":"g"}});
    let next: dvb_core::EventKind = serde_json::from_value(ordered.clone()).unwrap();
    assert_eq!(serde_json::to_value(next).unwrap(), ordered);
    let cleared =
        serde_json::json!({"kind":"goal.after_set", "payload":{"goal":"next", "after":null}});
    let kind: dvb_core::EventKind = serde_json::from_value(cleared.clone()).unwrap();
    assert_eq!(serde_json::to_value(&kind).unwrap(), cleared);
    assert_eq!(
        dvb_core::wording::event_text(&kind, |t| t.0.to_string()).unwrap(),
        "Goal next no longer follows another goal"
    );
}

#[test]
fn goal_issues_are_add_only_and_old_goal_payloads_stay_readable() {
    let historical = serde_json::json!({"kind":"goal.stated", "payload":{"goal":"g", "text":"Old goal", "from_issue":4}});
    let old: dvb_core::EventKind = serde_json::from_value(historical.clone()).unwrap();
    assert!(
        matches!(&old, dvb_core::EventKind::GoalStated { issues, from_issue: Some(4), .. } if issues.is_empty())
    );
    assert_eq!(serde_json::to_value(old).unwrap(), historical);
    let carried = serde_json::json!({"kind":"goal.stated", "payload":{"goal":"g", "text":"New goal", "from_issue":null,
        "issues":[{"number":7, "url":"https://github.com/org/repo/issues/7", "text":"Fenced"}]}});
    let new: dvb_core::EventKind = serde_json::from_value(carried.clone()).unwrap();
    assert_eq!(serde_json::to_value(new).unwrap(), carried);
    let old_api: dvb_core::api::NewGoal =
        serde_json::from_value(serde_json::json!({"text":"Goal", "from_issue":null})).unwrap();
    assert!(old_api.issues.is_empty());
}
