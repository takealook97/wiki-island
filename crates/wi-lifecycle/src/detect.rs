//! Conservative, model-free suggestions over canonical note content. These
//! checks only produce work items; they never merge, delete or promote notes.

use std::collections::{BTreeMap, BTreeSet};

use wi_note::{Kind, LifecycleType, Note, Status};

use crate::{NoteSnapshot, Policy, Timestamp, Trigger, WorkItem, WorkKind, parse_time, sha256};

/// Maximum members named in one promotion suggestion and active groups per bucket.
pub const MAX_MEMBERS: usize = 10;
/// Maximum new promotion suggestions written by one maintenance pass.
pub const MAX_NEW_CANDIDATES: usize = 3;
/// Maximum open promotion suggestions in a vault.
pub const MAX_OPEN_CANDIDATES: usize = 10;
/// Maximum new duplicate groups written by one maintenance pass.
pub const MAX_NEW_DUPLICATES: usize = 5;

/// Ephemeral detector input. It is rebuilt from files, never added to the index format.
#[derive(Debug, Clone)]
pub struct DetectionNote {
    snapshot: NoteSnapshot,
    title: String,
    raw_body_hash: String,
    content_hash: String,
    body: String,
    date: Option<String>,
    references: Vec<String>,
}

impl DetectionNote {
    pub fn from_note(path: &str, note: &Note) -> Self {
        let snapshot = NoteSnapshot::from_note(path, note);
        let field = |key: &str| {
            note.frontmatter()
                .and_then(|f| f.get(key))
                .and_then(|v| v.as_str().map(str::to_owned))
        };
        let authored = [
            field("created_at"),
            field("date"),
            snapshot
                .lifecycle
                .origin
                .as_ref()
                .and_then(|o| o.captured_at.clone()),
            snapshot.lifecycle.observed_at.clone(),
        ]
        .into_iter()
        .flatten()
        .find_map(|text| {
            parse_time(&text).ok().or_else(|| {
                chrono::NaiveDate::parse_from_str(&text, "%Y-%m-%d")
                    .ok()
                    .and_then(|d| d.and_hms_opt(0, 0, 0))
                    .map(|d| d.and_utc())
            })
        });
        let date = authored.map(|t| t.date_naive().to_string());
        let references = note
            .relations()
            .into_iter()
            .map(|r| r.target)
            .chain(
                snapshot
                    .lifecycle
                    .sources
                    .iter()
                    .flatten()
                    .map(|s| s.reference.clone()),
            )
            .collect();
        Self {
            snapshot,
            title: normalize(&field("title").unwrap_or_default()),
            raw_body_hash: sha256(note.body().as_bytes()),
            content_hash: sha256(note.to_bytes()),
            body: normalize(note.body()),
            date,
            references,
        }
    }

    fn identity(&self) -> &str {
        self.snapshot
            .permalink
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or(&self.snapshot.path)
    }

    fn live(&self, policy: &Policy) -> bool {
        let note = &self.snapshot;
        note.unparseable.is_none()
            && note.lifecycle.lifecycle_type != LifecycleType::Work
            && note.lifecycle.excluded != Some(true)
            && !matches!(
                note.lifecycle.status,
                Some(Status::Rejected | Status::Superseded | Status::Archived)
            )
            && !policy.is_archived_path(&note.path)
    }
}

