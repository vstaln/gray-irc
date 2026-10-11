//! The IRC daemon: TCP/TLS connect, CAP/NICK/USER, JOIN, PRIVMSG in ->
//! durable inbox -> isolated gray turns -> chunked PRIVMSG replies out.
//!
//! Admission mirrors the Slack bridge: ourselves never turn; the sender's
//! nick must be on `allowed_users`; a channel message must address the bot
//! (`nick:`/`nick,`/`nick `). An un-admitted DMer gets a pairing reply
//! instead of silence.
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

use crate::irc::{Conn, Message, Writer};
use crate::runner::RunError;
use crate::store::Store;

/// The queue file next to the config.
pub fn open_store(config_path: &Path) -> Result<Store, String> {
    Store::new(&Store::default_path(config_path))
}

/// One outbound work item for the writer task. `Tell` acknowledges the
/// delivery so a dead connection re-queues instead of losing the reply.
enum Out {
    /// A raw protocol line (PONG), fire and forget.
    Raw(String),
    /// A PRIVMSG the sender does not wait on (notices, pairing replies).
    Say { target: String, text: String },
    /// A PRIVMSG whose sender waits for the write to land.
    Tell {
        target: String,
        text: String,
        ack: oneshot::Sender<Result<(), String>>,
    },
}

fn is_channel(target: &str) -> bool {
    target.starts_with(['#', '&', '+', '!'])
}

/// Owns the write half; drains the queue then QUITs and exits. A send
/// failure drops every pending `Tell` ack, which re-queues its item.
async fn writer_task(mut w: Writer, mut rx: mpsc::Receiver<Out>) {
    while let Some(o) = rx.recv().await {
        let (res, ack) = match o {
            Out::Raw(line) => (w.send(&line).await, None),
            Out::Say { target, text } => (w.privmsg(&target, &text).await, None),
            Out::Tell { target, text, ack } => (w.privmsg(&target, &text).await, Some(ack)),
        };
        let failed = res.is_err();
        if let Some(ack) = ack {
            let _ = ack.send(res);
        }
        if failed {
            break;
        }
    }
    w.quit("gray").await;
}

struct Daemon {
    config: Value,
    config_path: PathBuf,
    store: Store,
    /// The live connection's outbound queue; `None` while reconnecting.
    out: std::sync::Mutex<Option<mpsc::Sender<Out>>>,
    /// `!cmd`/`/cmd` registry + runner (gray cmd, GRAY-CMD-CONTRACT).
    commands: Arc<crate::commands::Commands>,
}

