//! The vendors' usage windows and the pause at a limit.
//!
//! A run's driver reports the vendor's window (`/run/ratelimit`). A window at 100
//! percent is a limit: `vendor.limited` is recorded, new runs of that vendor wait
//! in `launch`, and `vendor.resumed` is recorded at the reset time the vendor
//! gave, or after `RETRY_EVERY` when it gave none. A pause in the log outlives an
//! engine restart (`restore`). The owner can have a paused vendor read again
//! (`check`, a usage check): the fresh reading ends the pause or sets its end.
//! A paused run waits for its own vendor unless it moves at launch
//! (`dispatch::decide_launch`, waited for in `dispatch::wait_launch`): under the
//! timed model switch it waits only for its switched model's vendor; otherwise,
//! with `vendor_fallback` on, a task run of a limited vendor moves to the
//! orchestrator's vendor.

use crate::engine::{AppState, Fail};
use crate::spawn;
use dvb_core::api::VendorWindow;
use dvb_core::run_verbs::RateLimitBody;
use dvb_core::{Actor, EventId, EventKind, Timestamp, Vendor};
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::watch;

/// A window at or above this is a limit.
pub const LIMIT_PCT: f64 = 100.0;
/// When the vendor gives no reset time: look again after this long.
pub const RETRY_EVERY: Duration = Duration::from_secs(30 * 60);

/// A vendor's pause: when it ends (if the vendor said) and which timer owns the
/// end of it. A newer limiting reading replaces the timer, so an old one cannot
/// reopen the vendor.
#[derive(Debug, Default)]
struct Pauses {
    next: u64,
    by_vendor: HashMap<Vendor, (Option<Timestamp>, u64)>,
}

pub struct Vendors {
    windows: Mutex<HashMap<Vendor, VendorWindow>>,
    pauses: Mutex<Pauses>,
    limited: watch::Sender<HashSet<Vendor>>,
}

impl Default for Vendors {
    fn default() -> Self {
        Self {
            windows: Mutex::new(HashMap::new()),
            pauses: Mutex::new(Pauses::default()),
            limited: watch::channel(HashSet::new()).0,
        }
    }
}

impl Vendors {
    fn windows_lock(&self) -> std::sync::MutexGuard<'_, HashMap<Vendor, VendorWindow>> {
        self.windows.lock().unwrap_or_else(dvb_core::sync::recover)
    }

    /// The last window each vendor reported.
    pub fn windows(&self) -> Vec<VendorWindow> {
        let mut all: Vec<VendorWindow> = self.windows_lock().values().cloned().collect();
        all.sort_by_key(|w| w.vendor.as_str());
        all
    }

    pub fn is_limited(&self, v: Vendor) -> bool {
        self.limited.borrow().contains(&v)
    }

    /// When the pause on `v` ends, if the vendor said.
    pub fn resets_at(&self, v: Vendor) -> Option<Timestamp> {
        let p = self.pauses.lock().unwrap_or_else(dvb_core::sync::recover);
        p.by_vendor.get(&v).and_then(|(end, _)| *end)
    }

    /// A receiver that wakes when any vendor's limit starts or ends.
    pub fn subscribe(&self) -> watch::Receiver<HashSet<Vendor>> {
        self.limited.subscribe()
    }

    /// Waits while `v` is at its limit.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn wait_open(&self, v: Vendor) {
        let mut rx = self.limited.subscribe();
        // The sender lives as long as `self`, so this only returns on `Ok`.
        let _ = rx.wait_for(|set| !set.contains(&v)).await;
    }

    /// Limits `v` until `resets_at`. Returns the pause's effective end and the
    /// timer generation that owns it when this is new or moves the end later (the
    /// caller records it and starts that timer); `None` when nothing changed.
    fn limit(&self, v: Vendor, resets_at: Option<Timestamp>) -> Option<(Option<Timestamp>, u64)> {
        let mut p = self.pauses.lock().unwrap_or_else(dvb_core::sync::recover);
        let end = match p.by_vendor.get(&v) {
            Some((old, _)) => {
                let merged = (*old).max(resets_at);
                if merged == *old {
                    return None;
                }
                merged
            }
            None => resets_at,
        };
        let gen_ = self.hold(&mut p, v, end);
        drop(p);
        Some((end, gen_))
    }

    /// Limits `v` until `resets_at`, earlier or later than before; without one,
    /// until the end it had. Returns the end and the timer generation that owns it.
    fn relimit(&self, v: Vendor, resets_at: Option<Timestamp>) -> (Option<Timestamp>, u64) {
        let mut p = self.pauses.lock().unwrap_or_else(dvb_core::sync::recover);
        let end = resets_at.or_else(|| p.by_vendor.get(&v).and_then(|(end, _)| *end));
        let gen_ = self.hold(&mut p, v, end);
        drop(p);
        (end, gen_)
    }

    fn hold(&self, p: &mut Pauses, v: Vendor, end: Option<Timestamp>) -> u64 {
        p.next += 1;
        let gen_ = p.next;
        p.by_vendor.insert(v, (end, gen_));
        self.limited.send_if_modified(|set| set.insert(v));
        gen_
    }

    /// Ends the pause if timer `gen_` still owns it.
    fn resume(&self, v: Vendor, gen_: u64) -> bool {
        let mut p = self.pauses.lock().unwrap_or_else(dvb_core::sync::recover);
        if p.by_vendor.get(&v).is_none_or(|(_, g)| *g != gen_) {
            return false;
        }
        let lifted = self.lift(&mut p, v);
        drop(p);
        lifted
    }

    /// Ends the pause, whichever timer owns it; false when there was none.
    fn end(&self, v: Vendor) -> bool {
        let mut p = self.pauses.lock().unwrap_or_else(dvb_core::sync::recover);
        let lifted = self.lift(&mut p, v);
        drop(p);
        lifted
    }

    fn lift(&self, p: &mut Pauses, v: Vendor) -> bool {
        if p.by_vendor.remove(&v).is_none() {
            return false;
        }
        self.limited.send_if_modified(|set| set.remove(&v));
        if let Some(w) = self.windows_lock().get_mut(&v) {
            w.limited = false;
        }
        true
    }

    fn set_window(&self, w: VendorWindow) {
        self.windows_lock().insert(w.vendor, w);
    }
}

