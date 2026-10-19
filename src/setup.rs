//! Interactive setup wizard.
//! Credentials stay in the wizard; the model never receives the passwords.
use serde_json::{json, Value};
use std::path::Path;

/// Terminal IO. The real implementation talks to stdin/stderr; tests inject
/// a scripted fake. `print_line` goes to user-visible output (never logs).
pub trait Prompter {
    fn is_terminal(&self) -> bool;
    fn prompt(&mut self, text: &str) -> Result<String, String>;
    fn prompt_hidden(&mut self, text: &str) -> Result<String, String>;
    fn confirm(&mut self, text: &str) -> Result<bool, String>;
    fn print_line(&mut self, text: &str);
}

/// Real terminal prompter (hidden input via `stty -echo`, no new deps).
pub struct Tty;

impl Prompter for Tty {
    fn is_terminal(&self) -> bool {
        use std::io::IsTerminal;
        std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
    }
    fn prompt(&mut self, text: &str) -> Result<String, String> {
        use std::io::Write;
        let _ = std::io::stderr().write_all(text.as_bytes());
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .map_err(|_| "cannot read terminal input".to_string())?;
        Ok(line.trim_end_matches(['\n', '\r']).to_string())
    }
    fn prompt_hidden(&mut self, text: &str) -> Result<String, String> {
        use std::io::Write;
        let _ = std::io::stderr().write_all(text.as_bytes());
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        let res = without_echo(|| std::io::stdin().read_line(&mut line));
        let _ = std::io::stderr().write_all(b"\n");
        res.map_err(|_| "cannot read terminal input".to_string())?;
        Ok(line.trim().to_string())
    }
    fn confirm(&mut self, text: &str) -> Result<bool, String> {
        let answer = self.prompt(text)?.trim().to_ascii_lowercase();
        Ok(answer == "y" || answer == "yes")
    }
    fn print_line(&mut self, text: &str) {
        eprintln!("{text}");
    }
}

/// Echo off for one read. Ctrl-C would otherwise kill setup with the
/// terminal still silent, so SIGINT restores it first (tcsetattr is
/// async-signal-safe).
#[cfg(unix)]
fn without_echo<T>(read: impl FnOnce() -> T) -> T {
    static SAVED: std::sync::OnceLock<libc::termios> = std::sync::OnceLock::new();
    extern "C" fn restore_and_exit(_: libc::c_int) {
        if let Some(t) = SAVED.get() {
            unsafe { libc::tcsetattr(0, libc::TCSANOW, t) };
        }
        unsafe { libc::_exit(130) };
    }
    let mut term: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(0, &mut term) } != 0 {
        return read();
    }
    let saved = *SAVED.get_or_init(|| term);
    let mut quiet = saved;
    quiet.c_lflag &= !libc::ECHO;
    let handler = restore_and_exit as extern "C" fn(libc::c_int) as libc::sighandler_t;
    let old = unsafe {
        libc::tcsetattr(0, libc::TCSANOW, &quiet);
        libc::signal(libc::SIGINT, handler)
    };
    let out = read();
    unsafe {
        libc::tcsetattr(0, libc::TCSANOW, &saved);
        libc::signal(libc::SIGINT, old);
    }
    out
}

/// Windows echo off for one read: clear ENABLE_ECHO_INPUT on the console
/// input buffer. A console control handler puts the mode back before the
/// default Ctrl-C processing kills setup, same shape as the unix version.
#[cfg(windows)]
fn without_echo<T>(read: impl FnOnce() -> T) -> T {
    use std::ffi::c_void;
    type Bool = i32;
    type Dword = u32;
    type Handle = *mut c_void;
    const STD_INPUT_HANDLE: Dword = -10i32 as Dword;
    const ENABLE_ECHO_INPUT: Dword = 0x0004;
    extern "system" {
        fn GetStdHandle(which: Dword) -> Handle;
        fn GetConsoleMode(handle: Handle, mode: *mut Dword) -> Bool;
        fn SetConsoleMode(handle: Handle, mode: Dword) -> Bool;
        fn SetConsoleCtrlHandler(
            handler: Option<extern "system" fn(Dword) -> Bool>,
            add: Bool,
        ) -> Bool;
    }
    static SAVED: std::sync::OnceLock<Dword> = std::sync::OnceLock::new();
    extern "system" fn restore(_: Dword) -> Bool {
        if let Some(&mode) = SAVED.get() {
            unsafe {
                SetConsoleMode(GetStdHandle(STD_INPUT_HANDLE), mode);
            }
        }
        0
    }
    let input = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    if input.is_null() {
        return read();
    }
    let mut mode: Dword = 0;
    if unsafe { GetConsoleMode(input, &mut mode) } == 0 {
        return read();
    }
    let saved = *SAVED.get_or_init(|| mode);
    unsafe {
        SetConsoleMode(input, saved & !ENABLE_ECHO_INPUT);
        SetConsoleCtrlHandler(Some(restore), 1);
    }
    let out = read();
    unsafe {
        SetConsoleMode(input, saved);
        SetConsoleCtrlHandler(Some(restore), 0);
    }
    out
}

