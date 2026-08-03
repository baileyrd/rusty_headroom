//! Pointing an agent at the proxy, and putting it back.
//!
//! # Unwrap is the feature
//!
//! Wrapping is easy: change a base URL. The part that has to be right is *undoing* it.
//! A `headroom unwrap` that leaves an agent half-configured has broken the customer's
//! tooling in a way they will attribute to their agent rather than to this program, and
//! they will debug it in the wrong place.
//!
//! So the backup holds the **original bytes of the whole file**, not a record of what
//! was changed, and unwrap restores those bytes verbatim. Reconstructing the original by
//! reversing each edit sounds equivalent and is not: it silently rewrites formatting,
//! reorders keys, and drops anything the writer did not understand.
//!
//! The corollary is that **wrapping twice must not overwrite the backup**. The second
//! wrap would capture an already-wrapped file, and unwrap would then restore the
//! customer to the wrapped state while reporting success.
//!
//! # Two ways an agent is configured
//!
//! Some agents read a base URL from the environment; some read it from a settings file.
//! Environment-only agents get printed exports rather than a file this program has no
//! business writing to — a shell profile belongs to its owner.

use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

/// Suffix appended to a wrapped file to hold its original bytes.
const BACKUP_SUFFIX: &str = ".headroom-backup";

/// An agent that can be pointed at the proxy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agent {
    /// Claude Code.
    Claude,
    /// OpenAI Codex CLI.
    Codex,
    /// Cursor.
    Cursor,
    /// Aider.
    Aider,
    /// Cline.
    Cline,
    /// Continue.
    Continue,
    /// Goose.
    Goose,
    /// OpenHands.
    OpenHands,
}

impl Agent {
    /// Every supported agent.
    pub const ALL: [Agent; 8] = [
        Agent::Claude,
        Agent::Codex,
        Agent::Cursor,
        Agent::Aider,
        Agent::Cline,
        Agent::Continue,
        Agent::Goose,
        Agent::OpenHands,
    ];

    /// Parses an agent name.
    pub fn parse(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "claude" | "claude-code" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            "cursor" => Some(Self::Cursor),
            "aider" => Some(Self::Aider),
            "cline" => Some(Self::Cline),
            "continue" => Some(Self::Continue),
            "goose" => Some(Self::Goose),
            "openhands" | "open-hands" => Some(Self::OpenHands),
            _ => None,
        }
    }

    /// The canonical name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Cursor => "cursor",
            Self::Aider => "aider",
            Self::Cline => "cline",
            Self::Continue => "continue",
            Self::Goose => "goose",
            Self::OpenHands => "openhands",
        }
    }

    /// The environment variables that point this agent at `proxy`.
    ///
    /// Anthropic-shaped agents get `ANTHROPIC_BASE_URL`; OpenAI-shaped ones get
    /// `OPENAI_BASE_URL`. Agents that speak both get both, since setting the unused one
    /// costs nothing and guessing wrong costs a confusing failure.
    pub fn env(self, proxy: &str) -> Vec<(&'static str, String)> {
        let proxy = proxy.trim_end_matches('/').to_owned();
        match self {
            Self::Claude => vec![("ANTHROPIC_BASE_URL", proxy)],
            Self::Codex => vec![("OPENAI_BASE_URL", format!("{proxy}/v1"))],
            Self::Aider | Self::Cline | Self::Continue | Self::Goose | Self::OpenHands => vec![
                ("ANTHROPIC_BASE_URL", proxy.clone()),
                ("OPENAI_BASE_URL", format!("{proxy}/v1")),
            ],
            // Cursor is configured through its own settings UI and does not read a base
            // URL from the environment. Returning nothing is the honest answer; the
            // command reports it as unsupported rather than printing exports that would
            // do nothing.
            Self::Cursor => Vec::new(),
        }
    }

    /// Whether this agent can be wrapped by setting environment variables.
    pub fn env_configurable(self) -> bool {
        !self.env("http://x").is_empty()
    }
}