impl Daemon {
    fn home_channel(&self) -> String {
        self.config
            .get("home_channel")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| {
                self.config
                    .get("channels")
                    .and_then(Value::as_array)
                    .and_then(|a| a.first())
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_default()
    }

    fn out_tx(&self) -> Option<mpsc::Sender<Out>> {
        self.out.lock().ok().and_then(|g| g.clone())
    }

    /// Nicks compare case-insensitively (RFC: ASCII case mapping).
    fn user_admitted(&self, nick: &str) -> bool {
        !nick.is_empty()
            && self
                .config
                .get("allowed_users")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .any(|u| u.eq_ignore_ascii_case(nick))
                })
                .unwrap_or(false)
    }

    /// One inbound PRIVMSG: CTCP, addressing, admission, commands, enqueue.
    async fn on_privmsg(
        &self,
        out: &mpsc::Sender<Out>,
        own_nick: &str,
        sender: &str,
        target: &str,
        text: &str,
    ) {
        if sender.is_empty() || sender.eq_ignore_ascii_case(own_nick) {
            return;
        }
        let mut text = text.to_string();
        // CTCP: /me actions read as prose; everything else (VERSION, DCC,
        // floods) is dropped.
        if let Some(inner) = text
            .strip_prefix('\x01')
            .and_then(|s| s.strip_suffix('\x01'))
        {
            if let Some(action) = inner.strip_prefix("ACTION ") {
                text = format!("* {sender} {}", action.trim());
            } else {
                return;
            }
        }
        let chan = is_channel(target);
        // True only for an unaddressed channel `!…`; still gated below, and
        // dropped rather than prompted if it parses to no known command.
        let mut bang = false;
        if chan {
            // In a channel the bot only answers when addressed — except a
            // `!command`, which is self-addressing by convention.
            let mut addressed = None;
            for sep in [':', ',', ' '] {
                let prefix = format!("{own_nick}{sep}");
                if text.len() > prefix.len() && text[..prefix.len()].eq_ignore_ascii_case(&prefix) {
                    addressed = Some(text[prefix.len()..].trim().to_string());
                    break;
                }
            }
            match addressed {
                Some(t) if !t.is_empty() => text = t,
                _ if text.trim_start().starts_with('!') => bang = true,
                _ => return,
            }
        }
        let reply_target = if chan { target } else { sender };

        if !self.user_admitted(sender) {
            // Only a DM hears how to get in; channel strangers get silence.
            if let Some(reply) = crate::pairing::reply_for(&self.store, sender, !chan) {
                let _ = out
                    .send(Out::Say {
                        target: sender.to_string(),
                        text: reply.text,
                    })
                    .await;
            }
            return;
        }

        let conversation = crate::session::conversation_key(target, sender, chan);

        // Bare commands never reach the model.
        if matches!(text.trim(), "/new" | "/reset") {
            let home = crate::session::conversation_home(&self.config_path, &conversation);
            let note = match crate::session::reset_home(&home, crate::store::now_secs()) {
                Ok(()) => "Session reset — the next message starts fresh.",
                Err(_) => "Could not reset this session.",
            };
            let _ = out
                .send(Out::Say {
                    target: reply_target.to_string(),
                    text: note.to_string(),
                })
                .await;
            return;
        }

        // `!cmd`/`/cmd` headless commands answer inline via `gray cmd`;
        // they never enqueue a turn. The reply rides the outbound queue
        // (chunked to IRC's line limit) from a spawned task so a slow
        // command cannot stall the read loop.
        if let Some((name, args)) = self.commands.parse(&self.config, &text, !chan) {
            let out = out.clone();
            let commands = Arc::clone(&self.commands);
            let config = self.config.clone();
            let config_path = self.config_path.clone();
            let conversation = conversation.clone();
            let target = reply_target.to_string();
            let mention = if chan {
                format!("{sender}: ")
            } else {
                String::new()
            };
            tokio::spawn(async move {
                let reply = commands
                    .run(&config, &config_path, &conversation, &name, &args)
                    .await;
                let _ = out
                    .send(Out::Say {
                        target,
                        text: format!("{mention}{reply}"),
                    })
                    .await;
            });
            return;
        }
        if bang {
            return;
        }

        let capacity = self
            .config
            .get("queue_capacity")
            .and_then(Value::as_u64)
            .unwrap_or(1000);
        match self
            .store
            .enqueue(reply_target, sender, &conversation, &text, capacity)
        {
            Ok(id) => eprintln!("[irc] queued {id} from {sender} in {reply_target}"),
            Err(e) => {
                let _ = out
                    .send(Out::Say {
                        target: reply_target.to_string(),
                        text: format!("Queue full ({e}); try again later."),
                    })
                    .await;
            }
        }
    }

    /// One claimed inbox item end to end. A dead connection re-queues;
    /// a generated answer retries delivery instead of regenerating.
    async fn work(&self) -> Result<bool, String> {
        let Some(item) = self.store.claim()? else {
            return Ok(false);
        };
        // Do not spend a turn while disconnected.
        if self.out_tx().is_none() {
            self.store.requeue(&item.id)?;
            tokio::time::sleep(Duration::from_millis(500)).await;
            return Ok(true);
        }
        let outcome = crate::runner::run_gray(
            &self.config,
            &self.config_path,
            &item.conversation,
            &item.text,
            None,
        )
        .await;
        match outcome {
            Ok(answer) => {
                // Channel replies are addressed to the sender, DMs are bare.
                let text = if is_channel(&item.target) {
                    format!("{}: {answer}", item.sender)
                } else {
                    answer.clone()
                };
                let mut attempts = 0u32;
                loop {
                    let Some(out) = self.out_tx() else {
                        attempts += 1;
                        if attempts > 5 {
                            self.store.fail(&item.id, "delivery failed: not connected")?;
                            break;
                        }
                        tokio::time::sleep(Duration::from_secs(2)).await;
                        continue;
                    };
                    let (tx, rx) = oneshot::channel();
                    let sent = out
                        .send(Out::Tell {
                            target: item.target.clone(),
                            text: text.clone(),
                            ack: tx,
                        })
                        .await
                        .is_ok()
                        && matches!(rx.await, Ok(Ok(())));
                    if sent {
                        self.store.complete(&item.id, &answer)?;
                        break;
                    }
                    attempts += 1;
                    if attempts > 5 {
                        self.store.fail(&item.id, "delivery failed: connection lost")?;
                        break;
                    }
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
            Err(RunError::Busy) => {
                self.store.requeue(&item.id)?;
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            Err(e) => {
                let note = if is_channel(&item.target) {
                    format!("{}: {e}", item.sender)
                } else {
                    e.to_string()
                };
                if let Some(out) = self.out_tx() {
                    let _ = out
                        .send(Out::Say {
                            target: item.target.clone(),
                            text: note,
                        })
                        .await;
                }
                self.store.fail(&item.id, &e.to_string())?;
            }
        }
        Ok(true)
    }
}

/// One connection: register, split, run read/write until EOF, a writer
/// failure, or shutdown. `notice` is posted to the home channel once.
async fn session(
    daemon: &Daemon,
    shutdown: &tokio::sync::Notify,
    notice: &mut Option<String>,
) -> Result<(), String> {
    let nick = crate::irc::cfg_str(&daemon.config, "nickname").to_string();
    let mut conn = Conn::connect(&daemon.config, &nick).await?;
    conn.register(&daemon.config, "gray irc agent").await?;
    conn.join(&daemon.config).await?;
    let mut own_nick = conn.nick.clone();
    eprintln!("[irc] registered as {own_nick}");
    let (mut reader, writer) = conn.split();
    let (tx, rx) = mpsc::channel::<Out>(256);
    if let Ok(mut slot) = daemon.out.lock() {
        *slot = Some(tx.clone());
    }
    let mut wtask = tokio::spawn(writer_task(writer, rx));
    // Whatever way this session ends, no worker may keep a dead sender.
    struct ClearOut<'a>(&'a std::sync::Mutex<Option<mpsc::Sender<Out>>>);
    impl Drop for ClearOut<'_> {
        fn drop(&mut self) {
            if let Ok(mut slot) = self.0.lock() {
                *slot = None;
            }
        }
    }
    let _clear = ClearOut(&daemon.out);

    if let Some(text) = notice.take() {
        let home = daemon.home_channel();
        if !home.is_empty() {
            let _ = tx.send(Out::Say { target: home, text }).await;
        }
    }

    let result: Result<(), String> = loop {
        tokio::select! {
            msg = reader.read_msg() => {
                match msg {
                    Ok(Some(m)) => on_line(daemon, &tx, &mut own_nick, &m).await,
                    Ok(None) => break Err("server closed the connection".to_string()),
                    Err(e) => break Err(e),
                }
            }
            _ = &mut wtask => break Err("connection write failed".to_string()),
            _ = shutdown.notified() => {
                // Goodbye notices ride the live connection, bounded.
                let dir = config_dir(&daemon.config_path);
                let gray_bin = Path::new(crate::irc::cfg_str(&daemon.config, "gray_bin"));
                let goodbye = crate::lifecycle::stop_via(gray_bin, &dir).await;
                if crate::lifecycle::enabled(&daemon.config) {
                    let running: Vec<String> = daemon
                        .store
                        .running_targets()
                        .unwrap_or_default()
                        .into_iter()
                        .map(|i| i.target)
                        .collect();
                    let targets = crate::lifecycle::shutdown_targets(
                        &running,
                        &daemon.home_channel(),
                        &goodbye,
                    );
                    let send = async {
                        for (target, text) in targets {
                            let _ = tx
                                .send(Out::Say {
                                    target,
                                    text: text.to_string(),
                                })
                                .await;
                        }
                    };
                    let _ = tokio::time::timeout(Duration::from_secs(4), send).await;
                }
                // Close the queue so the writer drains and QUITs.
                if let Ok(mut slot) = daemon.out.lock() {
                    *slot = None;
                }
                drop(tx);
                // A moment for the writer to drain the notices and QUIT.
                let _ = tokio::time::timeout(Duration::from_secs(2), &mut wtask).await;
                break Ok(());
            }
        }
    };
    result
}