fn normalize(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn tokens(note: &DetectionNote) -> BTreeSet<String> {
    note.body
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

fn periodic(note: &DetectionNote) -> bool {
    const FOLDERS: &[&str] = &[
        "daily",
        "dailies",
        "daily notes",
        "journal",
        "journals",
        "weekly",
        "weeklies",
        "monthly",
        "yearly",
        "periodic",
        "periodic notes",
    ];
    let path = note.snapshot.path.to_lowercase();
    let mut parts = path.split('/').peekable();
    while let Some(part) = parts.next() {
        if parts.peek().is_some() && FOLDERS.contains(&part) {
            return true;
        }
    }
    let dated = |text: &str| {
        let text = text.trim().trim_end_matches(".md");
        ["%Y-%m-%d", "%Y/%m/%d", "%Y.%m.%d", "%Y%m%d"]
            .iter()
            .any(|format| chrono::NaiveDate::parse_from_str(text, format).is_ok())
            || {
                let parts: Vec<_> = text.split(['-', '/', '.']).collect();
                parts.len() == 2
                    && parts[0].len() == 4
                    && parts[0].parse::<u16>().is_ok()
                    && (parts[1].parse::<u8>().is_ok_and(|m| (1..=12).contains(&m))
                        || parts[1].strip_prefix('w').is_some_and(|week| {
                            week.parse::<u8>().is_ok_and(|w| (1..=53).contains(&w))
                        }))
            }
    };
    dated(&note.title) || dated(path.rsplit('/').next().unwrap_or(&path))
}

fn overlap(a: &BTreeSet<String>, b: &BTreeSet<String>) -> bool {
    let shared = a.intersection(b).count();
    let union = a.len() + b.len() - shared;
    union > 0 && shared * 5 >= union * 3
}

/// One item per exact normalized-body group, with all member paths. Bodies
/// shorter than 200 characters and frontmatter-only variants are ignored.
/// The body digest identifies a group independently of its current members.
pub fn duplicates(notes: &[DetectionNote], policy: &Policy, now: Timestamp) -> Vec<WorkItem> {
    if !policy.lifecycle_on {
        return Vec::new();
    }
    let mut groups: BTreeMap<_, Vec<&DetectionNote>> = BTreeMap::new();
    for note in notes.iter().filter(|n| n.live(policy)) {
        if note.body.chars().count() >= 200 {
            groups
                .entry(sha256(note.body.as_bytes()))
                .or_default()
                .push(note);
        }
    }
    let mut out = Vec::new();
    for (digest, mut members) in groups {
        if members.len() < 2 {
            continue;
        }
        // Equal bodies with different metadata alone are not redundant notes.
        let body_hashes: BTreeSet<_> = members.iter().map(|n| &n.raw_body_hash).collect();
        let content_hashes: BTreeSet<_> = members.iter().map(|n| &n.content_hash).collect();
        if body_hashes.len() == 1 && content_hashes.len() > 1 {
            continue;
        }
        members.sort_by(|a, b| a.snapshot.path.cmp(&b.snapshot.path));
        let paths: Vec<_> = members.iter().map(|n| n.snapshot.path.clone()).collect();
        out.push(WorkItem::new(WorkKind::Duplicate, &digest, paths.clone(),
            format!("notes share an exact normalized body: {}", paths.join(", ")),
            "compare the members and resolve redundant content; never merge or delete automatically",
            Trigger::Session, now)
            .with_condition(format!("duplicate: {digest}")));
    }
    out
}

fn reference_key(reference: &str) -> &str {
    reference
        .split('#')
        .next()
        .unwrap_or(reference)
        .trim()
        .trim_start_matches("./")
}

fn relative_reference(path: &str, reference: &str) -> String {
    let mut parts: Vec<_> = path
        .rsplit_once('/')
        .map(|(dir, _)| dir.split('/').collect())
        .unwrap_or_default();
    for part in reference.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            _ => parts.push(part),
        }
    }
    parts.join("/")
}

