//! Usage: tokens, cost, vendor windows, and a paused vendor's usage check.

use crate::AppState;
use crate::engine::Fail;
use axum::Json;
use axum::extract::{Path, Query, State};
use dvb_core::api::{Usage, UsageQuery, VendorWindow};
use dvb_core::launcher::{UnitId, UnitState};
use dvb_core::run_verbs::{RateLimitBody, UsageReading};
use dvb_core::{EventId, Refusal, RefusalCode, RunId, Vendor};
use std::time::Duration;

/// The Fake vendor's reading unless a dev test puts its own at `fake/usage.json`.
const USAGE_SCRIPT: &str = include_str!("../fake_scripts/usage.json");
/// How long a usage check waits for its reading: the unit's start and the Codex
/// app-server's own 30-second startup allowance.
const READING_WAIT: Duration = Duration::from_secs(45);

#[utoipa::path(
    get,
    path = "/usage",
    operation_id = "get_usage",
    tag = "usage",
    params(dvb_core::api::UsageQuery),
    responses(crate::api::ErrorResponses, (status = 200, description = "Tokens, estimated cost and vendor windows", body = dvb_core::api::Usage))
)]
pub async fn get_usage(
    State(e): State<AppState>,
    Query(q): Query<UsageQuery>,
) -> Result<Json<Usage>, Fail> {
    let mut sums = crate::usage::Sums::new(&q);
    e.store.each(
        EventId(0),
        dvb_store::Select::Kinds(&["run.started", "run.rerouted", "run.usage"]),
        |ev| sums.add(&ev),
    )?;
    Ok(Json(Usage {
        rows: sums.rows(),
        windows: e.vendors.windows(),
    }))
}

/// `POST /usage/{vendor}/check`: one fresh reading of a paused vendor's window
/// (`vendors::check`), for when the account reset early or was topped up.
#[utoipa::path(
    post,
    path = "/usage/{vendor}/check",
    operation_id = "check_usage",
    tag = "usage",
    params(("vendor" = dvb_core::Vendor, Path)),
    responses(crate::api::ErrorResponses, (status = 200, description = "The vendor's window from the fresh reading: not `limited` when the pause ended, else the reset time the pause now holds until", body = dvb_core::api::VendorWindow))
)]
pub async fn check_usage(
    State(e): State<AppState>,
    Path(vendor): Path<Vendor>,
) -> Result<Json<VendorWindow>, Fail> {
    let _check = e.usage_checks.lock(vendor.as_str()).await;
    if !e.vendors.is_limited(vendor) {
        return Err(Refusal::new(
            RefusalCode::VendorNotLimited,
            format!("{} is not paused at a usage limit.", vendor.as_str()),
        )
        .into());
    }
    match vendor {
        Vendor::Claude => {
            return Err(Refusal::new(
                RefusalCode::NoUsageReading,
                "Claude's usage can only be read with a model reply, which spends usage, so the pause lasts until its reset time.",
            )
            .into());
        }
        Vendor::Codex | Vendor::Fake => {}
    }
    let reading = take_reading(&e, vendor).await?;
    crate::vendors::check(&e, vendor, &reading).map(Json)
}

/// Starts a reading run (`dvb_run::reading`) in the vendor's unit and waits for
/// the `reading.json` it leaves. Like a login run, it is not recorded as a run.
async fn take_reading(e: &AppState, vendor: Vendor) -> Result<RateLimitBody, Fail> {
    // The unit may be accepted before its start acknowledgement arrives.
    let mut unit = ReadingUnit {
        e: e.clone(),
        unit: UnitId {
            vendor,
            run: RunId::new(e.new_id()),
        },
        stopping: false,
    };
    let dir = super::logins::start_unrecorded(
        e,
        vendor,
        &unit.unit.run,
        "DVB_USAGE_READING",
        "usage",
        |dir| {
            if vendor != Vendor::Fake {
                return Ok(Vec::new());
            }
            let script = dir.join("fake-script.json");
            crate::launch::write_run_file(
                &script,
                crate::launch::fake_script(e, "usage", USAGE_SCRIPT),
            )
            .map_err(|err| Fail::io("fake script", err))?;
            Ok(vec![(
                "DVB_FAKE_SCRIPT",
                script.to_string_lossy().into_owned(),
            )])
        },
    )
    .await?;
    let text = super::logins::poll(&dir.join("reading.json"), READING_WAIT).await;
    unit.stop()
        .await
        .map_err(|err| Fail::context("stopping the usage reading", err))??;
    let Some(text) = text else {
        return Err(Fail::internal(format!(
            "{} gave no usage reading within {} seconds",
            vendor.as_str(),
            READING_WAIT.as_secs()
        )));
    };
    match serde_json::from_str(&text).map_err(|err| Fail::context("reading.json", err))? {
        UsageReading::Read(window) => Ok(window),
        UsageReading::Failed(why) => {
            Err(Fail::internal(format!("the usage reading failed: {why}")))
        }
    }
}

struct ReadingUnit {
    e: AppState,
    unit: UnitId,
    stopping: bool,
}

impl ReadingUnit {
    fn stop(&mut self) -> tokio::task::JoinHandle<Result<(), Fail>> {
        self.stopping = true;
        let e = self.e.clone();
        let unit = self.unit.clone();
        // Keep cleanup alive if the request is dropped while awaiting the stop.
        tokio::spawn(async move {
            match e.launcher().state(&unit).await {
                Ok(UnitState::Inactive | UnitState::Failed) => return Ok(()),
                Ok(UnitState::Active) => {}
                Err(err) => {
                    eprintln!("dvb engine: checking usage reading {}: {err}", unit.run);
                }
            }
            e.launcher().stop(&unit).await.map_err(|err| {
                let err = Fail::context("stopping the usage reading", err);
                eprintln!("dvb engine: {}: {err}", unit.run);
                err
            })
        })
    }
}

impl Drop for ReadingUnit {
    fn drop(&mut self) {
        if !self.stopping {
            drop(self.stop());
        }
    }
}
