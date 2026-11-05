# wi-lifecycle

The pure lifecycle engine: no I/O, no clock of its own, no model. The host passes note snapshots, source bytes, evidence times, observation events, loss events and `now`; the engine returns adoption gate results, frontmatter edits, work notes, read-time eligibility and freshness, change-cap decisions, lint findings and capture filtering. `lifecycle: off` makes it inert. It depends only on `wi-note`. Behavior: [KNOWLEDGE](../../docs/KNOWLEDGE.md), [AUTHORITY](../../docs/AUTHORITY.md).

Maintenance detectors take ephemeral `DetectionNote` inputs parsed from the
canonical files. They do not add stored fields to note snapshots or require an
index migration; existing vaults keep working and an index rebuild preserves
all detector inputs. The compiled maintenance procedure includes its
`maintenance_version`; maintenance refreshes its bytes automatically.

- Duplicate checks lowercase and collapse whitespace, then group equal body
  hashes with at least 200 normalized characters. Equal raw bodies that differ
  only in frontmatter are excluded. One item lists all members; its identity
  depends on the normalized body digest, so adding or removing a member
  refreshes the same item. Fewer than two members resolves the condition.
  Note-targeted duplicate items remain readable and close when their condition
  is absent. A pass creates at most five new duplicate groups.
- Promotion suggestions require at least three live, pairwise-similar notes
  across two known UTC dates in one scope. Features are distinct lowercase body
  tokens after removing the vault's 200 most frequent body tokens, computed
  once per pass across live notes. Each note needs at least 20 remaining
  tokens, with Jaccard overlap at least 0.6. Periodic folders, date-shaped
  titles or filenames, procedures and notes referenced by a live procedure
  are excluded. The rarest token occurring in at least three inputs chooses a
  bucket; at most ten active groups of ten members are compared for each note
  in a bucket. Different buckets or evicted groups can miss a repeated
  workflow. These are suggestions, never a finding that the content is true.
- Promotion identities use sorted permalinks, falling back to paths when a
  permalink is absent. One pass creates at most three promotion items and
  keeps at most ten open. Existing live items remain open when a creation cap
  is reached. Maintenance closes detector items when their condition
  disappears. It never merges, deletes or promotes notes automatically.

Validation: unit tests in [detect.rs](src/detect.rs) exercise normalization,
dates, scope, references, bounded membership and stable identities. The
[maintenance integration test](../wi-store/tests/maintain.rs) checks that a
200-note mixed corpus produces two duplicate groups and one promotion
suggestion, a second pass writes and commits nothing, membership changes keep
body-group identities, resolved causes close items, and creation/open caps hold.