/// Repeated notes across at least two known dates may describe a procedure.
/// Rarest-token buckets and at most ten groups of ten members bound comparisons
/// per note. This conservative heuristic may miss similar notes in different
/// buckets; it never equates similarity with truth or edits a note.
pub fn promote_candidates(
    notes: &[DetectionNote],
    policy: &Policy,
    now: Timestamp,
) -> Vec<WorkItem> {
    if !policy.lifecycle_on {
        return Vec::new();
    }
    let referenced: BTreeSet<String> = notes
        .iter()
        .filter(|n| n.live(policy) && n.snapshot.lifecycle.kind == Some(Kind::Procedure))
        .flat_map(|n| {
            n.references.iter().flat_map(|r| {
                let key = reference_key(r);
                [key.to_owned(), relative_reference(&n.snapshot.path, key)]
            })
        })
        .collect();
    let referenced_titles: BTreeSet<String> = referenced.iter().map(|r| normalize(r)).collect();
    let eligible = |n: &&DetectionNote| {
        n.live(policy)
            && n.snapshot.lifecycle.kind != Some(Kind::Procedure)
            && n.date.is_some()
            && !periodic(n)
            && !referenced.contains(n.snapshot.path.as_str())
            && !referenced.contains(n.identity())
            && !referenced.contains(n.snapshot.path.trim_end_matches(".md"))
            && (n.title.is_empty() || !referenced_titles.contains(&n.title))
            && !referenced.contains(
                n.snapshot
                    .path
                    .rsplit('/')
                    .next()
                    .unwrap_or(&n.snapshot.path)
                    .trim_end_matches(".md"),
            )
    };
    // Count body tokens once across the live vault, including periodic notes,
    // so boilerplate cannot become a domain feature merely by filtering notes.
    let mut common = BTreeMap::<String, usize>::new();
    for note in notes.iter().filter(|n| n.live(policy)) {
        for token in note
            .body
            .split(|c: char| !c.is_alphanumeric())
            .filter(|s| !s.is_empty())
        {
            *common.entry(token.to_owned()).or_default() += 1;
        }
    }
    let mut ranked: Vec<_> = common.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let stop: BTreeSet<_> = ranked
        .into_iter()
        .take(200)
        .map(|(token, _)| token)
        .collect();
    let mut entries: Vec<_> = notes
        .iter()
        .filter(eligible)
        .map(|n| {
            (
                n,
                tokens(n)
                    .difference(&stop)
                    .cloned()
                    .collect::<BTreeSet<_>>(),
            )
        })
        .filter(|(_, t)| t.len() >= 20)
        .collect();
    entries.sort_by(|a, b| {
        a.0.identity()
            .cmp(b.0.identity())
            .then(a.0.snapshot.path.cmp(&b.0.snapshot.path))
    });
    let mut frequencies = BTreeMap::<&str, usize>::new();
    for (_, set) in &entries {
        for token in set {
            *frequencies.entry(token).or_default() += 1;
        }
    }
    let mut buckets: BTreeMap<_, Vec<usize>> = BTreeMap::new();
    for (index, (note, set)) in entries.iter().enumerate() {
        // Unique embellishments must not isolate otherwise repeated workflows.
        // A token seen fewer than three times cannot anchor a three-note group.
        if let Some(token) = set
            .iter()
            .filter(|t| frequencies[t.as_str()] >= 3)
            .min_by_key(|t| (frequencies[t.as_str()], t.as_str()))
        {
            buckets
                .entry((note.snapshot.lifecycle.scope.clone(), token.as_str()))
                .or_default()
                .push(index);
        }
    }
    let mut out = Vec::new();
    let mut emit = |group: &[usize]| {
        if group.len() < 3 {
            return;
        }
        let dates: BTreeSet<_> = group
            .iter()
            .filter_map(|i| entries[*i].0.date.as_deref())
            .collect();
        if dates.len() < 2 {
            return;
        }
        let mut members: Vec<_> = group.iter().map(|i| entries[*i].0).collect();
        members.sort_by(|a, b| a.identity().cmp(b.identity()));
        let identities: Vec<_> = members.iter().map(|n| n.identity().to_owned()).collect();
        // Work identity is the sorted permalinks (paths only when absent).
        // Paths remain in the reason so the agent can open every member.
        out.push(WorkItem::new(WorkKind::PromoteCandidate, &identities[0], identities[1..].to_vec(),
            format!("repeated notes may describe a procedure: {}", members.iter().map(|n| n.snapshot.path.as_str()).collect::<Vec<_>>().join(", ")),
            "read the members and write a new procedure only if they describe a durable workflow", Trigger::Session, now)
            .with_condition(format!("promote-candidate: {}", sha256(members.iter().map(|n| format!("{}:{}", n.identity(), sha256(n.body.as_bytes()))).collect::<Vec<_>>().join("\0").as_bytes()))));
    };
    for indices in buckets.into_values() {
        let mut groups: Vec<Vec<usize>> = Vec::new();
        for index in indices {
            if let Some(group) = groups.iter_mut().find(|g| {
                g.iter()
                    .all(|other| overlap(&entries[index].1, &entries[*other].1))
            }) {
                group.push(index);
                if group.len() == MAX_MEMBERS {
                    emit(group);
                    group.clear();
                }
            } else {
                if groups.len() == MAX_MEMBERS {
                    emit(&groups.remove(0));
                }
                groups.push(vec![index]);
            }
            groups.retain(|g| !g.is_empty());
        }
        for group in groups {
            emit(&group);
        }
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> Timestamp {
        parse_time("2026-11-07T00:00:00Z").unwrap()
    }

    fn note(path: &str, title: &str, date: &str, body: &str) -> DetectionNote {
        let text = format!(
            "---\ntitle: {title}\npermalink: {}\ndate: {date}\n---\n{body}",
            path.trim_end_matches(".md")
        );
        DetectionNote::from_note(path, &Note::parse(text.as_bytes()).unwrap())
    }

    fn workflow() -> String {
        (0..25)
            .map(|i| format!("release{i}"))
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn repeated() -> Vec<DetectionNote> {
        let mut notes: Vec<_> = (0..3)
            .map(|i| {
                note(
                    &format!("notes/{i}.md"),
                    "Release build",
                    if i == 0 { "2026-11-01" } else { "2026-11-02" },
                    &format!("{} detail{i}", workflow()),
                )
            })
            .collect();
        let common = (0..200)
            .map(|i| format!("common{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        notes.push(note(
            "background.md",
            "Background",
            "",
            &format!("{common}\n").repeat(100),
        ));
        notes
    }

    #[test]
    fn repeated_notes_need_three_members_and_two_known_dates() {
        let mut notes = repeated();
        let items = promote_candidates(&notes, &Policy::default(), now());
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].related.len(), 2);
        assert!(crate::auto_closable(&items[0]));
        notes.reverse();
        assert_eq!(promote_candidates(&notes, &Policy::default(), now()), items);
        notes[1].snapshot.path = "moved.md".into();
        assert_eq!(
            promote_candidates(&notes, &Policy::default(), now())[0].id,
            items[0].id
        );
        notes.remove(1);
        assert!(promote_candidates(&notes, &Policy::default(), now()).is_empty());
        let mut notes = repeated();
        for n in &mut notes {
            n.date = Some("2026-11-01".into());
        }
        assert!(promote_candidates(&notes, &Policy::default(), now()).is_empty());
        notes[0].date = None;
        assert!(promote_candidates(&notes, &Policy::default(), now()).is_empty());
    }

    #[test]
    fn periodic_paths_and_date_titles_do_not_suggest_procedures() {
        for path in [
            "daily/a.md",
            "Journal/a.md",
            "notes/2026-11-07.md",
            "notes/20261107.md",
            "periodic/a.md",
            "weekly/a.md",
        ] {
            let mut notes = repeated();
            notes[0].snapshot.path = path.into();
            assert!(
                promote_candidates(&notes, &Policy::default(), now()).is_empty(),
                "{path}"
            );
        }
        for title in [
            "2026-11-07",
            "2026/11/07",
            "2026.11.07",
            "2026-11",
            "2026-w45",
        ] {
            let mut notes = repeated();
            notes[0].title = title.into();
            assert!(
                promote_candidates(&notes, &Policy::default(), now()).is_empty(),
                "{title}"
            );
        }
    }

    #[test]
    fn body_features_require_twenty_distinct_tokens_after_vault_boilerplate() {
        let mut notes = repeated();
        for n in &mut notes[..3] {
            n.body = "release build".into();
        }
        assert!(promote_candidates(&notes, &Policy::default(), now()).is_empty());
        let mut notes = repeated();
        notes.pop(); // All workflow terms are frequent in this small vault.
        assert!(promote_candidates(&notes, &Policy::default(), now()).is_empty());
        let mut notes = repeated();
        notes[0].body = (0..25)
            .map(|i| format!("unrelated{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(promote_candidates(&notes, &Policy::default(), now()).is_empty());
    }

    #[test]
    fn procedure_references_remove_covered_members() {
        for reference in ["notes/0", "notes/0.md", "../notes/0.md"] {
            let mut notes = repeated();
            let mut procedure = note(
                "procedures/release.md",
                "Reusable release",
                "2026-11-03",
                "Reusable workflow",
            );
            procedure.snapshot.lifecycle.kind = Some(Kind::Procedure);
            procedure.references = vec![reference.into()];
            notes.push(procedure);
            assert!(
                promote_candidates(&notes, &Policy::default(), now()).is_empty(),
                "{reference}"
            );
        }
    }

    #[test]
    fn scopes_disabled_lifecycle_and_archives_do_not_create_candidates() {
        let mut notes = repeated();
        assert!(
            promote_candidates(
                &notes,
                &Policy {
                    lifecycle_on: false,
                    ..Policy::default()
                },
                now()
            )
            .is_empty()
        );
        notes[0].snapshot.lifecycle.scope = Some("other".into());
        assert!(promote_candidates(&notes, &Policy::default(), now()).is_empty());
        notes = repeated();
        notes[0].snapshot.path = format!("{}/old.md", Policy::default().archive_dir);
        assert!(promote_candidates(&notes, &Policy::default(), now()).is_empty());
    }

    #[test]
    fn candidate_members_are_bounded_and_pairwise_similar() {
        let mut notes = repeated();
        for i in 3..45 {
            let mut n = notes[1].clone();
            n.snapshot.path = format!("notes/{i}.md");
            n.snapshot.permalink = Some(format!("note-{i}"));
            notes.push(n);
        }
        for n in &mut notes {
            if n.date.is_some() {
                n.body = workflow();
            }
        }
        let items = promote_candidates(&notes, &Policy::default(), now());
        assert!(!items.is_empty());
        for item in items {
            assert!(item.related.len() < MAX_MEMBERS);
        }
        assert!(!overlap(
            &["one".into(), "two".into()].into(),
            &["one".into(), "three".into()].into()
        ));
        assert!(overlap(
            &["one".into(), "two".into(), "three".into()].into(),
            &["one".into(), "two".into(), "three".into(), "four".into()].into()
        ));
    }

    #[test]
    fn exact_body_groups_keep_identity_when_members_change() {
        let body = "Meaningful duplicate content with several words. ".repeat(6);
        let mut notes = vec![
            note("a.md", "First", "2026-11-01", &body),
            note("b.md", "Second", "2026-11-02", &body.to_uppercase()),
            note("c.md", "Third", "2026-11-03", &body.replace(' ', "  ")),
        ];
        let found = duplicates(&notes, &Policy::default(), now());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].related, ["a.md", "b.md", "c.md"]);
        assert!(crate::auto_closable(&found[0]));
        notes.reverse();
        assert_eq!(duplicates(&notes, &Policy::default(), now()), found);
        notes.pop();
        let next = duplicates(&notes, &Policy::default(), now());
        assert_eq!(next[0].id, found[0].id);
        assert_eq!(next[0].related, ["b.md", "c.md"]);
        notes.pop();
        assert!(duplicates(&notes, &Policy::default(), now()).is_empty());
    }

    #[test]
    fn shared_headings_short_bodies_and_frontmatter_variants_are_not_duplicates() {
        let body = "Same long body with useful information. ".repeat(8);
        let a = note("a.md", "Daily note", "2026-11-01", &body);
        let b = note("b.md", "Daily note", "2026-11-02", &body);
        assert!(duplicates(&[a.clone(), b], &Policy::default(), now()).is_empty());
        let b = note(
            "b.md",
            "Daily note",
            "2026-11-02",
            &format!("{body}different"),
        );
        assert!(duplicates(&[a, b], &Policy::default(), now()).is_empty());
        let a = note("a.md", "Short", "2026-11-01", "short body");
        let b = note("b.md", "Short", "2026-11-01", "SHORT BODY");
        assert!(duplicates(&[a, b], &Policy::default(), now()).is_empty());
    }

    #[test]
    fn identical_files_are_duplicate_members_but_ineligible_notes_are_not() {
        let a = note(
            "a.md",
            "Duplicate",
            "2026-11-01",
            &"Long identical body. ".repeat(15),
        );
        let mut b = a.clone();
        b.snapshot.path = "b.md".into();
        assert_eq!(
            duplicates(&[a.clone(), b.clone()], &Policy::default(), now()).len(),
            1
        );
        b.snapshot.lifecycle.excluded = Some(true);
        assert!(duplicates(&[a.clone(), b.clone()], &Policy::default(), now()).is_empty());
        b.snapshot.lifecycle.excluded = None;
        b.snapshot.lifecycle.status = Some(Status::Archived);
        assert!(duplicates(&[a, b], &Policy::default(), now()).is_empty());
    }
}
