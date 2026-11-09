# Integrations

How wiki-island connects to AI hosts and editors. Support is claimed only for a host with a recorded test run ([VALIDATION](VALIDATION.md#host-and-client-checks)).

## Setup and uninstall

`wi setup [--vault <dir>]` is one zero-question, idempotent command (default vault `~/wiki-island`). `scripts/install.sh` (macOS and Linux) installs the checksum-verified prebuilt `wi` from the latest release, or builds it from source when none fits, then runs it ([OPERATIONS](OPERATIONS.md#install-and-release)). `scripts/uninstall.sh` runs `wi uninstall`. `wi setup` first preflights every step, including the service binary, and writes nothing if one fails; the failure is one plain sentence. It:

1. initializes the vault if missing;
2. installs the background service (macOS launchd, Linux systemd user unit, Windows Task Scheduler logon task; [OPERATIONS](OPERATIONS.md#service)); where the platform has no usable service manager (no systemd user session, Task Scheduler turned off) it skips this step and says to run `wi watch`;
3. detects every installed AI host and registers the MCP server and skills sync for each;
4. runs the first maintenance pass;
5. prints a three-line plain-language summary.

`wi uninstall` reverses everything wiki-island owns: the service, the host MCP entries and the exported skills, including files and empty containers it created itself (for example a VS Code `mcp.json`). Log and LaunchAgents folders are left only if they existed before. It never deletes notes, removes the disposable device cache and prints one sentence saying the notes were kept. After setup the owner only talks to their AI; maintenance, repair and skill upkeep are silent unless something needs a human, in which case the maintenance procedure tells the owner in one plain sentence.

## Host table

MCP registration is data-driven over a host table in `crates/wi-export` (`hosts` module, `detect(home)`). A host is configured only if its config file or directory already exists: wiki-island detects, and never creates, a host's application directories. Each entry names its detection path, config path, format (a JSON key path or a TOML table), backup and edit strategy.

| Host | Config format |
| --- | --- |
| Claude Code (`~/.claude.json`) | JSON `mcpServers` |
| Codex (`~/.codex/config.toml`, or `$CODEX_HOME`) | standard TOML table `[mcp_servers.wiki-island]` (never an inline table) |
| Antigravity CLI `agy` (`~/.gemini/config/mcp_config.json`) | JSON `mcpServers` with `command`, `args`, `disabled: false` |
| Gemini CLI (`~/.gemini/settings.json`) | JSON `mcpServers` |
| Cursor, Windsurf, Claude Desktop, VS Code, OpenCode, Zed | per the host table |

On Windows the home-relative hosts (Claude Code, Codex with `$CODEX_HOME` kept, Antigravity, Gemini CLI, Cursor, Windsurf, OpenCode) use the same paths under `%USERPROFILE%`. Three hosts have their own location under `%APPDATA%`:

| Host | Windows config |
| --- | --- |
| Claude Desktop | `%APPDATA%\Claude\claude_desktop_config.json` |
| VS Code | `%APPDATA%\Code\User\mcp.json` |
| Zed | `%APPDATA%\Zed\settings.json` |

`--host gemini` is an alias for `--host agy`. The same table decides which hosts get skills ([Skills export](#skills-export)).

## Registration commands

```sh
wi mcp install --host <host> --vault <dir>          # diff preview
wi mcp install --host <host> --vault <dir> --yes    # apply
wi mcp --vault <dir>                                          # raw stdio server
```

`<host>` is one of `claude`, `codex`, `agy`, `gemini`, `cursor`, `windsurf`, `claude-desktop`, `vscode`, `opencode`, `zed`. `wi setup` does this for every detected host. Without `--yes` only a diff is shown. Applying edits only the wiki-island entry, preserves every other key, server, comment and the original bytes and file mode outside that entry, writes a timestamped backup of the original (including a 0-byte file) under `backups/` of the per-device local directory (`<vault>/.wiki-island/local/` in git mode, the data directory in folder mode), never beside the host's files, and is idempotent: an unchanged re-run writes nothing and creates no backup. An existing HTTP transport entry for wiki-island is replaced by stdio fields. The installed command uses the running binary's absolute path. The installer neither launches the host nor verifies its authentication. Use an absolute binary path if a host does not inherit your shell `PATH`.

MCP-created notes get `origin.kind=agent`. Delivered text is evidence, not instruction. No provider-specific memory store is overwritten, and a host without tool access can still read the Markdown files. Tools, schemas and caps: [PROTOCOL](PROTOCOL.md).

## Skills export

Procedures become host skills only where a host reads them: Claude Code reads `~/.claude/skills/<dir>/SKILL.md`, and hosts that follow the cross-host standard read `~/.agents/skills/<dir>/SKILL.md`. A host with no skills directory relies on the procedure resources served over MCP. `<dir>` is `wi-<slug>-<8 hex of skill_id>`: stable for a procedure, never renamed when other procedures appear (a title with no ASCII letters gives an empty slug). If a directory of that name already belongs to the user, an alternative stable name is used. Uninstall removes the files and the empty folders `wi` created, and restores a host file's trailing-comma style. Sync is driven by a persisted digest of the active procedures: any process that may sync (the watcher, the MCP server or the CLI) compares the current digest with the one last synced (`skills-digest` in the per-device local directory) and syncs when they differ. `wi setup` and `wi mcp install --host <h> --yes` enable sync for the host; `wi skills sync` and `wi skills status` (a preview) manage it by hand. Behavior, ownership and limits: [SKILLS](SKILLS.md#skills-export).

## Rules export

Run `wi export rules --host claude|codex|gemini|all --json` (default all). The JSON report lists included paths and every observed omission with its reason. `rule` and `preference` notes are rendered into a generated, read-only `AGENTS.md`, `CLAUDE.md` or `GEMINI.md` fragment that hosts can include.

- Selection uses the full current-eligibility evaluator, not `status=active`. A drifted, expired, conflicted or excluded note is left out, and so is one whose conditions the export cannot evaluate. Omissions are listed.
- Only notes with `origin.kind=user` can be in it. The header says the rules are user-origin and not authenticated.
- The fragment is regenerated, never hand-edited. It carries no wall-clock field, so every device renders the same bytes from the same rules (a fragment from an older layout is regenerated once); it carries `valid_until` (the earliest applicability or freshness expiry among its included notes; `unbounded` when none ages out).
- Notes sort by source path. The renderer quotes complete bodies and attached evidence, labels their origin, and includes source refs and checksums. It drops whole notes deterministically to fit **32768 bytes for Codex** and a conservative **25000 bytes for Claude and Gemini**, header included. The latter are policy limits, not established host caps. A host's other files consume additional context.
- Default paths: `.wiki-island/exports/AGENTS.md`, `.wiki-island/exports/CLAUDE.md`, `.wiki-island/exports/GEMINI.md` (a hidden folder, still synced between devices; engine-generated files are excluded from detectors). Old generated files in `exports/` are removed once, only when byte-identical to what `wi` would write; the config keys `export_codex_path`, `export_claude_path` and `export_gemini_path` override them with vault-relative paths. Default and configured paths are protected from ordinary writes, including folded case and Unicode variants. Restart the server after changing vault config.
- A SHA-256 marker covers every following byte. A hand edit (or an existing unmarked file) is retained: generation refuses with `export-hand-edited` and opens a recheck work note. Move the edited view aside, resolve the work, then regenerate. Deleting a view regenerates it.
- The MCP server regenerates shortly after start and on a 250 ms poll on its background thread, re-evaluating selection and generated-file checksums only when the publication generation or index watermark changed, when the earliest `valid_until` passes, or every 2 s otherwise. No real-time deadline is claimed. CLI generation runs on demand.
- Fragments are not merged into any host instruction file automatically; including them is up to the owner.
- Limit: a host may read a stale file between polls, while a tool call is busy, when regeneration fails, or indefinitely when no server runs at expiry. Check `valid_until` or use current MCP reads.

## Obsidian and other editors

- Notes are plain Markdown, so an Obsidian vault works as is: wikilinks, frontmatter, tags and attachments are read like any note. There is no plugin and no special property schema.
- The `.obsidian/` and `.trash/` folders are ignored and never read, indexed or modified. wiki-island treats an Obsidian save as an ordinary external edit.
- Unsupported link or syntax forms are reported, never rewritten.
- Whether `![[embed]]` is indexed as a link is not established.

## Notion

Export the workspace as Markdown & CSV (zip or folder) and run `wi import <export>`. The export may be at an absolute path outside the vault (for example `~/Downloads/Export.zip`): it is read-only and the converted copy is written into the vault. A folder inside the vault also works. The conversion is offline ([FORMAT](FORMAT.md#notion-exports)).

## Notion view

- One-way and read-only: committed notes (the branch tip, never unsaved files) become pages under one parent page. Each page says it is a read-only view; edits made in Notion are never imported.
- `wi publish notion setup --parent <page link or id>` reads an integration token from standard input or a hidden prompt (if terminal echo cannot be turned off, the prompt refuses and asks for the token on standard input) and stores it in the macOS keychain (service `wiki-island.notion`, account = vault id) through `/usr/bin/security`, or on Windows as a generic Credential Manager entry named `wiki-island.notion/<vault id>`. Linux has no token store, so setup is unavailable there. The token never reaches the vault, the data directory, logs or output. Setup does not contact Notion.
- `wi publish notion [--json]` publishes once; `wi publish notion status` reads local state only.
- The parent, the permalink-to-page map with content hashes, and retry state live in this device's data directory (`<data_dir>/wiki-island/vaults/<vault_id>/notion/`). Only the device where publishing was set up publishes. Setup refuses to run while a publish holds the device lock, so the parent cannot change under a publish in progress. Unchanged notes make no requests; archived or deleted notes archive their page.
- Background care runs a publish thread beside the sync thread: after the committed state has been still for a minute, and every five minutes. Failures back off from one minute to one hour; after a day of continuous failure one plain health line is logged. Markdown mapping, API limits and the non-atomic create caveat are in [wi-notion](../crates/wi-notion/README.md).

## Git remote sync

- `wi sync setup <url>` connects an existing private git remote. `wi sync` makes one attempt and accepts `--json`; `wi sync status` reports local status without network access. Transport is available in git mode only ([OPERATIONS](OPERATIONS.md#git-history-and-sync)).
- `wi setup --github` uses an installed, authenticated `gh` to create or reuse the private `<login>/wiki-island-notes` repository and connects it. A public repository is refused. If the CLI or authentication is unavailable, setup prints the command for connecting an existing remote.
- Background care runs sync separately from maintenance and from Notion publishing, including when a remote is connected after service startup. Each device builds its own index. Conflicting note bytes are retained in a committed copy and a work request; local files receive no conflict markers.
- Never place a git directory in an iCloud Drive or Google Drive folder. A vault inside such a folder uses folder mode, which keeps history in the per-device data directory ([DESIGN](DESIGN.md#storage-and-history)).

## Not integrated

No UI, web app, mobile app or hosted service. No provider-specific memory silo.
