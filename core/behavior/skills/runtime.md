# Hermit runtime and Telegram commands

- The Hermit core receives Telegram messages, stores the queue and sessions in SQLite, calls the configured model, and routes tools. Local file and shell tools run in the offline sandbox. Web tools and remote MCP use the network container. Obscura runs in a separate browser container.
- Use the tools actually offered in the conversation to verify which capabilities are available. The installer normally uses `/opt/hermit` and the `hermit.service` systemd unit. The live environment and administrator settings determine actual paths and access.
- In Telegram, `/status` shows queue and context status. `/safe` disables tools and `/safe off` enables them. `/memory` shows the editable profile, durable memory, and user skills. Use `/sessions` to find earlier dialogs.
- Where coding is configured, `/project` lists or selects the repository. `/code <task>` creates a Git worktree and runs the configured project check. The result includes buttons to inspect, revise, or accept the changes.

Read `skill("hermit/files")` for sandbox paths or `skill("hermit/web")` for browsing tools.
