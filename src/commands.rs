//! Headless `gray cmd` commands over IRC (GRAY-CMD-CONTRACT.md).
//!
//! `!name args` in a channel; `/name args`, `!name args`, or a bare known
//! `name` in DM. Answers come back as PRIVMSG text — IRC has no components,
//! so `kind:"ui"` results render through `fallback_text`. A command never
//! enqueues a turn: it spawns `GRAY_HOME=<home> gray cmd run` the same way
//! the runner spawns turns and replies inline. `local` commands answer
//! `local_only` server-side.
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;

/// `gray cmd list` is re-fetched at most this often.
const LIST_TTL: Duration = Duration::from_secs(300);
/// Listing is a local CLI call; a stale cache is better than a stalled loop.
const LIST_TIMEOUT: Duration = Duration::from_secs(15);
/// Commands are meant to be quick; bound a hung one anyway.
const RUN_TIMEOUT: Duration = Duration::from_secs(60);
/// Registry rows marked `slow` get this window instead.
const SLOW_TIMEOUT: Duration = Duration::from_secs(120);
/// Concurrent `gray cmd run` children across all conversations.
const MAX_RUNS: usize = 4;
/// Registry used until `gray cmd list` answers (older gray may lack `cmd`).
const FALLBACK: &[&str] = &[
    "help", "model", "usage", "new", "sessions", "cron", "skills",
];

/// Which home a command runs against (`cmd list`'s `scope` field).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Conversation,
    Global,
}

#[derive(Default)]
struct CmdState {
    /// When `names` was last settled (list success or failure).
    fetched: Option<Instant>,
    /// A refresh task is in flight; do not stack another.
    refreshing: bool,
    /// Canonical name or alias -> scope. Empty until the first fetch.
    names: BTreeMap<String, Scope>,
    /// Names (and aliases) whose runs can outlast RUN_TIMEOUT.
    slow: BTreeSet<String>,
}

pub struct Commands {
    state: Mutex<CmdState>,
    permits: tokio::sync::Semaphore,
}

impl Commands {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(CmdState::default()),
            permits: tokio::sync::Semaphore::new(MAX_RUNS),
        }
    }

    /// `(name, args)` when `text` invokes a known command, else None — the
    /// caller treats the message as a normal prompt. Never blocks: the
    /// registry is the fresh cache, a stale one, or the fallback, and a
    /// stale cache kicks a background refresh.
    pub fn parse(
        self: &Arc<Self>,
        config: &Value,
        text: &str,
        is_dm: bool,
    ) -> Option<(String, String)> {
        parse_prefixed(text, is_dm, &self.working(config))
    }

    /// The registry to parse against right now (see `parse`).
    fn working(self: &Arc<Self>, config: &Value) -> BTreeMap<String, Scope> {
        let (cached, kick) = {
            let mut st = match self.state.lock() {
                Ok(st) => st,
                Err(_) => return fallback_set(),
            };
            let fresh = st.fetched.is_some_and(|t| t.elapsed() < LIST_TTL);
            if fresh {
                return st.names.clone();
            }
            let kick = !st.refreshing;
            if kick {
                st.refreshing = true;
            }
            (st.names.clone(), kick)
        };
        if kick {
            let me = Arc::clone(self);
            let bin = crate::irc::cfg_str(config, "gray_bin").to_string();
            let home = crate::irc::cfg_str(config, "gray_home").to_string();
            tokio::spawn(async move {
                let listed = fetch_registry(&bin, Path::new(&home)).await;
                if let Ok(mut st) = me.state.lock() {
                    if let Some(reg) = listed.filter(|r| !r.0.is_empty()) {
                        st.names = reg.0;
                        st.slow = reg.1;
                    }
                    st.fetched = Some(Instant::now());
                    st.refreshing = false;
                }
            });
        }
        if cached.is_empty() {
            fallback_set()
        } else {
            cached
        }
    }

    /// Scope for a parsed name; unlisted names stay conversation-local.
    fn scope_of(&self, name: &str) -> Scope {
        self.state
            .lock()
            .ok()
            .and_then(|st| st.names.get(name).copied())
            .unwrap_or(Scope::Conversation)
    }

    /// Whether the registry marks this name slow.
    fn slow_of(&self, name: &str) -> bool {
        self.state
            .lock()
            .ok()
            .map(|st| st.slow.contains(name))
            .unwrap_or(false)
    }

    /// Run one validated command; the caller posts `Text` or enqueues
    /// `Prompt` as a turn. Spawning mirrors the runner's child env.
    pub async fn run(
        &self,
        config: &Value,
        config_path: &Path,
        conversation: &str,
        name: &str,
        args: &str,
    ) -> Outcome {
        let Ok(_permit) = self.permits.try_acquire() else {
            return Outcome::Text("Commands are busy; try again in a moment.".to_string());
        };
        let scope = self.scope_of(name);
        let (home, reset) = match scope {
            Scope::Global => (
                PathBuf::from(crate::irc::cfg_str(config, "gray_home")),
                false,
            ),
            Scope::Conversation => {
                let home = match ensure_home(config, config_path, conversation) {
                    Ok(h) => h,
                    Err(e) => return Outcome::Text(e),
                };
                // `new` must drop the plugin's pinned session too — gray's
                // own pointer reset alone would leave `session.json` forcing
                // the old session back onto the next turn.
                let reset = name == "new"
                    && crate::session::reset_home(&home, crate::store::now_secs()).is_ok();
                (home, reset)
            }
        };
        let mut argv = vec!["cmd".to_string(), "run".to_string(), name.to_string()];
        if !args.is_empty() {
            argv.push(args.to_string());
        }
        let timeout = if self.slow_of(name) {
            SLOW_TIMEOUT
        } else {
            RUN_TIMEOUT
        };
        match spawn_cmd(config, &home, &argv, timeout).await {
            Ok(v) => reply_from(&v, name),
            Err(e) if reset => {
                let _ = e;
                Outcome::Text("Session reset — the next message starts fresh.".to_string())
            }
            Err(e) => Outcome::Text(e),
        }
    }
}

