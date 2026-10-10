//! Doctor preflight checks with safe errors.
//! Every failure is a category string; config values never surface.
use serde_json::Value;

/// Production entry point: checks `config` locally, then registers a probe
/// nick on the live server (PASS is exercised; JOINs are not attempted).
pub async fn doctor(config: &Value) -> Result<(), String> {
    let gray_bin = config.get("gray_bin").and_then(Value::as_str).unwrap_or("");
    if !is_executable(gray_bin) {
        return Err("gray executable is missing or not executable".to_string());
    }
    let gray_home = config
        .get("gray_home")
        .and_then(Value::as_str)
        .unwrap_or("");
    let provider: Value = std::fs::read(std::path::Path::new(gray_home).join("config.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .ok_or_else(|| "gray provider configuration is missing or invalid".to_string())?;
    if provider
        .get("model")
        .and_then(Value::as_str)
        .is_none_or(str::is_empty)
    {
        return Err("gray has no configured model".to_string());
    }
    // Probe with a scratch nick so a running daemon keeps its own.
    let base: String = crate::irc::cfg_str(config, "nickname")
        .chars()
        .take(24)
        .collect();
    let base = if base.is_empty() { "gray-irc" } else { &base };
    let probe = format!("{base}-doc");
    let mut conn = crate::irc::Conn::connect(config, &probe).await?;
    conn.register(config, "gray irc doctor").await?;
    conn.quit("doctor done").await;
    println!("IRC connect, TLS handshake, registration and local gray configuration verified.");
    println!("Channel joins, NickServ identify and provider generation were not tested.");
    Ok(())
}

/// `os.access(path, X_OK)` equivalent: a real file with any exec bit set.
fn is_executable(path: &str) -> bool {
    if path.is_empty() {
        return false;
    }
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}
