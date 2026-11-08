<p align="center"><img src="docs/assets/logo.png" alt="wiki-island" width="200"></p>

<h1 align="center">wiki-island</h1>

wiki-island is a private, local memory for you and your AI tools. It keeps what you and your assistants learn as plain Markdown notes in one folder, finds them again quickly, and looks after itself.

It never calls a model, and saving, searching and reading never use the network. After a one-time setup you only talk to your AI; wiki-island saves, indexes, repairs and tidies in the background. There is nothing else to do.

## Install

You need a Mac and an internet connection. Open the Terminal app and paste these two lines:

```sh
git clone https://github.com/takealook97/wiki-island.git
sh wiki-island/scripts/install.sh
```

The installer also adds `~/.cargo/bin` to your login profile (`~/.zprofile` for zsh) once, so new Terminal windows find `wi`. It checks that your Mac has Apple's command line tools (if not, it prints the one command to run and stops), installs the Rust build tool if needed, builds wiki-island (a few minutes the first time) and runs `wi setup`. It needs no administrator password and is safe to run again.

`wi setup` asks nothing. It creates your notes folder (`~/wiki-island`), starts a small background service, connects every AI app it finds on this computer (for example Claude Code, Codex, Gemini CLI, Cursor), and prints a three-line summary. It checks everything first and changes nothing if a step cannot work, then tells you why in one sentence.

## What happens next

Nothing. Open your AI app and talk as usual: it can save and look up notes through wiki-island. Maintenance, repair and skill upkeep run silently. If something needs you, your AI tells you in one plain sentence. Tell your AI what you prefer and it remembers. A preference or rule your AI records is kept as a plainly labelled note ("noted by your AI"), separate from the ones you wrote yourself. If you delete or rename a note in Obsidian or any editor, that is saved automatically. Every change to a note is kept in its history, so earlier versions can be brought back.

You can also open the notes folder (`~/wiki-island`) in [Obsidian](https://obsidian.md) or any editor. wiki-island reads ordinary Markdown, `[[links]]`, note properties and attachments as they are, and never touches Obsidian's `.obsidian/` folder.

## Bring your notes

- **Obsidian or any folder of Markdown:** use that folder as the notes folder (`wi setup --vault <folder>`).
- **Notion:** in Notion, choose Export, pick Markdown & CSV, and save the file to your Downloads folder. Then run `wi import ~/Downloads/<name of the export>.zip`. Your export is only read; wiki-island writes a cleaned copy into `Notion/<export name>/` in your notes folder, with Notion's id codes removed from names, links fixed and databases turned into notes.
- **PDF, Word, PowerPoint, CSV and images:** put them in the notes folder and run `wi import <file-or-folder>`. Text is extracted on your Mac into a companion note; originals stay untouched.

## Three ways to keep your notes

- **One folder, synced by Obsidian or a file-sync service.** Put the notes folder in Obsidian Sync, iCloud Drive or Dropbox. The folder is the original; each device keeps its own history outside it, and one device (`wi setup --primary`) runs note maintenance.
- **A private git repository, with Obsidian as a view.** Keep the notes folder out of synced folders and connect a private remote with `wi sync setup <url>` (or `wi setup --github` when the GitHub CLI is signed in). Background care syncs every few minutes; `wi sync` syncs now. Obsidian can open the same folder.
- **Notion as a read-only view.** On one device, run `wi publish notion setup --parent <page link>` and paste a Notion integration token; it is kept in the macOS keychain. Background care then copies committed notes under that page; `wi publish notion` publishes now and `wi publish notion status` shows the state. Edits made in Notion are not brought back.

## Uninstall

```sh
sh wiki-island/scripts/uninstall.sh
```

This removes the background service and every AI-app connection and skill that wiki-island added, and restores the settings files it edited (copies were saved first). Your notes are kept. It also tells you how to remove the Rust build tool if you want that gone.

## Where your data lives

- **Notes:** `~/wiki-island` (or the folder you chose). Plain Markdown files you can open with anything.
- **Settings and backups:** the hidden `.wiki-island` folder inside the notes folder, which also holds the files generated for AI apps. Copies of AI-app settings files taken before any change are in `.wiki-island/local/backups/` (for notes in iCloud Drive or a similar synced folder: in `~/Library/Application Support/wiki-island/vaults/<id>/local/backups/`). In Finder press Cmd+Shift+. to show hidden folders.
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
- **Manual install:** with [rustup](https://rustup.rs) available, run `cargo install --path crates/wi-cli --locked` and then `wi setup`.

## Platforms and license

macOS on Macs with Apple chips (M1 and later) is the tested platform. Linux and Windows are not tested; the background service is macOS only.

## For contributors

wiki-island is a Rust workspace. Start with [CONTRIBUTING](CONTRIBUTING.md) and the [repository contract](AGENTS.md), then the specs:

- [Design](docs/DESIGN.md), [format](docs/FORMAT.md), [knowledge lifecycle](docs/KNOWLEDGE.md), [operations](docs/OPERATIONS.md), [protocol](docs/PROTOCOL.md), [origin and trust](docs/AUTHORITY.md)
- [Integrations](docs/INTEGRATIONS.md), [skills](docs/SKILLS.md), [dependencies](docs/DEPENDENCIES.md)
- [Validation](docs/VALIDATION.md), [performance targets](docs/PERFORMANCE.md)