/// What a `cmd run` answer becomes: reply text, or a prompt the caller
/// queues as a turn in this conversation.
#[derive(Debug)]
pub enum Outcome {
    Text(String),
    Prompt { prompt: String, fallback: String },
}

impl Default for Commands {
    fn default() -> Self {
        Self::new()
    }
}

fn split_first(s: &str) -> (&str, &str) {
    let s = s.trim_start();
    match s.find(char::is_whitespace) {
        Some(i) => (&s[..i], s[i..].trim()),
        None => (s, ""),
    }
}

/// gray names commands like `model`/`compact`; keep the check structural.
fn name_valid(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name.as_bytes()[0].is_ascii_alphanumeric()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

/// `(name, args)` for a command invocation, else None (prompt path).
/// Channel text needs `!`; DM takes `/`, `!`, or a bare single known word.
/// The name must be syntactically valid *and* in `known` — an unknown name
/// is a prompt, not a command.
pub fn parse_prefixed(
    text: &str,
    is_dm: bool,
    known: &BTreeMap<String, Scope>,
) -> Option<(String, String)> {
    let t = text.trim();
    let (name, args) = if is_dm {
        match t.strip_prefix('/').or_else(|| t.strip_prefix('!')) {
            Some(body) => split_first(body),
            // Bare form: one word only, and only when it is a real command —
            // "new phone who dis" must stay a prompt.
            None if !t.contains(char::is_whitespace) => (t, ""),
            None => return None,
        }
    } else {
        split_first(t.strip_prefix('!')?)
    };
    let name = name.to_ascii_lowercase();
    if !name_valid(&name) || !known.contains_key(&name) {
        return None;
    }
    Some((name, args.to_string()))
}

/// `cmd list` JSON -> (name/alias -> scope map, slow-name set).
fn registry_from(v: &Value) -> (BTreeMap<String, Scope>, BTreeSet<String>) {
    let mut out = BTreeMap::new();
    let mut slow = BTreeSet::new();
    let Some(commands) = v["result"]["commands"].as_array() else {
        return (out, slow);
    };
    for c in commands {
        if c["source"].as_str() == Some("skill") {
            continue;
        }
        let scope = if c["scope"].as_str() == Some("global") {
            Scope::Global
        } else {
            Scope::Conversation
        };
        let is_slow = c["slow"].as_bool().unwrap_or(false);
        if let Some(name) = c["name"].as_str() {
            let name = name.to_ascii_lowercase();
            out.insert(name.clone(), scope);
            if is_slow {
                slow.insert(name);
            }
        }
        if let Some(aliases) = c["aliases"].as_array() {
            for a in aliases.iter().filter_map(Value::as_str) {
                let a = a.to_ascii_lowercase();
                out.entry(a.clone()).or_insert(scope);
                if is_slow {
                    slow.insert(a);
                }
            }
        }
    }
    (out, slow)
}

fn fallback_set() -> BTreeMap<String, Scope> {
    FALLBACK
        .iter()
        .map(|n| (n.to_string(), Scope::Conversation))
        .collect()
}

/// `GRAY_HOME=<main home> gray cmd list`; errors just keep the old set.
async fn fetch_registry(
    gray_bin: &str,
    gray_home: &Path,
) -> Option<(BTreeMap<String, Scope>, BTreeSet<String>)> {
    if gray_bin.is_empty() {
        return None;
    }
    let config = serde_json::json!({ "gray_bin": gray_bin });
    let v = spawn_cmd(
        &config,
        gray_home,
        &["cmd".into(), "list".into()],
        LIST_TIMEOUT,
    )
    .await
    .ok()?;
    if v["ok"].as_bool() == Some(true) {
        Some(registry_from(&v))
    } else {
        None
    }
}

/// Create the conversation home and seed the provider config once — the
/// runner re-snapshots every turn, but commands must not clobber a config
/// `gray cmd` just persisted (e.g. `/model`).
fn ensure_home(config: &Value, config_path: &Path, conversation: &str) -> Result<PathBuf, String> {
    let home = crate::session::conversation_home(config_path, conversation);
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&home)
        .map_err(|_| "Cannot create the conversation home.".to_string())?;
    let _ = std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700));
    let local_cfg = home.join("config.json");
    if !local_cfg.exists() {
        let provider: Option<Value> =
            std::fs::read(Path::new(crate::irc::cfg_str(config, "gray_home")).join("config.json"))
                .ok()
                .and_then(|b| serde_json::from_slice(&b).ok());
        if let Some(provider) = provider {
            crate::config::atomic_json(&local_cfg, &provider)?;
        }
    }
    Ok(home)
}

