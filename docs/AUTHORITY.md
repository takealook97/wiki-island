# Origin and trust without approval

There is no human approval step. Trust rests on provenance, git history, revert, lint and conflict detection ([KNOWLEDGE](KNOWLEDGE.md#adoption)). The origin rule, supersedes validation and the retirement ban are implemented in `crates/wi-lifecycle` and applied by the store and the MCP provider. This file keeps its name so links survive.

## Origin by ingress

`origin` says who authored a note's current claim content (`summary`, `sources`, `applicability`, `kind`, `claim_kind`, `observed_at`, `entities`, `scope`, `slot`, `value` and, for `type: memory`, the Markdown body). It is set from how that content arrived, never from what the content says, and no caller can write it ([FORMAT](FORMAT.md#lifecycle-frontmatter)).

| Ingress | `origin.kind` |
| --- | --- |
| The CLI | `user` |
| Direct file edit (any editor, file sync, git merge, `wi sync`) | as written in the note; `user` when the note has no `origin` |
| MCP tool call | `agent` |
| Document or file import | `file` |
| Fetched URL | `url` |

- Provenance is carried by the note itself. File ingress cannot tell an editor from a file-sync service or a git merge, so it never infers `origin` from the channel or device that delivered the bytes: an agent-origin note edited or synced in stays `agent`, and a `user` note is never downgraded. A file-ingress change to claim content resets the note to `candidate`.
- The owner adopts an agent note by removing its agent label (`origin.kind: agent`, or `noted_by: agent` on a noted rule or preference) in any editor, or with `wi adopt <note>`. A lifecycle note without `origin` then gets `origin.kind=user`; while either label remains, file ingress never adds a `user` origin. There is no MCP adopt tool, and MCP may not remove or change `noted_by`. The "user-origin, not authenticated" caveat below still applies.
- MCP tools never change claim content in place. An MCP edit of claim fields, body edits of memory notes included, creates a new candidate that supersedes the old note, so an agent cannot inherit a user's origin and an agent-appended instruction never reaches a current user rule.
- Work notes and the deferred queue are engine-owned executable state. Ordinary tools cannot create or change them or write under the work directory; a queued change runs only from an engine-written work note and is re-authorized against the live target when it runs.
- Every operation also records its ingress actor in the pending manifest and a commit trailer ([OPERATIONS](OPERATIONS.md#write-path)). The actor is history, separate from `origin`.
- Ordinary tools cannot write the vault config, generated exports, `.git` or paths outside the vault, and never follow symlinks.

## The rule and preference origin rule

- A `rule` or `preference` note is adopted only when `origin.kind=user`.
- Agent-derived content may become `fact`, `decision`, `procedure` or `event`, or a `historical` claim. It never becomes a `rule` or `preference`.
- A `rule` or `preference` loses visibility (supersede, archive, exclude) only through user (file) ingress; agents and automatic engine actions never retire one ([KNOWLEDGE](KNOWLEDGE.md#conflicts-and-the-automatic-change-cap)).

## Limits

- An agent with shell access can write a file directly, and that edit is recorded as `user`. The engine cannot tell who typed it.
- A user-origin claim needs no source pins and missing classification fields are filled automatically ([KNOWLEDGE](KNOWLEDGE.md#user-origin-auto-completion)). Rules and preferences still need `origin.kind=user`, so this widens what a shell-capable agent can assert without changing the origin rule.
- Git shows every such change, and revert undoes it. Neither prevents it.
- The rule constrains what the engine adopts. It does not authenticate anyone.
- Persistent rules and preferences are the natural target for memory poisoning: text written once and re-read later as instruction. Without a human gate, the origin rule, lint, conflict detection and the evidence-not-instruction label ([KNOWLEDGE](KNOWLEDGE.md#evidence-not-instruction)) are the main defense. They lower the risk from agent writes over MCP and do nothing against an agent that edits files through a shell.

## Provenance

- Every snapshot observation stores `observed_by`, the ingress origin kind: `agent` for MCP, `user` for the CLI and the watcher. A sidecar written without the field reads as `unknown`. Delivered source metadata and lint and gate guidance show it.
- A shell-capable agent writes files as the user, so its edits arrive as `user` and carry `observed_by: user`. The engine cannot tell them from the owner's.
- Snapshots are self-reported by whoever wrote them. `observed_by` records the ingress, not who authored the content.
- The generated rules export header says the rules are user-origin and not authenticated.
- No gate depends on `observed_by`.
