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

/// The key an MCP host uses for its server map.
///
/// Every host this supports uses `mcpServers`. Named rather than inlined so the one
/// place to change it is obvious when a host picks something else.
const MCP_SERVERS_KEY: &str = "mcpServers";

/// Registers the headroom MCP server in `path`, backing the file up first.
///
/// Returns `false` when an entry named `headroom` was already there — installing twice
/// is a no-op that says so, not an error and not a silent overwrite. A customer may have
/// tuned the command or the arguments, and replacing that is not this command's call.
///
/// # Errors
///
/// Returns an error if the file cannot be read, is not a JSON object, or cannot be
/// written. A file that does not exist yet is **created**, since an MCP host with no
/// config is the normal starting state rather than a problem — unlike a *settings* file,
/// which the agent owns and this program should not invent.
pub fn install_mcp_server(path: &Path, command: &str) -> Result<bool> {
    let original = if path.exists() {
        std::fs::read(path).with_context(|| format!("reading {}", path.display()))?
    } else {
        b"{}".to_vec()
    };

    let mut config: serde_json::Value = serde_json::from_slice(&original)
        .with_context(|| format!("{} is not valid JSON", path.display()))?;
    let Some(object) = config.as_object_mut() else {
        bail!("{} is not a JSON object", path.display());
    };

    let servers = object
        .entry(MCP_SERVERS_KEY)
        .or_insert_with(|| serde_json::Value::Object(Default::default()));
    let Some(servers) = servers.as_object_mut() else {
        bail!("{MCP_SERVERS_KEY} in {} is not an object", path.display());
    };

    if servers.contains_key("headroom") {
        return Ok(false);
    }

    // The backup is written before the original is touched, and only when there was an
    // original — a file this command created has nothing to restore to, and leaving a
    // `{}` backup would make `uninstall` recreate an empty config the user never had.
    if path.exists() {
        let backup = backup_path(path);
        if !backup.exists() {
            std::fs::write(&backup, &original)
                .with_context(|| format!("writing backup {}", backup.display()))?;
        }
    }

    servers.insert(
        "headroom".into(),
        serde_json::json!({ "command": command, "args": [] }),
    );

    std::fs::write(path, serde_json::to_string_pretty(&config)?)
        .with_context(|| format!("writing {}", path.display()))?;

    Ok(true)
}

/// Removes the headroom entry from `path`.
///
/// Returns `false` when there was nothing to remove.
///
/// # Why this edits rather than restoring the backup
///
/// [`unwrap_settings_file`] restores bytes verbatim, which is right for a file this
/// program *rewrote*. An MCP config is different: the user may have added other servers
/// since installing, and restoring the backup would delete them. So this removes one
/// key and leaves everything else — accepting a reformat of the file in exchange for
/// not destroying work.
///
/// # Errors
///
/// Returns an error if the file cannot be read, is not JSON, or cannot be written.
pub fn uninstall_mcp_server(path: &Path) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }

    let original = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let mut config: serde_json::Value = serde_json::from_slice(&original)
        .with_context(|| format!("{} is not valid JSON", path.display()))?;

    let removed = config
        .as_object_mut()
        .and_then(|object| object.get_mut(MCP_SERVERS_KEY))
        .and_then(serde_json::Value::as_object_mut)
        .map(|servers| servers.remove("headroom").is_some())
        .unwrap_or(false);

    if !removed {
        return Ok(false);
    }

    std::fs::write(path, serde_json::to_string_pretty(&config)?)
        .with_context(|| format!("writing {}", path.display()))?;

    // The backup is left in place. It is the only record of what the file looked like
    // before this program touched it, and the user may still want it.
    Ok(true)
}

