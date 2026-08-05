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
}