/// `GRAY_HOME=<home> gray <argv...>`; returns the single wire object.
/// Child env mirrors the runner: stripped GRAY_/IRC_/OPENAI_, rebuilt PATH.
async fn spawn_cmd(
    config: &Value,
    home: &Path,
    argv: &[String],
    timeout: Duration,
) -> Result<Value, String> {
    let gray_bin = crate::irc::cfg_str(config, "gray_bin");
    if gray_bin.is_empty() {
        return Err("The gray executable is not configured.".to_string());
    }
    let mut cmd = tokio::process::Command::new(gray_bin);
    cmd.args(argv);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::null());
    cmd.kill_on_drop(true);
    // Own process group so a timeout SIGKILL reaches grandchildren too.
    unsafe {
        cmd.pre_exec(|| {
            libc::setpgid(0, 0);
            Ok(())
        });
    }
    cmd.env_clear();
    for (k, v) in std::env::vars_os() {
        let Some(k_str) = k.to_str() else { continue };
        if k_str.starts_with("GRAY_") || k_str.starts_with("IRC_") || k_str.starts_with("OPENAI_") {
            continue;
        }
        cmd.env(k, v);
    }
    cmd.env("GRAY_HOME", home);
    cmd.env(
        "PATH",
        crate::runner::child_path(
            std::env::var_os("PATH"),
            std::env::var_os("HOME").map(PathBuf::from),
            Path::new(gray_bin),
        ),
    );

    let mut child = cmd.spawn().map_err(|_| "Cannot start gray.".to_string())?;
    let pid = child.id().unwrap_or(0);
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Cannot start gray.".to_string())?;
    let drain = async move {
        let mut buf = Vec::new();
        stdout.read_to_end(&mut buf).await.map(|_| buf)
    };
    let bytes = match tokio::time::timeout(timeout, drain).await {
        Ok(Ok(b)) => {
            let _ = child.wait().await;
            b
        }
        Ok(Err(_)) => {
            let _ = child.wait().await;
            return Err("Cannot read gray output.".to_string());
        }
        Err(_) => {
            if pid != 0 {
                unsafe {
                    libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
                }
            }
            let _ = child.wait().await;
            return Err("That command timed out.".to_string());
        }
    };
    let text = String::from_utf8_lossy(&bytes);
    serde_json::from_str::<Value>(text.trim()).map_err(|_| {
        "gray returned no command answer — this build may not support `gray cmd`.".to_string()
    })
}