#[cfg(not(any(unix, windows)))]
fn without_echo<T>(read: impl FnOnce() -> T) -> T {
    read()
}

/// Resolve the gray binary (`GRAY_BIN` env or `PATH` lookup).
fn resolve_gray(explicit: &str) -> Option<String> {
    let candidate = explicit.trim();
    if candidate.is_empty() {
        return None;
    }
    if candidate.contains('/') || candidate.contains('\\') {
        let path = Path::new(candidate);
        if path.is_file() && is_executable(path) {
            return Some(candidate.to_string());
        }
        return None;
    }
    // Executables carry extensions on Windows (`gray` is `gray.exe`).
    let names: Vec<String> = {
        #[cfg(windows)]
        {
            [".exe", ".cmd", ".bat", ".com", ".ps1", ""]
                .iter()
                .map(|ext| format!("{candidate}{ext}"))
                .collect()
        }
        #[cfg(not(windows))]
        {
            vec![candidate.to_string()]
        }
    };
    let path_var = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path_var) {
        for name in &names {
            let full = dir.join(name);
            if full.is_file() && is_executable(&full) {
                return Some(full.to_string_lossy().into_owned());
            }
        }
    }
    None
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

fn absolutize(path: &Path) -> std::path::PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}

fn shellexpand_home(raw: &str) -> String {
    if raw == "~" || raw.starts_with("~/") {
        if let Some(home) = crate::config::user_home() {
            return if raw == "~" {
                home.to_string_lossy().into_owned()
            } else {
                home.join(&raw[2..]).to_string_lossy().into_owned()
            };
        }
    }
    raw.to_string()
}

