<p align="center"><img src="docs/assets/logo.png" alt="wiki-island" width="200"></p>

<h1 align="center">wiki-island</h1>

wiki-island is a private, local memory for you and your AI tools. It keeps what you and your assistants learn as plain Markdown notes in one folder, finds them again quickly, and looks after itself.

It never calls a model, and saving, searching and reading never use the network. After a one-time setup you only talk to your AI; wiki-island saves, indexes, repairs and tidies in the background. There is nothing else to do.

## Install

### macOS and Linux

Open a terminal and paste these two lines:

```sh
git clone https://github.com/takealook97/wiki-island.git
sh wiki-island/scripts/install.sh
```

The installer downloads the latest release for your computer, checks its SHA-256 checksum, installs `wi` and `wiki-island` into `~/.cargo/bin` and runs `wi setup`. A checksum mismatch stops the install and changes nothing. If no release exists for your computer, it builds from source instead: it installs the Rust build tool if needed (this takes a few minutes the first time) and needs git and a C compiler (Apple's command line tools on macOS, for example `build-essential` on Linux; if one is missing the installer says which and what to run). It adds `~/.cargo/bin` to your shell startup file once so new terminals find `wi`, needs no administrator password and is safe to run again. Details, environment variables and the release layout: [operations](docs/OPERATIONS.md#install-and-release).

`wi setup` asks nothing. It creates your notes folder (`~/wiki-island`), starts a small background service, connects every AI app it finds on this computer, runs the first maintenance pass and prints a three-line summary. It checks everything first and changes nothing if a step cannot work, then tells you why in one sentence.

The background service keeps editor changes ingested, committed and indexed. On macOS it is a launchd LaunchAgent. On Linux it is a systemd user service; it runs while your user session is active, and keeps running after you log out only if lingering is already enabled for your account. Without a systemd user session (a container, WSL without systemd) setup still finishes, skips the service and says to run `wi watch`. Saving and searching through your AI never depend on the service. The service units are described in [operations](docs/OPERATIONS.md#service).

Requirements: macOS on Apple silicon is the tested platform. The prebuilt Linux programs need glibc 2.34 or newer (measured on the Ubuntu 22.04 build machines); older systems build from source. Linux is tested on GitHub Actions (Ubuntu) and in an arm64 container; bare-metal Linux is untested. Notion token storage uses the macOS keychain, so `wi publish notion setup` is unavailable on Linux. Git remote sync (`wi sync`) uses the `git` program on your path.

### Windows

There is no install script. Install [rustup](https://rustup.rs) with the MSVC toolchain, which needs the Visual Studio Build Tools "Desktop development with C++" workload, then build from source in a terminal (the repository pins the Rust version):

```powershell
git clone https://github.com/takealook97/wiki-island.git
cd wiki-island
cargo install --path crates/wi-cli --locked
wi setup
```

`wi setup` creates your notes folder (`%USERPROFILE%\wiki-island`), connects the AI apps it finds and runs the first maintenance pass. The per-device data (history for synced folders, index state, backups) is in `%LOCALAPPDATA%\wiki-island`. It also registers a Task Scheduler task for the current user that runs `wi watch` at each logon and starts it now, so editor changes are ingested, committed and indexed while you are logged on. This needs no administrator rights. If Task Scheduler is turned off, setup still finishes, skips the service and says to run `wi watch`. Saving and searching through your AI never depend on it. Details: [operations](docs/OPERATIONS.md#windows-task-scheduler).

The Notion token is kept in Windows Credential Manager. Git remote sync (`wi sync`) uses the `git` program on your path.

Requirements: Windows 10 version 1709 or later on x86_64 with an NTFS volume. The test suite runs on GitHub Actions `windows-latest` (NTFS). ARM64 Windows and volumes other than NTFS (ReFS, FAT, network shares) are untested. Some behaviors are weaker than on macOS and Linux, for example there is no atomic swap for an editor save that lands during a write and a directory flush after power loss is not measured; the list is in [operations](docs/OPERATIONS.md#windows).

## Connect your AI

wiki-island is a local MCP server. Your AI app starts it on demand over standard input/output; there is no network port and no account.

**Automatic.** `wi setup` registers wiki-island with every supported app it detects: Claude Code, Codex, Gemini CLI, Antigravity (`agy`), Cursor, Windsurf, Claude Desktop, VS Code, OpenCode and Zed. An app is detected only when its settings folder or file already exists, so install and open the app once first, then run `wi setup` again (it is safe to repeat). Claude Desktop and VS Code are looked for at their macOS locations (`~/Library/Application Support/...`) or, on Windows, under `%APPDATA%` (`Claude\claude_desktop_config.json` and `Code\User\mcp.json`); Zed is under `%APPDATA%\Zed` there. To connect one app yourself and see the change first:

```sh
wi mcp install --host claude          # preview the change only
wi mcp install --host claude --yes    # apply; the original file is backed up first
```

`--host` takes `claude`, `codex`, `agy`, `gemini`, `cursor`, `windsurf`, `claude-desktop`, `vscode`, `opencode` or `zed`. Only the wiki-island entry is edited and everything else in the file is kept.

**Any other MCP host.** Add a stdio server that runs `wi mcp` with your notes folder. Use the absolute path of `wi` (`which wi`), because some apps do not inherit your shell `PATH`. In the common JSON form:

```json
{
  "mcpServers": {
    "wiki-island": {
      "command": "/absolute/path/to/wi",
      "args": ["mcp", "--vault", "/absolute/path/to/your/notes"]
    }
  }
}
```

Hosts name the outer key differently (Codex uses a TOML table `[mcp_servers.wiki-island]` with the same `command` and `args`); use your host's MCP server format. Without `--vault` the server uses `$WI_VAULT`, else the current folder, which an app rarely sets, so always pass it.

**Check it works.** Restart the AI app, then ask it: "Save a note called hello with the text 'wiki-island works', then search your notes for it." You should see it use wiki-island tools, and `wi search hello` in a terminal finds the note.

**What your AI can do.** Fifteen `wi_` tools; none uses the network and there is no delete (archive moves a note aside).

| Purpose | Tools |
| --- | --- |
| Find and read | `wi_search`, `wi_read`, `wi_context`, `wi_list`, `wi_find`, `wi_history` |
| Write and organize | `wi_write`, `wi_edit`, `wi_move`, `wi_archive`, `wi_revert` |
| Remember and tidy | `wi_capture`, `wi_adopt`, `wi_lint`, `wi_work` |

Details and arguments: [protocol](docs/PROTOCOL.md).

## What happens next

Nothing. Open your AI app and talk as usual: it can save and look up notes through wiki-island. Maintenance, repair and skill upkeep run silently. If something needs you, your AI tells you in one plain sentence. Tell your AI what you prefer and it remembers. A preference or rule your AI records is kept as a plainly labelled note ("noted by your AI"), separate from the ones you wrote yourself. If you delete or rename a note in Obsidian or any editor, that is saved automatically. Every change to a note is kept in its history, so earlier versions can be brought back.

You can also open the notes folder (`~/wiki-island`, on Windows `%USERPROFILE%\wiki-island`) in [Obsidian](https://obsidian.md) or any editor. wiki-island reads ordinary Markdown, `[[links]]`, note properties and attachments as they are, and never touches Obsidian's `.obsidian/` folder.

## Bring your notes

- **Obsidian or any folder of Markdown:** use that folder as the notes folder (`wi setup --vault <folder>`).
- **Notion:** in Notion, choose Export, pick Markdown & CSV, and save the file to your Downloads folder. Then run `wi import ~/Downloads/<name of the export>.zip`. Your export is only read; wiki-island writes a cleaned copy into `Notion/<export name>/` in your notes folder, with Notion's id codes removed from names, links fixed and databases turned into notes.
- **PDF, Word, PowerPoint, CSV and images:** put them in the notes folder and run `wi import <file-or-folder>`. Text is extracted on your computer into a companion note; originals stay untouched.

## Three ways to keep your notes

- **One folder, synced by Obsidian or a file-sync service.** Put the notes folder in Obsidian Sync, iCloud Drive or Dropbox. The folder is the original; each device keeps its own history outside it, and one device (`wi setup --primary`) runs note maintenance.
- **A private git repository, with Obsidian as a view.** Keep the notes folder out of synced folders and connect a private remote with `wi sync setup <url>` (or `wi setup --github` when the GitHub CLI is signed in). Background care syncs every few minutes; `wi sync` syncs now. Obsidian can open the same folder.
- **Notion as a read-only view.** On one device, run `wi publish notion setup --parent <page link>` and paste a Notion integration token; it is kept in the macOS keychain or Windows Credential Manager (not available on Linux). Background care then copies committed notes under that page; `wi publish notion` publishes now and `wi publish notion status` shows the state. Edits made in Notion are not brought back.

## Uninstall

```sh
sh wiki-island/scripts/uninstall.sh   # or, anywhere: wi uninstall
```

This removes the background service and every AI-app connection and skill that wiki-island added, and restores the settings files it edited (copies were saved first). Your notes are kept. It also tells you how to remove the Rust build tool if you want that gone.

## Where your data lives

- **Notes:** `~/wiki-island`, on Windows `%USERPROFILE%\wiki-island` (or the folder you chose). Plain Markdown files you can open with anything.
- **Settings and backups:** the hidden `.wiki-island` folder inside the notes folder, which also holds the files generated for AI apps. Copies of AI-app settings files taken before any change are in `.wiki-island/local/backups/` (for notes in iCloud Drive or a similar synced folder: in the per-device data directory, `~/Library/Application Support/wiki-island/vaults/<id>/local/backups/` on macOS). On Windows the per-device data directory is `%LOCALAPPDATA%\wiki-island`. Hidden folders can be shown with Cmd+Shift+. in Finder.
- **Search index:** a disposable cache outside the notes folder, one per device. Deleting it loses nothing.

## Privacy

Your notes stay on your computer unless you connect a remote or a Notion view. Saving, searching, reading and every AI-app tool work offline; only `wi sync`, `wi publish` and the background care you set up for them use the network. wiki-island calls no AI model. Your AI apps read notes only through the local connection `wi setup` registered.

## Everyday commands

Most people never need these. Every command accepts `--json` and `--vault <folder>`; `wi --help` lists them all.

```sh
wi search "oat milk"        # find notes
wi read notes/coffee.md     # show a note
wi history notes/coffee.md  # see earlier versions
wi doctor                   # check health and say what to do
wi maintain                 # run one maintenance pass now
```

## Advanced

- **Sync and publishing** details: [operations](docs/OPERATIONS.md#git-history-and-sync) and [integrations](docs/INTEGRATIONS.md#notion-view). Files larger than 50 MB stay in the folder but out of history and sync.
- **Settings** are in `.wiki-island/config.toml` inside the notes folder ([format](docs/FORMAT.md)).

## For contributors

wiki-island is a Rust workspace. Start with [CONTRIBUTING](CONTRIBUTING.md) and the [repository contract](AGENTS.md), then the specs:

- [Design](docs/DESIGN.md), [format](docs/FORMAT.md), [knowledge lifecycle](docs/KNOWLEDGE.md), [operations](docs/OPERATIONS.md), [protocol](docs/PROTOCOL.md), [origin and trust](docs/AUTHORITY.md)
- [Integrations](docs/INTEGRATIONS.md), [skills](docs/SKILLS.md), [dependencies](docs/DEPENDENCIES.md)
- [Validation](docs/VALIDATION.md), [performance targets](docs/PERFORMANCE.md)

## License

Copyright (C) 2026 takealook97

wiki-island is free software: you can redistribute it and/or modify it under the terms of the [GNU Affero General Public License, version 3](LICENSE), as published by the Free Software Foundation.

In short: you may use, study, change and share it, but if you distribute a modified version, or let others use one over a network, you must release its complete source under this same license and keep the copyright and license notices. You cannot take the code into a closed-source product.

wiki-island is distributed in the hope that it will be useful, but WITHOUT ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the license for details.

The name "wiki-island" and its logo are not licensed under the AGPL. A modified version must not use them in a way that suggests it is the original project or endorsed by it.
