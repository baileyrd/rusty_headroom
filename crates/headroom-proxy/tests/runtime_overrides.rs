//! Taking a runtime override back.
//!
//! `set_overrides` merges (see D34), so `{}` no longer wipes anything and an empty value
//! is how a single setting is returned to its default. README.md promises that outright —
//! *"send a setting as an empty value to take it back"* — for every setting, which is a
//! claim about six independent parsers rather than about one code path.
//!
//! It holds because each parser treats an empty string as absent: `HEADROOM_UPSTREAM`
//! filters it, `HEADROOM_MEMORY_LIMIT` fails to parse it, `HEADROOM_STABILIZE` does not
//! match it against the on-spellings. That is six coincidences agreeing, not a shared
//! rule, so it is worth a test rather than an assumption — this was nearly documented as
//! a *wrinkle* on the reasoning that `HEADROOM_COMPRESSION` reads `""` as enabled. It
//! does, and enabled is also its default, so there is no divergence. Measuring settled
//! in seconds what reading had got wrong.
//!
//! One test function, because `set_overrides` writes one process-global map.

use std::collections::BTreeMap;

use headroom_proxy::config::{self, CcrStoreKind, Config};

/// Every setting whose value survives to somewhere observable.
const SETTINGS: [&str; 6] = [
    config::vars::UPSTREAM,
    config::vars::COMPRESSION,
    config::vars::OUTPUT_SHAPER,
    config::vars::STABILIZE,
    config::vars::MEMORY_LIMIT,
    config::vars::CCR_DIR,
];

/// Everything an override could move, read together.
fn observable() -> (String, bool, String, bool, usize, CcrStoreKind) {
    let config = Config::from_env();
    (
        config.upstream().to_owned(),
        config.compression_enabled(),
        format!("{:?}", config.verbosity()),
        Config::stabilization_enabled(),
        Config::memory_limit(),
        Config::ccr_store_with_kind().1,
    )
}

/// A value that visibly changes `name`, so the clearing below has something to undo.
fn a_value_that_changes(name: &str) -> &'static str {
    match name {
        config::vars::UPSTREAM => "http://example.invalid",
        config::vars::COMPRESSION => "0",
        config::vars::OUTPUT_SHAPER => "terse",
        config::vars::STABILIZE => "1",
        config::vars::MEMORY_LIMIT => "3",
        _ => &UNUSABLE_DIRECTORY,
    }
}

/// A directory path that can never become usable on any platform: the parent
/// component is an ordinary file, not a directory, so any attempt to create or open
/// something inside it fails rather than silently succeeding. `/proc/self/mem` served
/// this purpose on Linux only — on Windows it is an ordinary, creatable path, which
/// defeated the point of using an "unusable" `HEADROOM_CCR_DIR` here.
static UNUSABLE_DIRECTORY: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    let blocker = std::env::temp_dir().join("headroom-runtime-overrides-unusable-blocker");
    std::fs::write(&blocker, b"blocker").expect("could not create the blocker file");
    blocker.join("not-a-directory").display().to_string()
});

#[test]
fn an_empty_value_returns_any_setting_to_its_default() {
    config::clear_overrides();
    let default = observable();

    for name in SETTINGS {
        // The vacuity guard, and the reason this is not six assertions that nothing
        // happened. `HEADROOM_CCR_DIR` is the one that needs it most: an unopenable
        // directory falls back to memory, which is *also* the default, so without
        // checking that something moved first, clearing it would prove nothing.
        let mut set = BTreeMap::new();
        set.insert(name.to_owned(), a_value_that_changes(name).to_owned());
        config::set_overrides(set);

        let changed = observable();
        if name == config::vars::CCR_DIR {
            // Its fallback is indistinguishable from its default by observation, so the
            // override map is the only witness that anything was set.
            assert_eq!(
                config::overrides().get(name).map(String::as_str),
                Some(a_value_that_changes(name)),
                "{name} was not recorded, so clearing it proves nothing"
            );
        } else {
            assert_ne!(
                changed, default,
                "{name} did not move anything, so clearing it proves nothing"
            );
        }

        let mut cleared = BTreeMap::new();
        cleared.insert(name.to_owned(), String::new());
        config::set_overrides(cleared);

        assert_eq!(
            observable(),
            default,
            "{name} set to an empty value did not behave as unset"
        );
        config::clear_overrides();
    }
    // The loop above never sets the real process environment, so `default` and the
    // post-clear value can agree even when clearing merely fell back to a hardcoded
    // default rather than genuinely restoring what the environment says. That was
    // exactly the bug: `set_overrides` stored the empty string as a real override
    // instead of removing the key, so `HEADROOM_COMPRESSION` read `Some("")` forever
    // after the first clear — which happens to parse as `true`, `compression_enabled`'s
    // own no-config default, so nothing above could tell the difference.
    //
    // With `HEADROOM_COMPRESSION=0` actually set in the environment, clearing an
    // override on it must land back on `false`, not on `true`.
    std::env::set_var(config::vars::COMPRESSION, "0");
    config::clear_overrides();
    assert!(
        !Config::from_env().compression_enabled(),
        "HEADROOM_COMPRESSION=0 in the environment was not honored with no override in force"
    );

    let mut set = BTreeMap::new();
    set.insert(config::vars::COMPRESSION.to_owned(), "1".to_owned());
    config::set_overrides(set);
    assert!(
        Config::from_env().compression_enabled(),
        "the override did not take"
    );

    let mut cleared = BTreeMap::new();
    cleared.insert(config::vars::COMPRESSION.to_owned(), String::new());
    config::set_overrides(cleared);
    assert!(
        !Config::from_env().compression_enabled(),
        "clearing the override with an empty value did not restore \
         HEADROOM_COMPRESSION=0 from the environment"
    );

    std::env::remove_var(config::vars::COMPRESSION);
    config::clear_overrides();

}
