# gray-irc

A standalone IRC channel adapter for [gray](https://github.com/vstaln/gray)
— raw TCP/TLS in, chunked PRIVMSG replies out, with its own setup wizard,
pairing allowlist, durable SQLite queue, and a background service. Same
shape as `gray-discord-plugin`/`gray-slack`, IRC transport underneath
(ported from hermes `plugins/platforms/irc` — no `irc` crate).

**Status: v1.** Inbound PRIVMSGs from allowlisted nicks run one isolated
`gray` turn per conversation; outbound is plain chunked PRIVMSG. See
[runtime policy and limits](docs/RUNTIME.md).

## Requirements

- `gray` on PATH, provider/model configured in `~/.gray/config.json`
- An IRC network: Libera.Chat, OFTC, your own ZNC/bouncer/InspIRCd
- A nick for the bot (registered with NickServ if the network requires it)

## Install

```sh
cargo install --path .          # puts gray-irc on PATH
gray plugin install irc         # or let gray find the binary
```

## Setup

```sh
gray irc setup                  # or: gray gateway setup irc
```

The wizard asks for:

- server hostname, TLS (default yes → 6697, no → 6667), port
- bot nickname and channels to join (comma-separated)
- optional server password (`PASS`) and NickServ password
  (`IDENTIFY` on connect), entered hidden
- nicks allowed to trigger the bot
- home channel for cron notices (default: first channel)

It saves the config privately (`~/.config/gray-irc/config.json`, mode
0600), registers the `irc_send` tool with gray, probes a live registration
(doctor), and offers to install the background service.

## Running

```sh
gray irc status      # service health
gray irc stop        # graceful stop (posts shutdown notices)
gray irc restart
gray irc uninstall   # service only; config and sessions stay
gray irc run         # foreground daemon (what the service execs)
gray irc doctor      # verify registration + local gray config
gray irc queue list / cancel <id>
gray irc allowlist add|remove|list
gray irc pairing approve irc <CODE>
gray irc limits --timeout-seconds 600 --concurrency 2 --max-requests 200
```

The service probes what this box actually supervises with (runit,
`systemd --user`, or a detached pidfile daemon when no init is available).

## Talking to it

- DM the bot, or address it in a joined channel: `botnick: your message`
  (`botnick,` and `botnick ` work too). `/me` actions read as
  `* nick action`.
- Channel replies are addressed back (`younick: answer`); DMs are bare.
- `/new` or `/reset` in a chat starts a fresh session.
- `!steer <text>` adds guidance to the turn already running in that conversation ("Steering the current turn."); with nothing running it is sent as a new message.
- `!stop` (or `/stop`) cancels the turn running in that conversation ("Stopped."); queued messages still run next. With nothing running it answers "Nothing running."
- Messages sent while a turn runs queue as their own next turn ("Queued — N ahead.", at most once per 30s). Set `busy_input_mode: "steer"` in the config to steer the running turn instead.
- Long command output is paged: page 1 ends with `page 1/N — !page 2`; `!page N` shows page N of the conversation's last paged output.
- `!<cmd> args` in a channel or `/<cmd> args` (bare `<cmd>` too) in DM runs
  a headless `gray cmd` command — `!help`, `!model`, `!usage`, `!new`,
  `!sessions`, `!cron`, `!skills`. Answers come back as plain text and
  never spend a model turn.
- A DM from a nick that is not allowlisted gets a one-shot pairing code;
  the owner approves it with `gray irc pairing approve irc <CODE>`.
  Nicks are claimed, not authenticated — treat a paired nick as whoever
  holds it at the time.

## Tools (registered with gray)

- `irc_send {text, target?}` — markdown flattened to plain text, chunked
  to the 400-byte line budget; target defaults to the home channel.

## Limits (v1)

- Text only: no embeds, files, CTCP beyond ACTION, DCC, SASL, or
  streamed replies (the answer posts when the turn ends).
- In a channel the bot only answers when addressed by name; it ignores
  channel traffic not meant for it.
- Delivery is at-least-once: a reconnect between agent finish and the
  PRIVMSG landing retries the same answer rather than regenerating.
- One daemon per config (`gateway.lock`); two daemons would fight over
  the nick.
