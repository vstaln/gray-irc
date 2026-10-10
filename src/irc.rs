//! Minimal IRC client: raw TCP/TLS, CAP/NICK/USER, PING/PONG, JOIN, PRIVMSG.
//! Ported from hermes plugins/platforms/irc/adapter.py — no `irc` crate.
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

/// Any connected byte stream (plain TCP or TLS).
pub trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

/// A parsed IRC line: `:prefix COMMAND p1 p2 :trailing`.
#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub prefix: String,
    pub command: String,
    pub params: Vec<String>,
}

pub fn parse_line(raw: &str) -> Message {
    let raw = raw.trim_end_matches(['\r', '\n']);
    let (mut rest, mut prefix) = (raw, String::new());
    if let Some(stripped) = rest.strip_prefix(':') {
        let (p, r) = stripped.split_once(' ').unwrap_or((stripped, ""));
        prefix = p.to_string();
        rest = r;
    }
    let (head, trailing) = match rest.split_once(" :") {
        Some((h, t)) => (h, t.to_string()),
        None => (rest.trim_end_matches(':'), String::new()),
    };
    let mut parts = head.split_whitespace();
    let command = parts.next().unwrap_or("").to_ascii_uppercase();
    let mut params: Vec<String> = parts.map(str::to_string).collect();
    if !trailing.is_empty() {
        params.push(trailing);
    }
    Message {
        prefix,
        command,
        params,
    }
}

/// `nick!user@host` -> `nick`.
pub fn extract_nick(prefix: &str) -> &str {
    prefix.split('!').next().unwrap_or("")
}

/// Neutralise CR/LF (IRC command injection) and the illegal NUL.
pub fn strip_ctl(text: &str) -> String {
    text.replace(['\r', '\n'], " ").replace('\0', "")
}

fn truthy(s: &str) -> bool {
    matches!(s.to_ascii_lowercase().as_str(), "1" | "true" | "yes")
}

pub fn cfg_str<'a>(cfg: &'a Value, key: &str) -> &'a str {
    cfg.get(key).and_then(Value::as_str).unwrap_or("")
}

pub fn cfg_port(cfg: &Value) -> u16 {
    cfg.get("port")
        .and_then(Value::as_u64)
        .unwrap_or(6697)
        .min(65535) as u16
}

pub fn cfg_tls(cfg: &Value) -> bool {
    cfg.get("use_tls").and_then(Value::as_bool).unwrap_or(true)
}

/// Payload bytes left in a 510-byte line after `PRIVMSG <target> :` + CRLF,
/// capped at 400 per the port spec.
pub fn privmsg_budget(target: &str) -> usize {
    let overhead = "PRIVMSG ".len() + target.len() + " :".len() + 2;
    400usize.min(510usize.saturating_sub(overhead))
}

/// Split one line into UTF-8 chunks of at most `limit` bytes, preferring
/// space boundaries (hermes `_chunk_paragraph`).
fn chunk_paragraph(paragraph: &str, limit: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut rest = paragraph;
    while !rest.is_empty() {
        if rest.len() <= limit {
            chunks.push(rest.to_string());
            break;
        }
        // Largest char-boundary prefix that fits.
        let mut split_at = 0;
        for (i, _) in rest.char_indices() {
            if i > limit {
                break;
            }
            split_at = i;
        }
        if split_at == 0 {
            split_at = limit.min(rest.len());
        }
        if let Some(space) = rest[..split_at].rfind(' ') {
            if space > split_at / 3 {
                split_at = space;
            }
        }
        chunks.push(rest[..split_at].trim_end().to_string());
        rest = rest[split_at..].trim_start();
    }
    chunks
}

