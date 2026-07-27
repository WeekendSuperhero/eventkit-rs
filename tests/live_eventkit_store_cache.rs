//! Live checks on the THREAD-LOCAL `EKEventStore` cache.
//!
//! These exist because caching the store is the half of the authorization work
//! that can regress silently. Per-call construction used to guarantee every
//! request saw current state; now that a store is reused for the life of a
//! thread, two questions need real answers rather than reasoning:
//!
//! 1. Does a cached store still observe changes made by ANOTHER process?
//!    (A long-lived MCP server must see a reminder the user just added in
//!    Reminders.app. If it doesn't, the cache introduced staleness and needs
//!    an `EKEventStoreChanged` observer or a periodic `reset()`.)
//! 2. Does the cache actually hand out ONE store per thread?
//!
//! `#[ignore]`d: they touch the real Reminders database and need Full Access.
//! Run explicitly:
//!
//! ```sh
//! cargo nextest run --run-ignored only -E 'test(live_eventkit_store)'
//! ```
#![cfg(target_os = "macos")]

use eventkit::{AuthorizationStatus, RemindersManager};

/// Skip rather than fail when the host lacks Full Access — CI and a dev
/// machine that hasn't granted Reminders should not report a red suite.
fn require_full_access() -> bool {
    if RemindersManager::authorization_status() == AuthorizationStatus::FullAccess {
        return true;
    }
    eprintln!("SKIP: Reminders Full Access not granted on this host");
    false
}

/// A cached store MUST observe an external write.
///
/// The mutation runs in a SEPARATE PROCESS (a second `eventkit` invocation),
/// which is the situation that matters: the user editing in Reminders.app
/// while our long-lived server holds a store. An in-process write would prove
/// nothing, since it would go through the very same store.
///
/// If this fails, the thread-local cache is serving stale reads and the
/// `EKEventStoreChanged` observer stops being optional.
#[test]
#[ignore]
fn live_eventkit_store_cache_sees_external_writes() {
    if !require_full_access() {
        return;
    }

    // Warm this thread's cached store and take a baseline.
    let manager = RemindersManager::new();
    let before = manager
        .fetch_all_reminders()
        .expect("baseline fetch must succeed");
    let baseline_titles: Vec<String> = before.iter().map(|r| r.title.clone()).collect();

    // Mutate from ANOTHER process, so the change cannot travel through our
    // store object.
    let marker = format!("eventkit-cache-probe-{}", std::process::id());
    let exe = env!("CARGO_BIN_EXE_eventkit");
    let created = std::process::Command::new(exe)
        .args(["reminders", "add", &marker])
        .output()
        .expect("spawning the external mutator must succeed");
    assert!(
        created.status.success(),
        "external create failed: {}",
        String::from_utf8_lossy(&created.stderr)
    );

    // Re-fetch through the SAME cached store.
    let after = manager
        .fetch_all_reminders()
        .expect("second fetch must succeed");
    let found = after.iter().any(|r| r.title == marker);

    // Clean up before asserting, so a failure doesn't leave litter behind.
    if let Some(item) = after.iter().find(|r| r.title == marker) {
        let _ = manager.delete_reminder(&item.identifier);
    }

    assert!(
        found,
        "a cached EKEventStore did NOT observe a reminder created by another \
         process. Baseline had {} reminders {:?}...; the cache is serving stale \
         reads and needs an EKEventStoreChanged observer or a reset()",
        baseline_titles.len(),
        baseline_titles.iter().take(3).collect::<Vec<_>>()
    );
}

/// Repeated `RemindersManager::new()` must retain ONE store per thread — the
/// point of the cache. Pointer identity, not timing, so it's deterministic.
#[test]
#[ignore]
fn live_eventkit_store_cache_reuses_one_store_per_thread() {
    if !require_full_access() {
        return;
    }
    let a = RemindersManager::new();
    let b = RemindersManager::new();
    assert!(
        a.shares_store_with(&b),
        "repeated construction must reuse the thread's cached EKEventStore"
    );
}
