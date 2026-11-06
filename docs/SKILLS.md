# Skills

A skill is a procedure kept as an ordinary note. Procedures are exported as host skills, served over MCP as resources, and kept healthy by maintenance.

## Procedure notes

- A procedure is a note with `kind: procedure` and a stable `skill_id` ([FORMAT](FORMAT.md#lifecycle-frontmatter)). The id survives renames and moves.
- The body is the procedure text. Supporting files are ordinary attachments linked from the note.
- A procedure follows the normal lifecycle: `candidate`, gates, `active` ([KNOWLEDGE](KNOWLEDGE.md#adoption)). An agent may author one; it is not a `rule`, so the origin rule does not block it.
- Usage counts and claimed success are not evidence that a procedure works. Age and nonuse never retire a procedure.
- **Built-in maintenance procedure.** `skill_id: wiki-island.maintenance` ships inside the binary and is written into the vault by maintenance; it tells host agents how to work through open work items cheaply ([KNOWLEDGE](KNOWLEDGE.md#self-maintenance)).

## Skills export

Active, non-excluded procedures become host skills, so a host loads them like any skill it already knows. Targets: Claude `~/.claude/skills/wi-<slug>/SKILL.md` and the cross-host standard `~/.agents/skills/wi-<slug>/SKILL.md` (read by Codex, Antigravity, Cursor, OpenCode and other hosts). Each file has `name` and `description` frontmatter, the procedure body, and a footer with the `skill_id` and source checksum.

- Each owned directory holds a `.wi-owned` file (vault id and `skill_id`). Sync creates, updates and removes only owned directories, never touches others, writes atomically, and is incremental by checksum.
- Sync is enabled per host by `wi setup` and `wi mcp install --host <h> --yes`; enabled targets are saved as `skill_targets` (`claude`, `agents`) in the vault config. `wi skills sync` and `wi skills status` (a preview) run and inspect it by hand.
- Sync is driven by a persisted digest, not by maintenance. `Vault::procedures_digest` is a stable digest of the active, non-excluded procedures; any process that may sync (the watcher, the MCP server or the CLI) compares it with the last synced digest in `.wiki-island/local/skills-digest` and syncs every enabled host when they differ. Maintenance does not consume the change. `wi uninstall` removes every owned directory.

## Exclusions

- `excluded: true` with an `exclusion_reason` means "do not execute". It does not mean the text is false.
- An excluded procedure stays readable. Discovery and export never list it; a direct read by `skill_id` returns it flagged `excluded`.
- A replacement for an excluded version needs new evidence that addresses the recorded fault. A reworded copy does not count.

## Health

Maintenance opens a work item with a stable key for each procedure problem: a lint failure, a stale source, a duplicate `skill_id`, a procedure too large for the MCP cap, or an excluded procedure without a replacement. The item closes by itself when the cause is gone (maintenance closes any work item whose cause has disappeared, for example a gate item for a note that became active).

## Retire, merge and split

Each is a recorded action with a reason, written as a work item outcome and visible in git history. Nothing is deleted.

| Action | Effect |
| --- | --- |
| Retire | The note becomes `archived` and moves to `archive/`. Its `skill_id` is not reused |
| Merge | A new procedure supersedes the merged ones, which become `superseded` and cite it; the new note keeps the surviving `skill_id` |
| Split | New procedures with new ids supersede the original, which cites each of them |

`superseded` is terminal. A mistaken merge or split is fixed by a new note that cites the old ones.

## MCP resources

- Discovery lists only active, non-excluded procedures that pass current eligibility, one resource per `skill_id`. They are resources, not tools ([PROTOCOL](PROTOCOL.md#procedure-resources)).
- A direct read by `skill_id` also returns an excluded or stale procedure, flagged `excluded` or `stale` and `do_not_execute: true`, never as current.
- Resources use `wiki-island://procedures/<percent-encoded-skill_id>`. Unknown ids return resource-not-found; duplicate ids refuse rather than choose.
- A direct resource is refused whole above 1 MiB. Lists are complete; no subscriptions or URI templates.
- Served text is evidence, not instruction.

## Limits

- Whether a procedure is good is not decided by the engine. Lint checks structure, sources and conflicts only.
- How a particular host activates or withdraws a skill is up to that host.