/// One protocol line on the read side.
async fn on_line(daemon: &Daemon, tx: &mpsc::Sender<Out>, own_nick: &mut String, msg: &Message) {
    match msg.command.as_str() {
        "PING" => {
            let token = msg.params.first().map(String::as_str).unwrap_or("");
            let _ = tx.send(Out::Raw(format!("PONG :{token}"))).await;
        }
        "PRIVMSG" => {
            let sender = crate::irc::extract_nick(&msg.prefix).to_string();
            let target = msg.params.first().map(String::as_str).unwrap_or("");
            let text = msg.params.get(1).map(String::as_str).unwrap_or("");
            if target.is_empty() || text.is_empty() {
                return;
            }
            daemon
                .on_privmsg(tx, own_nick, &sender, target, text)
                .await;
        }
        // Track forced nick changes so addressing stays exact.
        "NICK" => {
            let old = crate::irc::extract_nick(&msg.prefix);
            if old.eq_ignore_ascii_case(own_nick) {
                if let Some(new) = msg.params.first() {
                    *own_nick = new.clone();
                }
            }
        }
        "ERROR" => {
            let detail = msg.params.first().map(String::as_str).unwrap_or("");
            eprintln!("[irc] server error: {detail}");
        }
        _ => {}
    }
}

fn config_dir(config_path: &Path) -> PathBuf {
    config_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf()
}

