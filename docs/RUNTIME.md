# Runtime notes

Requires a gray core implementing `gray -p --json` protocol 1 (the same
`{"protocol":1,"turn_id",...}` NDJSON the discord/slack adapters consume).

## Transport

Raw TCP, TLS via rustls + webpki roots (no openssl). `CAP LS 302`/`END`,
optional `PASS`, `NICK`, `USER`; waits for 001 with 432/433 nick-collision
retries (`nick_`, `nick_2`, …) and 464/465 rejection. `PING` is answered
while waiting and in the steady state. Optional NickServ `IDENTIFY` before
`JOIN`. Reconnects with bounded backoff (1s → 60s); a SIGTERM/SIGINT drains
notices and QUITs.

## Inbound policy

- The bot never answers itself; other CTCP than `ACTION` is ignored.
- Channels require addressing (`nick:` / `nick,` / `nick `); DMs are taken
  as-is. Only `allowed_users` nicks (case-insensitive) trigger a turn.
- An un-admitted DMer gets a pairing reply (code minted in SQLite);
  `gray irc pairing approve irc <code>` adds the nick.
- Conversation keys: `chat:<channel>` (shared channel transcript) or
  `dm:<nick>`. A channel shares one session; replies address the sender.
- Session reset policy `session_reset` (`both`: 1,440 idle minutes or the
  next 04:00 local boundary) mirrors the other adapters; `/new` and
  `/reset` force it early.
- `!name args` in a channel (no addressing needed) and `/name args`,
  `!name args`, or a bare single `name` in DM dispatch to `gray cmd run`
  in that conversation's home (GRAY-CMD-CONTRACT). `kind:"ui"` answers
  render through `fallback_text`; an unknown name falls back to a normal
  prompt. Commands spawn off the read loop and never enqueue a turn.

## Queue

Accepted messages commit to `irc.sqlite` next to the config before any
agent work: `queued` → `running` → `done`/`failed`/`cancelled`. Boot
re-queues interrupted `running` rows and reports the count in the startup
notice. `concurrency` workers (default 2, max 16) claim items; a
per-conversation flock serializes turns inside one chat. `queue list`
shows pending work; `queue cancel <id>` cancels what is still queued.

## Delivery

Replies go through the live connection's writer task. A generated answer
retries delivery (bounded) across a reconnect rather than regenerating —
delivery is at-least-once, never exactly-once. Markdown is flattened
(`**bold**` → `bold`, links → `text (url)`, fences dropped) and chunked to
400 bytes per PRIVMSG line with 300 ms pacing (flood protection).

`irc_send` is a standalone connect-register-send-quit under a
`<nick>-send` nick, not an outbox transaction — its errors must not be
blindly retried.

## Limits

`gray irc limits --timeout-seconds 600 --concurrency 2 --max-requests 200`
sets turn wall time, worker count, and per-turn provider requests; restart
to apply. `queue_capacity` (default 1,000) bounds the inbox; a full queue
tells the sender instead of dropping silently.

## Lifecycle notices

`lifecycle.json` + `restart_pending` next to the config, worded by
`gray gateway lifecycle boot|stop` when the installed gray has it (local
fallback otherwise). Chats with a running turn hear the shutdown/restart
notice over the still-open connection; the home channel hears the boot
notice on first connect. `"restart_notification": false` silences it.