impl fmt::Display for Agent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Backs up `path` and rewrites its `base_url` to `proxy`.
///
/// The file must be JSON. Returns the path that was written.
///
/// # Errors
///
/// Returns an error if the file cannot be read, is not JSON, or cannot be written.
/// Fails **before** touching the original if the backup cannot be created — a rewrite
/// that cannot be undone is worse than one that never happened.
pub fn wrap_settings_file(path: &Path, proxy: &str) -> Result<PathBuf> {
    let original = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;

    let backup = backup_path(path);
    if backup.exists() {
        // The second wrap would capture an already-wrapped file, and unwrap would then
        // restore the customer to the wrapped state while reporting success.
        bail!(
            "{} already exists; {} appears to be wrapped already",
            backup.display(),
            path.display()
        );
    }

    let mut settings: serde_json::Value = serde_json::from_slice(&original)
        .with_context(|| format!("{} is not valid JSON", path.display()))?;
    let object = match settings.as_object_mut() {
        Some(object) => object,
        None => bail!("{} is not a JSON object", path.display()),
    };

    // Written before the original is touched. If this fails there is nothing to undo.
    std::fs::write(&backup, &original)
        .with_context(|| format!("writing backup {}", backup.display()))?;

    object.insert(
        "base_url".into(),
        serde_json::Value::String(proxy.trim_end_matches('/').to_owned()),
    );

    let rendered = serde_json::to_string_pretty(&settings)?;
    std::fs::write(path, rendered).with_context(|| format!("writing {}", path.display()))?;

    Ok(path.to_path_buf())
}

/// Restores `path` from its backup, byte for byte.
///
/// Returns `false` when there is no backup — which is not an error. `unwrap` on an
/// unwrapped agent should be a no-op that says so, not a failure: the state the caller
/// asked for is the state they already have.
///
/// # Errors
///
/// Returns an error if the backup exists but cannot be read or restored. In that case
/// the backup is **left in place**, so the original is still recoverable by hand.
pub fn unwrap_settings_file(path: &Path) -> Result<bool> {
    let backup = backup_path(path);
    if !backup.exists() {
        return Ok(false);
    }

    let original =
        std::fs::read(&backup).with_context(|| format!("reading backup {}", backup.display()))?;

    // Restored verbatim rather than by reversing the edit. Reversing sounds equivalent
    // and is not: it rewrites formatting, reorders keys, and drops anything the writer
    // did not understand.
    std::fs::write(path, &original).with_context(|| format!("restoring {}", path.display()))?;

    // Removed only after the restore succeeded. A backup deleted first and a write that
    // then fails leaves the customer with neither version.
    std::fs::remove_file(&backup)
        .with_context(|| format!("removing backup {}", backup.display()))?;

    Ok(true)
}

/// The backup path for `path`.
fn backup_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(BACKUP_SUFFIX);
    PathBuf::from(name)
}