/// The daemon: lock + pidfile, workers, reconnect loop, graceful stop.
pub async fn run(config_path: &Path) -> Result<(), String> {
    let dir = config_dir(config_path);
    std::fs::DirBuilder::new()
        .recursive(true)
        .create(&dir)
        .map_err(|_| "cannot create config directory".to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }

    // daemon.json: `status` and lifecycle read it; removed on every exit.
    let pid_path = dir.join("daemon.json");
    {
        let stamp = serde_json::json!({
            "pid": std::process::id(),
            "started_at": crate::store::now_secs()
        });
        let _ = crate::config::atomic_json(&pid_path, &stamp);
    }
    struct RemovePid(PathBuf);
    impl Drop for RemovePid {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let _remove_pid = RemovePid(pid_path);

    // One daemon per config: two would fight over the same nick.
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("gateway.lock"))
        .map_err(|_| "cannot open gateway lock".to_string())?;
    use std::os::unix::io::AsRawFd;
    let locked = unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
    if !locked {
        return Err("Another gateway is running".to_string());
    }
    let _lock = lock_file;

    let config = crate::config::load_config(config_path)?;
    let store = open_store(config_path)?;
    let interrupted = store.recover().unwrap_or(0);

    let daemon = Arc::new(Daemon {
        config: config.clone(),
        config_path: config_path.to_path_buf(),
        store,
        out: std::sync::Mutex::new(None),
        commands: Arc::new(crate::commands::Commands::new()),
    });

    // Boot notice is computed now (it records lifecycle state) and posted
    // on the first successful connection.
    let gray_bin = Path::new(crate::irc::cfg_str(&daemon.config, "gray_bin"));
    let mut notice = if crate::lifecycle::enabled(&daemon.config) {
        crate::lifecycle::boot_via(gray_bin, &dir, interrupted).await
    } else {
        let _ = crate::lifecycle::boot(&dir);
        None
    };

    // Workers drain the inbox; the read loop only ever enqueues.
    let workers = daemon
        .config
        .get("concurrency")
        .and_then(Value::as_u64)
        .unwrap_or(2)
        .clamp(1, 16);
    let stop = Arc::new(tokio::sync::Notify::new());
    let mut handles = Vec::new();
    for _ in 0..workers {
        let d = daemon.clone();
        let stop = stop.clone();
        handles.push(tokio::spawn(async move {
            loop {
                match d.work().await {
                    Ok(true) => continue,
                    Ok(false) => {}
                    Err(e) => eprintln!("[irc] worker error: {e}"),
                }
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(500)) => {},
                    _ = stop.notified() => break,
                }
            }
        }));
    }

    let shutdown = Arc::new(tokio::sync::Notify::new());
    {
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            let mut term =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("SIGTERM handler");
            let mut int = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
                .expect("SIGINT handler");
            tokio::select! {
                _ = term.recv() => {},
                _ = int.recv() => {},
            }
            shutdown.notify_waiters();
        });
    }

    let mut backoff = Duration::from_secs(1);
    'outer: loop {
        match session(&daemon, &shutdown, &mut notice).await {
            Ok(()) => break 'outer,
            Err(e) => {
                eprintln!("[irc] {e}; reconnecting in {}s", backoff.as_secs());
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {},
                    _ = shutdown.notified() => break 'outer,
                }
                backoff = (backoff * 2).min(Duration::from_secs(60));
            }
        }
    }

    eprintln!("[irc] shutting down");
    stop.notify_waiters();
    for h in handles {
        h.abort();
    }
    // If shutdown never ran inside `session` (no live connection), still
    // record a clean exit so the next boot does not report a crash.
    let dir = config_dir(config_path);
    if std::fs::read(dir.join(crate::lifecycle::STATE_FILE))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|s| s.get("state").and_then(Value::as_str).map(str::to_string))
        .as_deref()
        == Some("running")
    {
        let restart = crate::lifecycle::restart_requested(&dir);
        crate::lifecycle::mark_stopped(&dir, restart);
    }
    Ok(())
}
