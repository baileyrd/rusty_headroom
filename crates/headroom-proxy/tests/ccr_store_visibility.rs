//! A CCR store that failed to open must be visible, not just logged.
//!
//! # The failure this pins
//!
//! `Config::ccr_store` falls back to memory on any error and warns. That trade is right —
//! CCR is a recovery path, and refusing to start costs the customer their whole service
//! rather than one retrievable block. What was missing is the *report*: the proxy went on
//! relaying, compressing, and handing the model `<<ccr:...>>` markers, while `/health`
//! answered `"ok"` and named no store at all.
//!
//! Measured before the fix: put a value, drop the store, rebuild it from the same
//! configuration, and it is gone. A marker written in that state is unredeemable the
//! moment the process restarts — and CCR's whole promise, in README.md, is that
//! compression is "a bet that can be unwound".
//!
//! # Why its own test binary
//!
//! `set_overrides` writes process-global configuration. Flipping it in-process would make
//! every parallel test's view of the store depend on scheduling — the same reason
//! `tests/stabilization.rs` exists.

use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use headroom_core::ccr::ContentHash;
use headroom_proxy::config::{self, CcrStoreKind, Config};
use headroom_proxy::health::Health;
use headroom_proxy::server::AppState;

/// Serializes the three cases below.
///
/// `set_overrides` writes one process-global map, so these race by construction: run
/// under the default thread count without this, and two of three fail about half the
/// time. Measured, five runs: 2, 1, 1, 1, 2 failures. A `--test-threads=1` in CI would
/// fix it for whoever remembered to pass it.
static CONFIG: Mutex<()> = Mutex::new(());

/// Takes the lock, ignoring poisoning — a panic in one case is that case's failure to
/// report, and swallowing the other two behind it would hide their result.
fn exclusive() -> MutexGuard<'static, ()> {
    CONFIG
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Points the CCR store at `dir` and returns what was actually built.
fn store_for(dir: &str) -> CcrStoreKind {
    config::set_overrides(
        [(config::vars::CCR_DIR.to_owned(), dir.to_owned())]
            .into_iter()
            .collect(),
    );
    Config::ccr_store_with_kind().1
}

/// Whether a value written to the configured store is still there after a rebuild.
fn survives_a_rebuild() -> bool {
    let hash = ContentHash::of(b"probe payload");
    Config::ccr_store()
        .put(hash, b"probe payload", Duration::from_secs(600))
        .expect("the store rejected a write");
    Config::ccr_store()
        .get(hash)
        .expect("the store failed to read")
        .is_some()
}

#[test]
fn a_ccr_directory_that_cannot_be_opened_is_reported_rather_than_only_logged() {
    let _guard = exclusive();
    // A path that is not a directory and cannot become one.
    let unusable = store_for("/proc/self/mem/not-a-directory");

    // The behaviour first, so what follows is about a real fallback rather than about a
    // label. Without this the assertions below would pass against a store that worked.
    assert!(
        !survives_a_rebuild(),
        "the bad path did not actually fall back, so this test proves nothing"
    );

    assert_eq!(unusable, CcrStoreKind::Memory);
    assert!(!unusable.survives_restart());

    let report = Health::current(&Config::from_env(), Some("http://x"), unusable);
    assert_eq!(report.ccr_store, "memory");
    assert!(
        !report.ccr_store_persistent,
        "a store that cannot outlive the process reported itself as persistent"
    );
    // The operator asked for one and did not get it — the distinction the boolean exists
    // to make, since plain `"memory"` is also the correct answer for a default install.
    assert!(
        Config::persistent_store_requested(),
        "the request itself went unrecorded, so nothing can flag the mismatch"
    );

    config::clear_overrides();
}

#[test]
fn a_usable_directory_is_reported_as_persistent() {
    let _guard = exclusive();
    // The control. Without it, a `ccr_store_persistent` hardwired to `false` would
    // satisfy every assertion above.
    let dir = std::env::temp_dir().join("headroom-ccr-visibility-test");
    std::fs::create_dir_all(&dir).expect("could not create the test directory");

    let kind = store_for(&dir.display().to_string());

    assert!(
        survives_a_rebuild(),
        "the good path did not persist, so the negative case proves nothing"
    );
    assert_eq!(kind, CcrStoreKind::File);
    assert!(kind.survives_restart());

    let report = Health::current(&Config::from_env(), Some("http://x"), kind);
    assert_eq!(report.ccr_store, "file");
    assert!(report.ccr_store_persistent);

    config::clear_overrides();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_default_install_reports_memory_without_claiming_anything_failed() {
    let _guard = exclusive();
    // `"memory"` is the honest answer when nobody configured a store, and it must not
    // read as a fault. This is why the report carries two fields rather than one.
    config::clear_overrides();

    let (_, kind) = Config::ccr_store_with_kind();
    assert_eq!(kind, CcrStoreKind::Memory);
    assert!(
        !Config::persistent_store_requested(),
        "nothing was configured, so nothing should read as unfulfilled"
    );

    let state = AppState::new("http://127.0.0.1:1");
    assert_eq!(state.ccr_store_kind(), CcrStoreKind::Memory);
}