/// How long until `resets_at`, at least a second; `RETRY_EVERY` without one.
pub fn pause_for(resets_at: Option<Timestamp>, now: Timestamp) -> Duration {
    match resets_at {
        Some(t) => {
            let ms = u64::try_from(t.0.saturating_sub(now.0)).unwrap_or(0);
            Duration::from_millis(ms).max(Duration::from_secs(1))
        }
        None => RETRY_EVERY,
    }
}

/// A window reading from a run's driver. At the limit, pauses the vendor.
pub fn record(e: &AppState, vendor: Vendor, b: &RateLimitBody) -> Result<(), Fail> {
    let now = e.clock.now();
    // A reading whose reset time has passed is stale, not a limit.
    let stale = b.resets_at.is_some_and(|t| t <= now);
    let limited = b.used_pct >= LIMIT_PCT && !stale;
    e.vendors.set_window(VendorWindow {
        vendor,
        used_pct: b.used_pct,
        window: b.window.clone(),
        resets_at: b.resets_at,
        limited: limited || e.vendors.is_limited(vendor),
    });
    if limited && let Some((end, gen_)) = e.vendors.limit(vendor, b.resets_at) {
        e.append_checked(None, Actor::Engine, |_, _| {
            Ok(vec![EventKind::VendorLimited {
                vendor,
                resets_at: end,
            }])
        })?;
        spawn("resume vendor", resume_when(e.clone(), vendor, end, gen_));
    }
    Ok(())
}

/// A fresh reading of a paused vendor's window, taken for a usage check. It is
/// the vendor's word now, so it replaces the pause rather than only moving it
/// later: below the limit the pause ends with `vendor.resumed` and the vendor's
/// waiting runs launch; at the limit it holds until the reading's reset time (or
/// the end it had, when the reading gave none), recorded as `vendor.limited` even
/// when that end is unchanged, so the check shows in the history. Returns the
/// window as it now stands.
pub fn check(e: &AppState, vendor: Vendor, b: &RateLimitBody) -> Result<VendorWindow, Fail> {
    let now = e.clock.now();
    let stale = b.resets_at.is_some_and(|t| t <= now);
    let limited = b.used_pct >= LIMIT_PCT && !stale;
    let mut window = VendorWindow {
        vendor,
        used_pct: b.used_pct,
        window: b.window.clone(),
        resets_at: b.resets_at,
        limited,
    };
    if limited {
        let (end, gen_) = e.vendors.relimit(vendor, b.resets_at);
        window.resets_at = end;
        e.vendors.set_window(window.clone());
        e.append_checked(None, Actor::Engine, |_, _| {
            Ok(vec![EventKind::VendorLimited {
                vendor,
                resets_at: end,
            }])
        })?;
        spawn("resume vendor", resume_when(e.clone(), vendor, end, gen_));
    } else {
        e.vendors.set_window(window.clone());
        if e.vendors.end(vendor) {
            e.append_checked(None, Actor::Engine, |_, _| {
                Ok(vec![EventKind::VendorResumed { vendor }])
            })?;
        }
    }
    Ok(window)
}

