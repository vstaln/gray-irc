//! NDJSON protocol 1.1 sidecar.
//! Manifest answers without config; delivery failures never echo secrets.
use serde_json::{json, Value};
use std::path::Path;
use std::sync::LazyLock;

/// Static manifest (`irc` 0.1.x, protocol 1.1, no commands).
pub static MANIFEST: LazyLock<Value> = LazyLock::new(|| {
    json!({
        "name": "irc",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": "1.1",
        "requires_plugins": ["gateway"],
        "commands": [],
        "hooks": ["prompt/context"],
        "tools": [{
            "name": "irc_send",
            "label": "IRC Send",
            "description": "Send text to an IRC channel or nick. Markdown is flattened and the text is chunked to the IRC line budget (400 bytes per line). Target defaults to the configured home channel.",
            "parameters": {
                "type": "object",
                "properties": {
                    "text": {"type": "string", "description": "Message text (markdown is flattened for IRC)"},
                    "target": {"type": "string", "description": "Channel (#chan) or nick; default: the configured home channel"}
                },
                "required": ["text"]
            }
        }]
    })
});

const PROMPT_CONTEXT: &str = "IRC tools: irc_send posts text to an IRC channel or nick (target defaults to the home channel). Markdown is flattened and long text is chunked to the IRC line limit; keep it plain — no embeds, no files. Errors say exactly what to fix; nothing is sent when a call is invalid. Never send secrets.";

fn is_error() -> Value {
    json!({
        "content": "IRC delivery failed. Run gray-irc doctor; do not blindly retry partial sends.",
        "is_error": true
    })
}

/// A specific, fixable failure. `detail` must never contain config values.
fn not_sent(detail: impl std::fmt::Display) -> Value {
    json!({
        "content": format!("Not sent: {detail}. Nothing reached IRC; fix it and call again."),
        "is_error": true
    })
}

/// Dispatch one protocol method. Never echoes config/password text:
/// failures describe the caller's own input or a sanitized reason only.
pub async fn dispatch(method: &str, params: &Value, config_path: &Path) -> Value {
    if method == "plugin/manifest" {
        return MANIFEST.clone();
    }
    if method == "prompt/context" {
        return json!({"text": PROMPT_CONTEXT});
    }
    if method == "tool/call" {
        let name = params.get("name").and_then(Value::as_str).unwrap_or("");
        return match name {
            "irc_send" => send_text(params, config_path).await,
            _ => is_error(),
        };
    }
    json!({"error": "Unsupported method"})
}

async fn send_text(params: &Value, config_path: &Path) -> Value {
    let args = params.get("args").cloned().unwrap_or(Value::Null);
    let text = args.get("text").and_then(Value::as_str).unwrap_or("");
    if text.trim().is_empty() {
        return not_sent("text is empty");
    }
    let config = match crate::config::load_config(config_path) {
        Ok(c) => c,
        Err(_) => return is_error(),
    };
    let fallback = config
        .get("home_channel")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| {
            config
                .get("channels")
                .and_then(Value::as_array)
                .and_then(|a| a.first())
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_default();
    let target = args
        .get("target")
        .and_then(Value::as_str)
        .filter(|t| !t.trim().is_empty())
        .unwrap_or(&fallback);
    if !crate::config::target_valid(&Value::String(target.to_string())) {
        return not_sent("target must be an IRC channel or nick");
    }
    match tokio::time::timeout(
        std::time::Duration::from_secs(60),
        crate::irc::standalone_send(&config, target, text),
    )
    .await
    {
        Ok(Ok(msg)) => json!({"content": msg}),
        Ok(Err(e)) => not_sent(e),
        Err(_) => is_error(),
    }
}

/// The sidecar loop: one JSON request per stdin line, one `{"id", "result"}`
/// reply per line on stdout. `plugin/shutdown` and EOF exit.
pub fn serve(config_path: &Path) -> ! {
    use std::io::{BufRead, Write};
    const LIMIT: usize = 256 * 1024;
    let stdin = std::io::stdin();
    let mut reader = stdin.lock();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("sidecar runtime builds");
    loop {
        let mut buf: Vec<u8> = Vec::new();
        let n = match reader.read_until(b'\n', &mut buf) {
            Ok(n) => n,
            Err(_) => std::process::exit(1),
        };
        if n == 0 {
            std::process::exit(0);
        }
        if buf.len() > LIMIT {
            std::process::exit(1);
        }
        let text = match String::from_utf8(buf) {
            Ok(s) => s,
            Err(_) => continue,
        };
        if text.trim().is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if !req.is_object() {
            continue;
        }
        if req.get("method").and_then(Value::as_str) == Some("plugin/shutdown") {
            std::process::exit(0);
        }
        let id = match req.get("id") {
            Some(Value::Number(_)) => req.get("id").cloned().unwrap(),
            _ => continue,
        };
        let method = req.get("method").and_then(Value::as_str).unwrap_or("");
        let params = match req.get("params") {
            Some(Value::Object(_)) => req.get("params").cloned().unwrap(),
            _ => json!({}),
        };
        let result = rt.block_on(dispatch(method, &params, config_path));
        let row = json!({"id": id, "result": result});
        if writeln!(out, "{row}").is_err() {
            std::process::exit(1);
        }
        if out.flush().is_err() {
            std::process::exit(1);
        }
    }
}