/// Wire object -> the room action: `text` results verbatim, `ui` via
/// `fallback_text`, `prompt` hands back a turn, error codes as plain
/// sentences.
fn reply_from(v: &Value, name: &str) -> Outcome {
    if v["ok"].as_bool() == Some(true) {
        let result = &v["result"];
        if result["kind"].as_str() == Some("prompt") {
            let prompt = result["prompt"].as_str().unwrap_or("").trim().to_string();
            if prompt.is_empty() {
                return Outcome::Text("gray answered an empty prompt.".to_string());
            }
            let fallback = result["fallback_text"].as_str().unwrap_or("").to_string();
            return Outcome::Prompt { prompt, fallback };
        }
        let text = match result["kind"].as_str() {
            Some("ui") => result["fallback_text"].as_str(),
            _ => result["text"].as_str(),
        };
        let text = text.unwrap_or("").trim_end();
        Outcome::Text(if text.is_empty() {
            "Done.".to_string()
        } else {
            text.to_string()
        })
    } else {
        let code = v["error"]["code"].as_str().unwrap_or("");
        let msg = v["error"]["message"].as_str().unwrap_or("").trim();
        Outcome::Text(match code {
            "local_only" => format!("`{name}` only works in the gray TUI."),
            "unknown_command" => format!("Unknown command `{name}`."),
            _ if !msg.is_empty() => msg.to_string(),
            _ => "That command is unavailable right now.".to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn known() -> BTreeMap<String, Scope> {
        let mut m = fallback_set();
        m.insert("plugin".to_string(), Scope::Global);
        m
    }

    #[test]
    fn channel_bang_commands() {
        let k = known();
        assert_eq!(
            parse_prefixed("!help", false, &k),
            Some(("help".into(), String::new()))
        );
        assert_eq!(
            parse_prefixed("!model openai/gpt-5", false, &k),
            Some(("model".into(), "openai/gpt-5".into()))
        );
        assert_eq!(
            parse_prefixed("!MODEL fast mode", false, &k),
            Some(("model".into(), "fast mode".into()))
        );
        assert_eq!(
            parse_prefixed("  !usage  ", false, &k),
            Some(("usage".into(), String::new()))
        );
    }

    #[test]
    fn channel_non_commands() {
        let k = known();
        // Slash and bare words are DM forms; a channel needs `!`.
        assert_eq!(parse_prefixed("/help", false, &k), None);
        assert_eq!(parse_prefixed("help", false, &k), None);
        assert_eq!(parse_prefixed("!unknown", false, &k), None);
        assert_eq!(parse_prefixed("!", false, &k), None);
        assert_eq!(parse_prefixed("! args", false, &k), None);
        assert_eq!(parse_prefixed("!-bad", false, &k), None);
        assert_eq!(parse_prefixed("!has space", false, &k), None);
    }

    #[test]
    fn dm_forms() {
        let k = known();
        assert_eq!(
            parse_prefixed("/sessions", true, &k),
            Some(("sessions".into(), String::new()))
        );
        assert_eq!(
            parse_prefixed("!cron", true, &k),
            Some(("cron".into(), String::new()))
        );
        assert_eq!(
            parse_prefixed("/model gpt-5", true, &k),
            Some(("model".into(), "gpt-5".into()))
        );
        // Bare form: exactly one known word.
        assert_eq!(
            parse_prefixed("usage", true, &k),
            Some(("usage".into(), String::new()))
        );
        assert_eq!(parse_prefixed("usage extra", true, &k), None);
        assert_eq!(parse_prefixed("new phone who dis", true, &k), None);
        assert_eq!(parse_prefixed("hello", true, &k), None);
        assert_eq!(parse_prefixed("/nope", true, &k), None);
        assert_eq!(parse_prefixed("/", true, &k), None);
    }

    #[test]
    fn registry_parses_list_fixture() {
        let v = serde_json::json!({
            "ok": true, "protocol": 1,
            "result": {"commands": [
                {"name": "model", "description": "", "aliases": ["m"],
                 "args": [], "scope": "conversation", "surface": "interactive"},
                {"name": "plugin", "description": "", "aliases": [],
                 "args": [], "scope": "global", "surface": "text"},
                {"name": "theme", "description": "", "aliases": [],
                 "args": [], "scope": "conversation", "surface": "local"}
            ]}
        });
        let (m, slow) = registry_from(&v);
        assert_eq!(m["model"], Scope::Conversation);
        assert_eq!(m["m"], Scope::Conversation);
        assert_eq!(m["plugin"], Scope::Global);
        assert_eq!(m["theme"], Scope::Conversation);
        assert!(slow.is_empty());
        assert_eq!(
            parse_prefixed("!m gpt-5", false, &m),
            Some(("m".into(), "gpt-5".into()))
        );
    }

    #[test]
    fn reply_from_fixtures() {
        let text = serde_json::json!({
            "ok": true, "result": {"kind": "text", "text": "**3** sessions", "close": true}
        });
        match reply_from(&text, "sessions") {
            Outcome::Text(t) => assert_eq!(t, "**3** sessions"),
            other => panic!("expected text: {other:?}"),
        }

        let ui = serde_json::json!({
            "ok": true, "result": {"kind": "ui", "token": "t",
                "document": {}, "fallback_text": "1. low\n2. high"}
        });
        match reply_from(&ui, "model") {
            Outcome::Text(t) => assert_eq!(t, "1. low\n2. high"),
            other => panic!("expected text: {other:?}"),
        }

        let ui_no_fallback = serde_json::json!({
            "ok": true, "result": {"kind": "ui", "token": "t", "document": {}}
        });
        match reply_from(&ui_no_fallback, "model") {
            Outcome::Text(t) => assert_eq!(t, "Done."),
            other => panic!("expected text: {other:?}"),
        }

        let local = serde_json::json!({
            "ok": false, "error": {"code": "local_only",
                "message": "theme is TUI-only", "retryable": false}
        });
        match reply_from(&local, "theme") {
            Outcome::Text(t) => assert_eq!(t, "`theme` only works in the gray TUI."),
            other => panic!("expected text: {other:?}"),
        }

        let unknown = serde_json::json!({
            "ok": false, "error": {"code": "unknown_command", "message": "?", "retryable": false}
        });
        match reply_from(&unknown, "frob") {
            Outcome::Text(t) => assert_eq!(t, "Unknown command `frob`."),
            other => panic!("expected text: {other:?}"),
        }

        let args = serde_json::json!({
            "ok": false, "error": {"code": "invalid_args",
                "message": "usage: model <id>", "retryable": false}
        });
        match reply_from(&args, "model") {
            Outcome::Text(t) => assert_eq!(t, "usage: model <id>"),
            other => panic!("expected text: {other:?}"),
        }

        let bare = serde_json::json!({"ok": false, "error": {}});
        match reply_from(&bare, "x") {
            Outcome::Text(t) => assert_eq!(t, "That command is unavailable right now."),
            other => panic!("expected text: {other:?}"),
        }
    }

    /// A stand-in `gray` binary: prints one fixture object like `cmd run`.
    fn fake_gray(dir: &Path, body: &str) -> PathBuf {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let path = dir.join("gray");
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o755)
            .open(&path)
            .unwrap();
        writeln!(f, "#!/bin/sh\nprintf '%s' '{body}'").unwrap();
        path
    }

    #[tokio::test]
    async fn run_returns_text_and_scopes_home() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_gray(
            dir.path(),
            "{\"ok\":true,\"result\":{\"kind\":\"text\",\"text\":\"pong\"}}",
        );
        let gray_home = dir.path().join("gray-home");
        std::fs::create_dir_all(&gray_home).unwrap();
        let config = serde_json::json!({
            "gray_bin": bin.to_string_lossy(),
            "gray_home": gray_home.to_string_lossy(),
        });
        let config_path = dir.path().join("config.json");
        let commands = Commands::new();
        let reply = commands
            .run(&config, &config_path, "chat:#c", "help", "")
            .await;
        match reply {
            Outcome::Text(t) => assert_eq!(t, "pong"),
            other => panic!("expected text: {other:?}"),
        }
        // The conversation home was created for the conversation-scoped run.
        let home = crate::session::conversation_home(&config_path, "chat:#c");
        assert!(home.is_dir());
    }

    #[tokio::test]
    async fn run_reports_garbage_output() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_gray(dir.path(), "gray: unrecognized command 'cmd'");
        let config = serde_json::json!({
            "gray_bin": bin.to_string_lossy(),
            "gray_home": dir.path().to_string_lossy(),
        });
        let commands = Commands::new();
        let reply = commands
            .run(&config, &dir.path().join("config.json"), "dm:n", "help", "")
            .await;
        match reply {
            Outcome::Text(t) => assert!(t.contains("may not support"), "{t}"),
            other => panic!("expected text: {other:?}"),
        }
    }
}