/// Markdown-free, per-line, byte-budgeted PRIVMSG bodies for `target`.
pub fn split_message(content: &str, target: &str) -> Vec<String> {
    let limit = privmsg_budget(target);
    let plain = strip_markdown(content);
    let mut lines = Vec::new();
    for paragraph in plain.split('\n') {
        let p = strip_ctl(paragraph);
        let p = p.trim_end();
        if p.is_empty() {
            continue;
        }
        lines.extend(chunk_paragraph(p, limit));
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// One open connection (registered or not).
pub struct Conn {
    reader: BufReader<tokio::io::ReadHalf<Box<dyn Io>>>,
    writer: tokio::io::WriteHalf<Box<dyn Io>>,
    /// The nick the server confirmed (may differ after 433 retries).
    pub nick: String,
}

impl Conn {
    /// TCP connect, TLS handshake when configured. No registration yet.
    pub async fn connect(cfg: &Value, nick: &str) -> Result<Conn, String> {
        let server = cfg_str(cfg, "server");
        if server.is_empty() {
            return Err("server is not configured".to_string());
        }
        let stream = tokio::time::timeout(
            Duration::from_secs(30),
            TcpStream::connect((server, cfg_port(cfg))),
        )
        .await
        .map_err(|_| "connect timed out".to_string())?
        .map_err(|e| format!("connect failed: {e}"))?;
        let io: Box<dyn Io> = if cfg_tls(cfg) {
            let mut roots = rustls::RootCertStore::empty();
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            let client = rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth();
            let connector = tokio_rustls::TlsConnector::from(Arc::new(client));
            let name = rustls::pki_types::ServerName::try_from(server.to_string())
                .map_err(|_| "invalid server name for TLS".to_string())?;
            let tls = connector
                .connect(name, stream)
                .await
                .map_err(|e| format!("TLS handshake failed: {e}"))?;
            Box::new(tls)
        } else {
            Box::new(stream)
        };
        let (read, write) = tokio::io::split(io);
        Ok(Conn {
            reader: BufReader::new(read),
            writer: write,
            nick: nick.to_string(),
        })
    }

    pub async fn send(&mut self, line: &str) -> Result<(), String> {
        let bytes = format!("{line}\r\n");
        self.writer
            .write_all(bytes.as_bytes())
            .await
            .map_err(|e| format!("write failed: {e}"))?;
        self.writer
            .flush()
            .await
            .map_err(|e| format!("write failed: {e}"))
    }

    /// One protocol line; Ok(None) on EOF.
    pub async fn read_msg(&mut self) -> Result<Option<Message>, String> {
        let mut buf = Vec::new();
        match self.reader.read_until(b'\n', &mut buf).await {
            Ok(0) => Ok(None),
            Ok(_) => Ok(Some(parse_line(&String::from_utf8_lossy(&buf)))),
            Err(e) => Err(format!("read failed: {e}")),
        }
    }

    /// CAP LS/END, optional PASS, NICK, USER; wait for 001 (30s).
    /// 432/433 nick collisions retry `nick_`, `nick_2`... (5 tries).
    /// 464/465 reject outright. Answers PING while waiting.
    pub async fn register(&mut self, cfg: &Value, realname: &str) -> Result<(), String> {
        self.send("CAP LS 302").await?;
        let password = cfg_str(cfg, "server_password");
        if !password.is_empty() {
            self.send(&format!("PASS {}", strip_ctl(password))).await?;
        }
        let base = self.nick.clone();
        self.send(&format!("NICK {}", self.nick)).await?;
        self.send(&format!("USER {} 0 * :{}", self.nick, strip_ctl(realname)))
            .await?;
        let mut attempts = 0u32;
        let wait = async {
            let mut cap_done = false;
            loop {
                let Some(msg) = self.read_msg().await? else {
                    return Err("server closed during registration".to_string());
                };
                match msg.command.as_str() {
                    "PING" => {
                        let token = msg.params.first().map(String::as_str).unwrap_or("");
                        self.send(&format!("PONG :{token}")).await?;
                    }
                    "CAP" if !cap_done => {
                        // We request nothing; end negotiation so the server
                        // proceeds with registration.
                        self.send("CAP END").await?;
                        cap_done = true;
                    }
                    "001" => {
                        if let Some(n) = msg.params.first() {
                            self.nick = n.clone();
                        }
                        return Ok(());
                    }
                    "432" | "433" => {
                        attempts += 1;
                        if attempts > 5 {
                            return Err("too many nick collisions".to_string());
                        }
                        self.nick = if attempts == 1 {
                            format!("{base}_")
                        } else {
                            format!("{base}_{attempts}")
                        };
                        self.send(&format!("NICK {}", self.nick)).await?;
                    }
                    "464" | "465" => {
                        return Err(format!("server rejected client ({})", msg.command));
                    }
                    _ => {}
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(30), wait)
            .await
            .map_err(|_| "registration timed out (no RPL_WELCOME)".to_string())?
    }

    /// Optional NickServ IDENTIFY, then JOIN every configured channel.
    pub async fn join(&mut self, cfg: &Value) -> Result<(), String> {
        let nickserv = cfg_str(cfg, "nickserv_password");
        if !nickserv.is_empty() {
            self.send(&format!(
                "PRIVMSG NickServ :IDENTIFY {}",
                strip_ctl(nickserv)
            ))
            .await?;
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        if let Some(channels) = cfg.get("channels").and_then(Value::as_array) {
            for ch in channels.iter().filter_map(Value::as_str) {
                self.send(&format!("JOIN {ch}")).await?;
            }
        }
        Ok(())
    }

    /// Send one logical message, chunked + paced (basic flood protection).
    pub async fn privmsg(&mut self, target: &str, text: &str) -> Result<(), String> {
        for line in split_message(text, target) {
            self.send(&format!("PRIVMSG {target} :{line}")).await?;
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        Ok(())
    }

    pub async fn quit(&mut self, msg: &str) {
        let _ = self.send(&format!("QUIT :{}", strip_ctl(msg))).await;
    }

    /// Two-task ownership after registration: the read loop owns `Reader`,
    /// the writer task owns `Writer`.
    pub fn split(self) -> (Reader, Writer) {
        (
            Reader {
                reader: self.reader,
            },
            Writer {
                writer: self.writer,
            },
        )
    }
}

/// The read half of a registered connection.
pub struct Reader {
    reader: BufReader<tokio::io::ReadHalf<Box<dyn Io>>>,
}

impl Reader {
    /// One protocol line; Ok(None) on EOF.
    pub async fn read_msg(&mut self) -> Result<Option<Message>, String> {
        let mut buf = Vec::new();
        match self.reader.read_until(b'\n', &mut buf).await {
            Ok(0) => Ok(None),
            Ok(_) => Ok(Some(parse_line(&String::from_utf8_lossy(&buf)))),
            Err(e) => Err(format!("read failed: {e}")),
        }
    }
}

/// The write half of a registered connection.
pub struct Writer {
    writer: tokio::io::WriteHalf<Box<dyn Io>>,
}

impl Writer {
    pub async fn send(&mut self, line: &str) -> Result<(), String> {
        let bytes = format!("{line}\r\n");
        self.writer
            .write_all(bytes.as_bytes())
            .await
            .map_err(|e| format!("write failed: {e}"))?;
        self.writer
            .flush()
            .await
            .map_err(|e| format!("write failed: {e}"))
    }

    /// Send one logical message, chunked + paced (basic flood protection).
    pub async fn privmsg(&mut self, target: &str, text: &str) -> Result<(), String> {
        for line in split_message(text, target) {
            self.send(&format!("PRIVMSG {target} :{line}")).await?;
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        Ok(())
    }

    pub async fn quit(&mut self, msg: &str) {
        let _ = self.send(&format!("QUIT :{}", strip_ctl(msg))).await;
    }
}

/// One-shot delivery used by `irc_send` and cron-style sends: register under
/// a `<base>-send` nick, JOIN the target if it is a channel, PRIVMSG, QUIT.
/// Works with or without the long-running daemon.
pub async fn standalone_send(cfg: &Value, target: &str, text: &str) -> Result<String, String> {
    if target.is_empty()
        || target
            .bytes()
            .any(|b| matches!(b, b'\r' | b'\n' | 0 | b' ' | b','))
    {
        return Err("target contains illegal IRC characters".to_string());
    }
    let base: String = cfg_str(cfg, "nickname")
        .trim_end_matches(['_', '-', '0', '1', '2', '3', '4', '5', '6', '7', '8', '9'])
        .chars()
        .take(24)
        .collect();
    let base = if base.is_empty() { "gray-irc" } else { &base };
    let nick = format!("{base}-send");
    let mut conn = Conn::connect(cfg, &nick).await?;
    conn.register(cfg, "gray irc agent").await?;
    if let Err(e) = conn.join(cfg).await {
        conn.quit("gray send done").await;
        return Err(e);
    }
    if target.starts_with(['#', '&', '+', '!']) {
        if let Err(e) = conn.send(&format!("JOIN {target}")).await {
            conn.quit("gray send done").await;
            return Err(e);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let result = conn.privmsg(target, text).await;
    conn.quit("gray send done").await;
    result.map(|()| format!("Sent to {target}."))
}

/// True when `v` names an env-style truthy flag ("1"/"true"/"yes").
#[allow(dead_code)]
pub fn env_truthy(v: &str) -> bool {
    truthy(v)
}

/// Convert basic markdown to plain text for IRC (hermes _MARKDOWN_RULES).
pub fn strip_markdown(text: &str) -> String {
    let rules: &[(&str, &str)] = &[
        (r"\*\*(.+?)\*\*", "$1"),
        (r"__(.+?)__", "$1"),
        (r"\*(.+?)\*", "$1"),
        (r"(?<!\w)_(.+?)_(?!\w)", "$1"),
        (r"`(.+?)`", "$1"),
        (r"!\[([^\]]*)\]\(([^)]+)\)", "$2"),
        (r"\[([^\]]+)\]\(([^)]+)\)", "$1 ($2)"),
    ];
    // Code fences first: drop the fence lines themselves, before the
    // inline-code rule can eat stray backticks out of them.
    let mut out = text
        .lines()
        .filter(|l| !l.trim_start().starts_with("```"))
        .collect::<Vec<_>>()
        .join("\n");
    for (pat, rep) in rules {
        if let Ok(re) = regex::Regex::new(pat) {
            out = re.replace_all(&out, *rep).into_owned();
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_privmsg() {
        let m = parse_line(":nick!u@h PRIVMSG #chan :hello there\r\n");
        assert_eq!(m.prefix, "nick!u@h");
        assert_eq!(m.command, "PRIVMSG");
        assert_eq!(m.params, vec!["#chan", "hello there"]);
    }

    #[test]
    fn parse_bare_command() {
        let m = parse_line("PING :token");
        assert_eq!(m.prefix, "");
        assert_eq!(m.command, "PING");
        assert_eq!(m.params, vec!["token"]);
    }

    #[test]
    fn nick_from_prefix() {
        assert_eq!(extract_nick("nick!u@h"), "nick");
        assert_eq!(extract_nick("nick"), "nick");
    }

    #[test]
    fn budget_counts_prefix() {
        assert_eq!(privmsg_budget("#a"), 400);
        let long = &"x".repeat(120);
        assert_eq!(privmsg_budget(long), 510 - 8 - 120 - 2 - 2);
    }

    #[test]
    fn chunking_respects_bytes() {
        let text = "word ".repeat(200);
        let chunks = split_message(&text, "#chan");
        assert!(chunks.len() > 1);
        assert!(chunks.iter().all(|c| c.len() <= privmsg_budget("#chan")));
        assert!(chunks.iter().all(|c| !c.is_empty()));
    }

    #[test]
    fn chunking_keeps_utf8() {
        let text = "é".repeat(300); // 2 bytes each
        let chunks = split_message(&text, "#c");
        for c in &chunks {
            assert!(c.len() <= 400);
        }
        assert_eq!(chunks.concat().len(), 600);
    }

    #[test]
    fn markdown_gone() {
        assert_eq!(strip_markdown("**bold** `code` [t](http://u)"), "bold code t (http://u)");
        assert_eq!(strip_markdown("```rust\ncode\n```"), "code");
    }

    #[test]
    fn ctl_stripped() {
        assert_eq!(strip_ctl("a\rb\nc\0d"), "a b cd");
    }
}
