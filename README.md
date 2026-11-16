<div align="center">

# **Read this in another language**

[🇷🇺 Русский](translations/ru.md)

---

</div>

# Hermit

A home agent: it lives on your server, is driven from Telegram, and works with
files and commands inside an isolated container. Its profile, memory and skills
live in git — behavior changes by commit and rolls back with one command.

## Installation

Requires Linux, systemd and Docker with compose. Rust is only needed to build —
install it with rustup as your own user; the installer finds `cargo` itself.

```sh
curl -fsSL https://raw.githubusercontent.com/nemiwastaken/Hermit/main/install.sh | sudo bash
```

The installer fetches the sources, builds the core and the images, creates the
directories, sets up a git repository for behavior, installs the systemd unit
and starts everything. It asks for the bot token and the LLM key.

Running it again is an update: config, database and `behavior/` are not
overwritten, and the binary is replaced atomically. Everything lives in
`/opt/hermit` (`hermit.env` with the secrets, `behavior/`, `workspace/`,
`data/hermit.db`, `mcp.json`), except the systemd unit.

## Features

- **Telegram**: long-poll, private chats only and only allow-listed users. The
  answer streams into one message, and "typing…" stays on during tool calls too.
- **Queue in SQLite**: a message survives a restart and a crash.
- **Model**: any OpenAI-compatible endpoint, DeepSeek by default.
- **Tools**: `bash` (argv, no shell), files `read`/`write`/`edit`/`patch`,
  search `glob`/`grep`/`list`, background processes `process_manage` and
  `execute_code` — Python that calls the tools itself.
- **Internet**: `web_search` (DuckDuckGo, no key) and `web_fetch` (page text as
  markdown) from a separate container without `/workspace`. There is no browser:
  JS and logins are not read.
- **Memory and behavior**: `USER.md` and `MEMORY.md` in git, changed through
  tools with a commit; `/rollback` undoes the last change.
- **Skills**: the prompt carries only the name and a one-line description, the
  body is read by `skill(name)` — fewer tokens per turn.
- **Sessions**: closed dialogs go to the archive; past ones are found by a
  question, `/sessions` or `recall`. Anything older than 30 days is deleted.
- **Context**: `/compress` replaces the transcript with a summary and the last
  exchange; at 100k characters this happens automatically.
- **Schedules without the model**: reminders and trackers — the question, the
  number and the average are computed by SQLite, not the LLM.
- **Cost**: `/usage` and `/insights` show tokens and price from SQLite; the
  price is computed if `LLM_PRICE_IN_PER_M` and `LLM_PRICE_OUT_PER_M` are set.
- **MCP**: remote HTTP servers add their tools at startup.
- **Protection**: dangerous commands (`rm -rf`, `mkfs`, `dd`, `reboot`, …) wait
  for a button in Telegram; `/dlq` and `/retry` deal with failed events.

## How it works

The main rule: **the core executes nothing**. It only knows "tool name + JSON
arguments → JSON result" and talks to the containers over a unix socket. The
model cannot execute code in the core — there is no such path. `bash` never has
network access, and `web_fetch` sees neither files nor the database.

| Part | What it does | Where it lives |
|---|---|---|
| **Core** `core/` | LLM, queue, Telegram, tool policy | binary on the host, systemd |
| **Behavior** `behavior/` | profile, memory, skills — injected into the prompt | git repo `/opt/hermit/behavior` |
| **Sandbox** `sandbox/` | runs `bash`, files, search, processes | Docker, no network |
| **Net** `net/` | `web_fetch`, `web_search`, MCP | Docker, the only way out |
| **Workspace** `workspace/` | everything the agent works with | `/opt/hermit/workspace` |

Only `core/`, `sandbox/`, `net/` and the installer are in the repository.
`behavior/` and `workspace/` are created on install; in a development checkout
they are replaced by a single ignored `.dev/`.

## Commands

Answered immediately, no model call:

| Command | What it does |
|---|---|
| `/pause`, `/resume` | stop and resume queue processing |
| `/safe`, `/safe off` | disable and re-enable the tools |
| `/status` | pause, safe mode, queue, context, archive |
| `/new`, `/clear` | archive the dialog / delete the chat context |
| `/sessions` | list, `<id>`, `resume`, `rename`, `delete` |
| `/memory` | profile, memory and skills the agent sees |
| `/compress` | compress the context into a summary |
| `/usage`, `/insights [days]` | spend for today and 7 days / by day |
| `/changes`, `/rollback` | behavior history and undoing the last change |
| `/dlq`, `/retry <id>` | failed events and returning one to the queue |
| `/help` | the list |

## MCP

`/opt/hermit/mcp.json` — an array of remote HTTP servers (Streamable HTTP):

```json
[
  {
    "name": "github",
    "url": "https://api.githubcopilot.com/mcp/",
    "headers": {"Authorization": "Bearer ..."},
    "approval": true
  }
]
```

The tools appear to the model as `mcp__<server>__<tool>`. `approval: true`
requires a button for every call, `allow_private: true` allows a server on the
home network (private addresses are blocked by default, as in `web_fetch`).
`stdio` is not supported. After editing, run
`docker compose -f /opt/hermit/compose.yml restart net`; the bot picks up the
new tools within a minute.

## License

See `LICENSE`.