#[cfg(test)]
mod mcp_tests {
    use super::*;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("headroom-mcp-{name}"));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }

        fn file(&self, name: &str, contents: &str) -> PathBuf {
            let path = self.path(name);
            std::fs::write(&path, contents).unwrap();
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn read(path: &Path) -> serde_json::Value {
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
    }

    #[test]
    fn installing_adds_the_server_entry() {
        let scratch = Scratch::new("add");
        let path = scratch.file("mcp.json", r#"{"mcpServers":{}}"#);

        assert!(install_mcp_server(&path, "headroom-mcp").unwrap());
        assert_eq!(
            read(&path)["mcpServers"]["headroom"]["command"],
            "headroom-mcp"
        );
    }

    #[test]
    fn a_missing_config_is_created() {
        // An MCP host with no config is the normal starting state, unlike an agent's
        // settings file — which the agent owns and this program should not invent.
        let scratch = Scratch::new("create");
        let path = scratch.path("mcp.json");

        assert!(install_mcp_server(&path, "headroom-mcp").unwrap());
        assert!(path.exists());
        assert_eq!(
            read(&path)["mcpServers"]["headroom"]["command"],
            "headroom-mcp"
        );
    }

    #[test]
    fn other_servers_are_preserved() {
        // The failure this guards: a customer with three MCP servers configured loses
        // two of them to a command that was meant to add one.
        let scratch = Scratch::new("preserve");
        let path = scratch.file(
            "mcp.json",
            r#"{"mcpServers":{"other":{"command":"other-mcp"}},"theme":"dark"}"#,
        );

        install_mcp_server(&path, "headroom-mcp").unwrap();
        let config = read(&path);

        assert_eq!(config["mcpServers"]["other"]["command"], "other-mcp");
        assert_eq!(config["theme"], "dark", "an unrelated setting was lost");
    }

    #[test]
    fn installing_twice_does_not_overwrite_a_tuned_entry() {
        // A customer may have edited the command or the arguments. Replacing that is not
        // this command's call, and doing it silently is worse than reporting a no-op.
        let scratch = Scratch::new("twice");
        let path = scratch.file(
            "mcp.json",
            r#"{"mcpServers":{"headroom":{"command":"/custom/path","args":["--flag"]}}}"#,
        );

        assert!(!install_mcp_server(&path, "headroom-mcp").unwrap());
        assert_eq!(
            read(&path)["mcpServers"]["headroom"]["command"],
            "/custom/path"
        );
    }

    #[test]
    fn uninstalling_removes_only_the_headroom_entry() {
        // Restoring the backup would delete servers the user added *after* installing.
        let scratch = Scratch::new("remove");
        let path = scratch.file("mcp.json", r#"{"mcpServers":{}}"#);

        install_mcp_server(&path, "headroom-mcp").unwrap();

        // The user adds another server afterwards.
        let mut config = read(&path);
        config["mcpServers"]["added-later"] = serde_json::json!({ "command": "x" });
        std::fs::write(&path, serde_json::to_string(&config).unwrap()).unwrap();

        assert!(uninstall_mcp_server(&path).unwrap());
        let config = read(&path);

        assert!(config["mcpServers"].get("headroom").is_none());
        assert_eq!(
            config["mcpServers"]["added-later"]["command"], "x",
            "a server added after installing was deleted"
        );
    }

    #[test]
    fn uninstalling_something_never_installed_is_a_no_op() {
        let scratch = Scratch::new("noop");
        let path = scratch.file("mcp.json", r#"{"mcpServers":{"other":{}}}"#);

        assert!(!uninstall_mcp_server(&path).unwrap());
        assert!(read(&path)["mcpServers"]["other"].is_object());
    }

    #[test]
    fn uninstalling_a_missing_file_is_a_no_op_not_an_error() {
        let scratch = Scratch::new("absent");
        assert!(!uninstall_mcp_server(&scratch.path("nope.json")).unwrap());
    }

    #[test]
    fn a_config_this_command_created_gets_no_backup() {
        // A `{}` backup would make a restore recreate an empty config the user never had.
        let scratch = Scratch::new("nobackup");
        let path = scratch.path("mcp.json");

        install_mcp_server(&path, "headroom-mcp").unwrap();
        assert!(!is_wrapped(&path));
    }

    #[test]
    fn an_existing_config_is_backed_up_before_being_touched() {
        let scratch = Scratch::new("backup");
        let path = scratch.file("mcp.json", r#"{"mcpServers":{},"theme":"dark"}"#);

        install_mcp_server(&path, "headroom-mcp").unwrap();
        assert!(is_wrapped(&path));
    }

    #[test]
    fn a_non_object_config_is_refused_before_anything_is_touched() {
        let scratch = Scratch::new("array");
        let path = scratch.file("mcp.json", "[1,2,3]");

        assert!(install_mcp_server(&path, "headroom-mcp").is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[1,2,3]");
    }

    #[test]
    fn a_non_object_server_map_is_refused() {
        let scratch = Scratch::new("badmap");
        let path = scratch.file("mcp.json", r#"{"mcpServers":"not an object"}"#);

        assert!(install_mcp_server(&path, "headroom-mcp").is_err());
    }
}
