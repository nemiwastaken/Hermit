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
- **Attachments**: send a file or photo with a caption such as “Summarize
  this PDF.” Hermit reads text, DOCX and text-based PDFs, and runs OCR on
  images in the offline sandbox.
- **Queue in SQLite**: a message survives a restart and a crash.
- **Model**: any OpenAI-compatible endpoint, DeepSeek by default.
- **Tools**: `bash` (argv, no shell), files `read`/`write`/`edit`/`patch`,
  search `glob`/`grep`/`list`, background processes `process_manage` and
  `execute_code` — Python that calls the tools itself.
- **Internet**: `web_search` (DuckDuckGo, no key) and `web_fetch` (page text as
  markdown) run in the network container.
- **Browser**: Obscura runs JavaScript, follows links, fills forms, and manages
  tabs. The bot sends screenshots and PDFs back to the Telegram chat.
- **Memory and behavior**: `USER.md` and `MEMORY.md` in git, changed through
  tools with a commit; `/rollback` undoes the last change.
- **Agent profile and skills**: [HERMIT.md](core/behavior/HERMIT.md) defines
  the agent and indexes its [built-in guides](core/behavior/skills). Hermit
  loads guide bodies and custom skills through `skill(name)` when needed.
  Keep personal preferences in `USER.md` and lasting facts in `MEMORY.md`.
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

## Telegram attachments

Files up to 20 MB are stored for 30 days and mounted read-only at
`/attachments` in the shared offline sandbox. Text files, DOCX, PDFs with a
text layer, and text recognized in JPEG, PNG and WebP images get a short
preview. Audio, video and other binary files are available to tools as
originals. With a vision-capable Chat Completions model, set `LLM_VISION=1` in
`hermit.env` to send eligible images up to 5 MB to the model directly.

For local development, create the bind mount directory with
`mkdir -p .dev/attachments` before starting Compose. If you customize
`HERMIT_ATTACHMENTS_DIR`, use the same host path for the core and Compose.

## How it works

The main rule: **the core executes nothing**. It only knows "tool name + JSON
arguments → JSON result" and talks to the containers over a unix socket. The
model cannot execute code in the core — there is no such path. `bash` runs
offline. The network and browser containers run without workspace or database mounts.

| Part | What it does | Where it lives |
|---|---|---|
| **Core** `core/` | LLM, queue, Telegram, tool policy | binary on the host, systemd |
| **Behavior** `behavior/` | profile, memory, skills — injected into the prompt | git repo `/opt/hermit/behavior` |
| **Sandbox** `sandbox/` | runs `bash`, files, search, processes | Docker, no network |
| **Net** `net/` | `web_fetch`, `web_search`, remote MCP | Docker, internet access |
| **Browser** `browser/` | Obscura and its MCP tools | Docker, separate network and Unix socket |
| **Workspace** `workspace/` | everything the agent works with | `/opt/hermit/workspace` |

The installer creates `behavior/` and `workspace/`. A development checkout
keeps local state in the ignored `.dev/` directory.

## Browser

The installer starts Obscura on Linux x86-64 and ARM64. Ask the bot to open an
interactive page or send a screenshot, for example “Open https://example.org
and send me a screenshot.” Tabs, cookies, and page state are kept in memory per
Telegram chat. Hermit keeps the four most recently used browser sessions and
clears them when the browser container restarts. Obscura blocks local files and
private network addresses by default.

For local development, create `.dev/run/browser/` before starting Compose and
set `BROWSER_SOCKET=./.dev/run/browser/browser.sock` in `.env`.

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

## Coding from Telegram

From a local Git checkout, run `sudo ./install.sh --with-coding` to add the Rust sandbox and an initial Hermit project. The image includes Hermit's locked dependencies for offline builds. Set up additional repositories in `/opt/hermit/projects.json` using `install/projects.json.example` as a template. Each entry specifies a repository path inside the container and a check command as an argument array.

Use `/project hermit` and send `/code <task>`. Hermit creates a Git worktree, runs the check before and after the changes, and returns the results with buttons for the diff, revisions, and acceptance. Acceptance commits the checked changes to the task branch. Task state is stored in SQLite and can resume after a core restart.

For a local installation that needs access to the host filesystem, run `sudo ./install.sh --host-disk`. The sandbox sees the host under `/host` with the installation user's filesystem permissions, including write access where that user has permission. This setting is opt-in per installation and survives upgrades.

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

[MIT](LICENSE)