/// Whether `path` currently has a backup, i.e. appears wrapped.
pub fn is_wrapped(path: &Path) -> bool {
    backup_path(path).exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch directory that cleans itself up.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("headroom-wrap-{name}"));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn file(&self, name: &str, contents: &str) -> PathBuf {
            let path = self.0.join(name);
            std::fs::write(&path, contents).unwrap();
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    // ---- agent identity ----

    #[test]
    fn every_agent_round_trips_through_its_name() {
        for agent in Agent::ALL {
            assert_eq!(Agent::parse(agent.as_str()), Some(agent));
        }
    }

    #[test]
    fn agent_names_are_case_and_alias_tolerant() {
        assert_eq!(Agent::parse("CLAUDE"), Some(Agent::Claude));
        assert_eq!(Agent::parse(" claude-code "), Some(Agent::Claude));
        assert_eq!(Agent::parse("open-hands"), Some(Agent::OpenHands));
        assert_eq!(Agent::parse("emacs"), None);
    }

    #[test]
    fn an_openai_shaped_agent_gets_the_v1_suffix() {
        // The OpenAI SDKs expect a base URL that already includes `/v1`; the Anthropic
        // ones do not. Getting this backwards produces `/v1/v1/chat/completions`, which
        // fails as a 404 that looks like the proxy is broken.
        let env = Agent::Codex.env("http://127.0.0.1:8787");
        assert_eq!(env[0].0, "OPENAI_BASE_URL");
        assert!(env[0].1.ends_with("/v1"));

        let env = Agent::Claude.env("http://127.0.0.1:8787");
        assert_eq!(env[0].0, "ANTHROPIC_BASE_URL");
        assert!(!env[0].1.ends_with("/v1"));
    }

    #[test]
    fn a_trailing_slash_on_the_proxy_url_does_not_double_up() {
        let env = Agent::Codex.env("http://127.0.0.1:8787/");
        assert_eq!(env[0].1, "http://127.0.0.1:8787/v1");
    }

    #[test]
    fn an_agent_that_cannot_be_wrapped_by_env_says_so() {
        // Printing exports that do nothing is worse than reporting the limitation: the
        // customer would believe they are routed through the proxy and see no savings,
        // with nothing to explain why.
        assert!(!Agent::Cursor.env_configurable());
        for agent in Agent::ALL.into_iter().filter(|a| *a != Agent::Cursor) {
            assert!(agent.env_configurable(), "{agent}");
        }
    }

    // ---- settings files ----

    #[test]
    fn wrapping_rewrites_the_base_url() {
        let scratch = Scratch::new("rewrite");
        let path = scratch.file(
            "settings.json",
            r#"{"model":"opus","base_url":"https://api.anthropic.com"}"#,
        );

        wrap_settings_file(&path, "http://127.0.0.1:8787").unwrap();

        let written: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(written["base_url"], "http://127.0.0.1:8787");
        assert_eq!(written["model"], "opus", "an unrelated setting was lost");
    }

    #[test]
    fn unwrapping_restores_the_original_bytes_exactly() {
        // The property the whole module exists for. Not "equivalent JSON" — the same
        // bytes, including formatting and key order, because anything less is a change
        // the customer did not ask for and will notice in a diff.
        let scratch = Scratch::new("restore");
        let original = "{\n  // a comment-shaped string\n  \"base_url\": \"https://api.anthropic.com\",\n  \"z\": 1,\n  \"a\": 2\n}";
        // Strip the comment line, which is not valid JSON — the point is unusual but
        // legal formatting.
        let original = original.replace("  // a comment-shaped string\n", "");
        let path = scratch.file("settings.json", &original);

        wrap_settings_file(&path, "http://127.0.0.1:8787").unwrap();
        assert_ne!(std::fs::read_to_string(&path).unwrap(), original);

        assert!(unwrap_settings_file(&path).unwrap());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            original,
            "the restore was not byte-exact"
        );
    }

    #[test]
    fn unwrapping_removes_the_backup() {
        let scratch = Scratch::new("cleanup");
        let path = scratch.file("settings.json", r#"{"base_url":"x"}"#);

        wrap_settings_file(&path, "http://127.0.0.1:8787").unwrap();
        assert!(is_wrapped(&path));

        unwrap_settings_file(&path).unwrap();
        assert!(!is_wrapped(&path), "a stale backup was left behind");
    }

    #[test]
    fn wrapping_twice_refuses_rather_than_overwriting_the_backup() {
        // The failure this prevents: the second wrap captures an already-wrapped file,
        // and unwrap then restores the customer to the wrapped state while reporting
        // success — leaving them permanently routed through a proxy they thought they
        // had removed.
        let scratch = Scratch::new("twice");
        let original = r#"{"base_url":"https://api.anthropic.com"}"#;
        let path = scratch.file("settings.json", original);

        wrap_settings_file(&path, "http://127.0.0.1:8787").unwrap();
        assert!(wrap_settings_file(&path, "http://127.0.0.1:9999").is_err());

        // And the original is still recoverable.
        unwrap_settings_file(&path).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn unwrapping_something_never_wrapped_is_a_no_op_not_an_error() {
        // The state the caller asked for is the state they already have.
        let scratch = Scratch::new("noop");
        let path = scratch.file("settings.json", r#"{"base_url":"x"}"#);

        assert!(!unwrap_settings_file(&path).unwrap());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            r#"{"base_url":"x"}"#
        );
    }

    #[test]
    fn a_settings_file_that_is_not_json_is_refused_before_anything_is_touched() {
        let scratch = Scratch::new("notjson");
        let path = scratch.file("settings.toml", "base_url = \"x\"");

        assert!(wrap_settings_file(&path, "http://127.0.0.1:8787").is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "base_url = \"x\"");
        assert!(!is_wrapped(&path), "a backup was left for a failed wrap");
    }

    #[test]
    fn a_missing_settings_file_is_an_error_not_a_created_one() {
        // Creating a config file the agent never had would leave a file behind that
        // unwrap has no record of, and the customer with settings they did not write.
        let scratch = Scratch::new("missing");
        let path = scratch.0.join("absent.json");

        assert!(wrap_settings_file(&path, "http://127.0.0.1:8787").is_err());
        assert!(!path.exists());
    }

    #[test]
    fn a_json_array_settings_file_is_refused() {
        let scratch = Scratch::new("array");
        let path = scratch.file("settings.json", "[1,2,3]");

        assert!(wrap_settings_file(&path, "http://127.0.0.1:8787").is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[1,2,3]");
    }
}