/// Split a comma-separated prompt answer into trimmed non-empty entries.
fn csv(answer: &str) -> Vec<String> {
    answer
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Dedupe keeping order, case-insensitively (nicks/channels).
fn merge(prior: Vec<String>, extra: &[String]) -> Vec<String> {
    let mut out = prior;
    for e in extra {
        if !out.iter().any(|u| u.eq_ignore_ascii_case(e)) {
            out.push(e.clone());
        }
    }
    out
}

/// The wizard: connection details, optional passwords, the allowlist.
/// A live registration proves the answers before anything is called done.
pub async fn run(path: &Path, io: &mut dyn Prompter) -> Result<bool, String> {
    if !io.is_terminal() {
        return Err("Setup needs a terminal for hidden password input".to_string());
    }
    if path.exists() && !io.confirm("Replace existing configuration? [y/N] ")? {
        return Ok(false);
    }
    // Gray's binary and home are resolved silently (env, then the
    // conventional defaults) — derived facts, not questions.
    let gray = resolve_gray(&std::env::var("GRAY_BIN").unwrap_or_else(|_| "gray".to_string()))
        .ok_or_else(|| "Install gray first; executable not found".to_string())?;
    let gray_home = absolutize(Path::new(&shellexpand_home(
        &std::env::var("GRAY_HOME").unwrap_or_else(|_| "~/.gray".to_string()),
    )));

    // Whoever the old config allowed stays allowed: the wizard only adds.
    let prior = std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .filter(Value::is_object);
    let prior_allowed: Vec<String> = prior
        .as_ref()
        .and_then(|p| p.get("allowed_users"))
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    for line in [
        "Connect gray to an IRC network (Libera.Chat, OFTC, your own bouncer).",
        "A probe registration against the live server checks the answers.",
    ] {
        io.print_line(line);
    }

    let server = io.prompt("IRC server hostname (e.g. irc.libera.chat): ")?;
    let server = server.trim().to_string();
    if server.is_empty() || server.bytes().any(|b| !b.is_ascii_graphic()) {
        return Err("Server hostname is required (no spaces)".to_string());
    }

    let tls_answer = io.prompt("Use TLS (recommended)? [Y/n] ")?;
    let use_tls = matches!(tls_answer.trim().to_lowercase().as_str(), "" | "y" | "yes");
    let default_port: u64 = if use_tls { 6697 } else { 6667 };
    let port_answer = io.prompt(&format!("Port [{default_port}]: "))?;
    let port: u64 = if port_answer.trim().is_empty() {
        default_port
    } else {
        port_answer
            .trim()
            .parse()
            .map_err(|_| "Port must be a number between 1 and 65535".to_string())?
    };
    if !(1..=65535).contains(&port) {
        return Err("Port must be a number between 1 and 65535".to_string());
    }

    let nickname = io.prompt("Bot nickname (e.g. gray-bot): ")?;
    let nickname = nickname.trim().to_string();
    if nickname.is_empty()
        || nickname.len() > 30
        || !nickname.bytes().all(|b| b.is_ascii_graphic())
    {
        return Err("Nickname must be 1-30 non-space characters".to_string());
    }

    let channels_answer = io.prompt("Channels to join, comma-separated (e.g. #gray): ")?;
    let channels = csv(&channels_answer);
    if channels.is_empty()
        || !channels
            .iter()
            .all(|c| crate::config::target_valid(&Value::String(c.clone())) && c.starts_with(['#', '&', '+', '!']))
    {
        return Err("Channels must be IRC channel names (#chan), comma-separated".to_string());
    }

    io.print_line("Optional authentication — leave blank to skip.");
    let server_password = if io.confirm("Configure a server password (PASS command)? [y/N] ")? {
        io.prompt_hidden("Server password: ")?
    } else {
        String::new()
    };
    let nickserv_password = if io.confirm("Identify with NickServ on connect? [y/N] ")? {
        io.prompt_hidden("NickServ password: ")?
    } else {
        String::new()
    };

    io.print_line(
        "Access control: only allowlisted nicks can trigger the bot. Nicks are \
         claimed, not authenticated — anyone else who DMs it gets a pairing code \
         the owner can approve.",
    );
    let allowed_answer = io.prompt(
        "Allowed nicks (comma-separated; first is usually yours — empty admits nobody): ",
    )?;
    let allowed = merge(prior_allowed, &csv(&allowed_answer));

    let default_home = channels[0].clone();
    let home_answer = io.prompt(&format!("Home channel for cron/notices [{default_home}]: "))?;
    let home_channel = if home_answer.trim().is_empty() {
        default_home
    } else {
        let h = home_answer.trim().to_string();
        if !crate::config::target_valid(&Value::String(h.clone())) {
            return Err("Home channel must be an IRC channel or nick".to_string());
        }
        h
    };

    // `path.parent` is the state dir; like the other adapters, the saved
    // config holds absolute paths or `save_config` validation rejects it.
    let workdir = path
        .parent()
        .map(|p| absolutize(p).to_string_lossy().into_owned())
        .unwrap_or_else(|| ".".to_string());
    let mut saved = json!({
        "server": server,
        "port": port,
        "use_tls": use_tls,
        "nickname": nickname,
        "channels": channels,
        "home_channel": home_channel,
        "allowed_users": allowed,
        "gray_bin": absolutize(Path::new(&gray)).to_string_lossy(),
        "gray_home": gray_home.to_string_lossy(),
        "workdir": workdir,
        "session_reset": {
            "mode": "both",
            "idle_minutes": 1_440,
            "at_hour": 4
        }
    });
    if !server_password.is_empty() {
        saved["server_password"] = Value::String(server_password);
    }
    if !nickserv_password.is_empty() {
        saved["nickserv_password"] = Value::String(nickserv_password);
    }
    // A re-run replaces only what the wizard asked about: limits and other
    // hand-tuned keys survive (unless they no longer validate).
    if let Some(mut merged) = prior {
        for (k, v) in saved.as_object().into_iter().flatten() {
            if k != "session_reset" || merged.get(k).is_none() {
                merged[k] = v.clone();
            }
        }
        if crate::config::validate_config(&merged).is_ok() {
            saved = merged;
        }
    }
    crate::config::save_config(path, &saved)?;
    io.print_line("Configuration saved privately.");

    // Prove it with a live registration before anyone is told it worked.
    crate::doctor::doctor(&saved).await.map_err(|e| {
        format!("Saved, but the doctor disagreed: {e}. Fix it, then run `gray irc doctor` and `gray irc install`.")
    })?;
    io.print_line("Doctor verified the configuration.");
    Ok(true)
}
