//! A vendor at its limit pauses its new runs, says why, and resumes at the reset
//! time the vendor gave; the other vendor is untouched.

mod support;
use dvb_core::run_verbs::RateLimitBody;
use dvb_core::{EventKind, Timestamp, Vendor};
use dvb_engine::{AppState, vendors};
use std::time::Duration;
use tokio::time::timeout;

use support::engine::engine;

fn kinds(e: &AppState) -> Vec<EventKind> {
    support::events::all(e)
        .into_iter()
        .map(|ev| ev.kind)
        .collect()
}

fn body(used_pct: f64, resets_at: Option<Timestamp>) -> RateLimitBody {
    RateLimitBody {
        used_pct,
        window: "5h".into(),
        resets_at,
    }
}

#[tokio::test]
async fn a_limit_pauses_the_vendor_until_its_reset() {
    let (_dir, e) = engine();
    let reset = Timestamp(e.clock.now().0 + 1500);
    vendors::record(&e, Vendor::Claude, &body(100.0, Some(reset))).unwrap();

    assert!(e.vendors.is_limited(Vendor::Claude));
    assert!(kinds(&e).iter().any(|k| matches!(
        k,
        EventKind::VendorLimited { vendor: Vendor::Claude, resets_at } if *resets_at == Some(reset)
    )));
    // The other vendor is not held, and nothing is switched to it.
    timeout(
        Duration::from_millis(100),
        e.vendors.wait_open(Vendor::Codex),
    )
    .await
    .unwrap();
    // The limited vendor waits...
    assert!(
        timeout(
            Duration::from_millis(200),
            e.vendors.wait_open(Vendor::Claude)
        )
        .await
        .is_err()
    );
    // ...and is let through after the reset.
    timeout(Duration::from_secs(5), e.vendors.wait_open(Vendor::Claude))
        .await
        .unwrap();
    assert!(!e.vendors.is_limited(Vendor::Claude));
    assert!(kinds(&e).iter().any(|k| matches!(
        k,
        EventKind::VendorResumed {
            vendor: Vendor::Claude
        }
    )));
    assert!(!e.vendors.windows()[0].limited);
}

#[tokio::test]
async fn a_second_reading_at_the_limit_records_one_pause() {
    let (_dir, e) = engine();
    let reset = Timestamp(e.clock.now().0 + 60_000);
    for _ in 0..3 {
        vendors::record(&e, Vendor::Codex, &body(100.0, Some(reset))).unwrap();
    }
    let n = kinds(&e)
        .iter()
        .filter(|k| matches!(k, EventKind::VendorLimited { .. }))
        .count();
    assert_eq!(n, 1);
}

#[tokio::test]
async fn a_reading_below_the_limit_or_already_reset_pauses_nothing() {
    let (_dir, e) = engine();
    vendors::record(&e, Vendor::Claude, &body(67.0, None)).unwrap();
    let past = Timestamp(e.clock.now().0 - 10_000);
    vendors::record(&e, Vendor::Claude, &body(100.0, Some(past))).unwrap();
    assert!(!e.vendors.is_limited(Vendor::Claude));
    assert!(
        !kinds(&e)
            .iter()
            .any(|k| matches!(k, EventKind::VendorLimited { .. }))
    );
    let w = e.vendors.windows();
    assert_eq!(w.len(), 1);
    assert!((w[0].used_pct - 100.0).abs() < f64::EPSILON && !w[0].limited);
}

#[tokio::test]
async fn a_limit_in_the_log_is_still_in_force_after_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let reset = {
        let e = support::engine::open(dir.path());
        let reset = Timestamp(e.clock.now().0 + 1200);
        vendors::record(&e, Vendor::Codex, &body(100.0, Some(reset))).unwrap();
        reset
    };
    let e = support::engine::open(dir.path());
    assert!(!e.vendors.is_limited(Vendor::Codex));
    vendors::restore(&e).unwrap();
    assert!(e.vendors.is_limited(Vendor::Codex));
    timeout(Duration::from_secs(5), e.vendors.wait_open(Vendor::Codex))
        .await
        .unwrap();
    assert!(e.clock.now() >= reset);
}

#[tokio::test]
async fn a_later_limiting_reading_moves_the_deadline_and_the_old_timer_cannot_reopen_the_vendor() {
    let (_dir, e) = engine();
    let now = e.clock.now().0;
    // A 5-hour window first (shortly), then a 7-day one that is still exhausted.
    vendors::record(&e, Vendor::Claude, &body(100.0, Some(Timestamp(now + 400)))).unwrap();
    let later = Timestamp(now + 2500);
    vendors::record(&e, Vendor::Claude, &body(100.0, Some(later))).unwrap();
    // An earlier reading afterwards changes nothing.
    vendors::record(&e, Vendor::Claude, &body(100.0, Some(Timestamp(now + 100)))).unwrap();

    let limited: Vec<Option<Timestamp>> = kinds(&e)
        .into_iter()
        .filter_map(|k| {
            if let EventKind::VendorLimited { resets_at, .. } = k {
                Some(resets_at)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(limited, vec![Some(Timestamp(now + 400)), Some(later)]);

    // Past the first deadline the vendor is still held.
    assert!(
        timeout(
            Duration::from_millis(1000),
            e.vendors.wait_open(Vendor::Claude)
        )
        .await
        .is_err()
    );
    assert!(e.vendors.is_limited(Vendor::Claude));
    timeout(Duration::from_secs(6), e.vendors.wait_open(Vendor::Claude))
        .await
        .unwrap();
    assert!(e.clock.now() >= later);
    let resumed = kinds(&e)
        .iter()
        .filter(|k| matches!(k, EventKind::VendorResumed { .. }))
        .count();
    assert_eq!(resumed, 1);
}

#[tokio::test]
async fn a_restart_restores_the_latest_deadline_not_the_first() {
    let dir = tempfile::tempdir().unwrap();
    let later = {
        let e = support::engine::open(dir.path());
        let now = e.clock.now().0;
        vendors::record(&e, Vendor::Codex, &body(100.0, Some(Timestamp(now + 300)))).unwrap();
        let later = Timestamp(now + 2500);
        vendors::record(&e, Vendor::Codex, &body(100.0, Some(later))).unwrap();
        later
    };
    let e = support::engine::open(dir.path());
    vendors::restore(&e).unwrap();
    assert!(
        timeout(
            Duration::from_millis(1000),
            e.vendors.wait_open(Vendor::Codex)
        )
        .await
        .is_err(),
        "the first, obsolete deadline has passed"
    );
    timeout(Duration::from_secs(6), e.vendors.wait_open(Vendor::Codex))
        .await
        .unwrap();
    assert!(e.clock.now() >= later);
}