async fn resume_when(
    e: AppState,
    vendor: Vendor,
    resets_at: Option<Timestamp>,
    gen_: u64,
) -> Result<(), Fail> {
    tokio::time::sleep(pause_for(resets_at, e.clock.now())).await;
    if e.vendors.resume(vendor, gen_) {
        e.append_checked(None, Actor::Engine, |_, _| {
            Ok(vec![EventKind::VendorResumed { vendor }])
        })?;
    }
    Ok(())
}

/// At engine start: a vendor the log says is still limited stays paused until its
/// reset time, and shows in the windows as a full one. The log keeps no window
/// name, so the restored window is named `usage`.
pub fn restore(e: &AppState) -> Result<(), Fail> {
    let mut last: HashMap<Vendor, Option<Timestamp>> = HashMap::new();
    for ev in e.store.select(
        EventId(0),
        dvb_store::Select::Kinds(&["vendor.limited", "vendor.resumed"]),
    )? {
        if let EventKind::VendorLimited { vendor, resets_at } = ev.kind {
            last.insert(vendor, resets_at);
        } else if let EventKind::VendorResumed { vendor } = ev.kind {
            last.remove(&vendor);
        }
    }
    for (vendor, resets_at) in last {
        // The last `vendor.limited` is the effective end: it is only written when
        // the end moves.
        if let Some((end, gen_)) = e.vendors.limit(vendor, resets_at) {
            e.vendors.set_window(VendorWindow {
                vendor,
                used_pct: LIMIT_PCT,
                window: "usage".into(),
                resets_at: end,
                limited: true,
            });
            spawn("resume vendor", resume_when(e.clone(), vendor, end, gen_));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pause_lasts_until_the_reset_or_the_schedule() {
        let now = Timestamp(1_000_000);
        assert_eq!(
            pause_for(Some(Timestamp(1_090_000)), now),
            Duration::from_secs(90)
        );
        assert_eq!(
            pause_for(Some(Timestamp(1_000_100)), now),
            Duration::from_secs(1)
        );
        assert_eq!(pause_for(Some(Timestamp(5)), now), Duration::from_secs(1));
        assert_eq!(pause_for(None, now), RETRY_EVERY);
    }

    #[test]
    fn a_limit_changes_only_when_it_is_new_or_moves_later() {
        let v = Vendors::default();
        let t = |ms| Some(Timestamp(ms));
        let (end, g1) = v.limit(Vendor::Claude, t(1000)).unwrap();
        assert_eq!(end, t(1000));
        assert!(v.is_limited(Vendor::Claude) && !v.is_limited(Vendor::Codex));
        // Same, earlier or unknown: nothing to record.
        assert!(v.limit(Vendor::Claude, t(1000)).is_none());
        assert!(v.limit(Vendor::Claude, t(500)).is_none());
        assert!(v.limit(Vendor::Claude, None).is_none());
        // Later: a new owner of the pause.
        let (end, g2) = v.limit(Vendor::Claude, t(9000)).unwrap();
        assert_eq!(end, t(9000));
        assert_ne!(g1, g2);
        assert!(
            !v.resume(Vendor::Claude, g1),
            "the old timer cannot reopen it"
        );
        assert!(v.is_limited(Vendor::Claude));
        assert!(v.resume(Vendor::Claude, g2));
        assert!(!v.resume(Vendor::Claude, g2));
    }

    #[test]
    fn a_checked_limit_takes_the_readings_end_and_keeps_the_old_one_without() {
        let v = Vendors::default();
        let t = |ms| Some(Timestamp(ms));
        let (_, g1) = v.limit(Vendor::Codex, t(9000)).unwrap();
        let (end, g2) = v.relimit(Vendor::Codex, t(1000));
        assert_eq!(end, t(1000));
        assert_eq!(v.resets_at(Vendor::Codex), t(1000));
        assert!(
            !v.resume(Vendor::Codex, g1),
            "the old timer cannot reopen it"
        );
        let (end, g3) = v.relimit(Vendor::Codex, None);
        assert_eq!(end, t(1000));
        assert!(!v.resume(Vendor::Codex, g2));
        assert!(v.end(Vendor::Codex) && !v.is_limited(Vendor::Codex));
        assert!(!v.end(Vendor::Codex) && !v.resume(Vendor::Codex, g3));
    }
}
