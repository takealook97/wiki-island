//! Index-resident lifecycle snapshots: a cold
//! current read that takes unchanged notes from the index must judge exactly
//! like one that re-reads every lifecycle file. Randomized vaults, synced and
//! stale indexes, compared on fresh handles.

mod common;

use chrono::{TimeDelta, Utc};
use common::Fixture;
use serde_json::{Value, json};
use tempfile::TempDir;
use wi_store::{OpenOptions, ProcedureMode, Query, SearchOptions, Vault};

const WORDS: [&str; 4] = ["alpha", "bravo", "charlie", "common"];
const SEEDS: u64 = 10;
const ROUNDS: usize = 12;
const NOTES: usize = 36;

/// xorshift64*: deterministic, dependency-free.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }
    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

const DIRS: [&str; 6] = [
    "memory",
    "memory/sub",
    "archive",
    "work",
    "captures",
    "notes",
];
const SOURCES: [&str; 3] = ["src/a.md", "src/b.md", "src/c.md"];

fn note_path(i: usize) -> String {
    format!("{}/n{i:02}.md", DIRS[i % DIRS.len()])
}

fn time(days: i64) -> String {
    wi_lifecycle::format_time(Utc::now() + TimeDelta::days(days))
}

fn yaml(fields: &[(&str, Value)]) -> String {
    fields.iter().map(|(k, v)| format!("{k}: {v}\n")).collect()
}

/// A random memory claim: status, kind, pins (current or stale), slots,
/// supersedes and conflict links to other notes, applicability windows.
fn memory(rng: &mut Rng, sources: &[Vec<u8>]) -> String {
    let status = *rng.pick(&[
        "active",
        "active",
        "active",
        "candidate",
        "superseded",
        "rejected",
    ]);
    let kind = *rng.pick(&["fact", "rule", "preference", "procedure", "decision"]);
    let claim_kind = *rng.pick(&["historical", "current-state", "intent"]);
    let mut fields = vec![
        ("type", json!("memory")),
        ("kind", json!(kind)),
        ("claim_kind", json!(claim_kind)),
        ("status", json!(status)),
        ("summary", json!(format!("claim {}", rng.below(1000)))),
        (
            "entities",
            json!([{"id": format!("e:{}", rng.below(3)), "label": null, "source": null}]),
        ),
        ("scope", json!(*rng.pick(&["work", "personal"]))),
        ("observed_at", json!(time(-(rng.below(60) as i64)))),
        (
            "origin",
            json!({"kind": *rng.pick(&["user", "agent"]), "locator": "file", "captured_at": time(-2)}),
        ),
    ];
    let supersedes: Vec<String> = (0..rng.below(2))
        .map(|_| note_path(rng.below(NOTES)))
        .collect();
    let until = rng.chance(20).then(|| time(*rng.pick(&[-3, 5])));
    fields.push((
        "applicability",
        json!({"from": null, "until": until, "conditions": [], "supersedes": supersedes}),
    ));
    if rng.chance(70) {
        let which = rng.below(SOURCES.len());
        let bytes = &sources[which];
        let hash = if rng.chance(80) {
            wi_lifecycle::sha256(bytes)
        } else {
            wi_lifecycle::sha256(b"stale")
        };
        fields.push((
            "sources",
            json!([{"ref": SOURCES[which], "commit": null, "pending": true, "source_hash": hash,
                    "span_hash": hash, "locator": {"span": {"start": 0, "end": bytes.len()}}}]),
        ));
    } else {
        fields.push(("sources", json!([])));
    }
    if rng.chance(40) {
        fields.push(("slot", json!(format!("slot-{}", rng.below(2)))));
        fields.push((
            "value",
            if rng.chance(15) {
                Value::Null
            } else {
                json!(rng.below(3))
            },
        ));
    }
    if rng.chance(15) {
        fields.push(("conflict_with", json!([note_path(rng.below(NOTES))])));
    }
    if rng.chance(10) {
        fields.push(("excluded", json!(true)));
    }
    if rng.chance(15) {
        fields.push((
            "check",
            json!({"checked_at": time(-(rng.below(90) as i64)), "result": "unchanged",
                   "integrity_at": null, "volatile": rng.chance(50), "ttl": "P7D"}),
        ));
    }
    if rng.chance(30) {
        fields.push((
            "adoption",
            json!({"at": time(-1), "by": "engine", "gates": {"g": rng.chance(50), "n": null},
                   "literal": rng.chance(30)}),
        ));
    }
    format!("---\n{}---\n", yaml(&fields))
}

fn content(rng: &mut Rng, sources: &[Vec<u8>]) -> String {
    let words: Vec<&str> = (0..2).map(|_| *rng.pick(&WORDS)).collect();
    let body = format!(
        "{} {} common body {}.\n",
        words[0],
        words[1],
        rng.below(100)
    );
    let head = match rng.below(10) {
        0..=5 => memory(rng, sources),
        6 => format!(
            "---\ntype: capture\ncapture_status: {}\n---\n",
            rng.pick(&["pending", "resolved"])
        ),
        7 => format!(
            "---\ntype: work\nwork_id: w{}\nwork_kind: conflict\nwork: {{\"state\": \"ready\"}}\n---\n",
            rng.below(4)
        ),
        8 => "---\ntype: memory\nstatus: [unterminated\n---\n".to_owned(),
        _ => "---\ntitle: plain\n---\n".to_owned(),
    };
    format!("{head}{body}")
}

fn write_raw(vault: &Vault, path: &str, text: &str) {
    let full = vault.root().join(path);
    std::fs::create_dir_all(full.parent().unwrap()).unwrap();
    std::fs::write(full, text).unwrap();
}

/// Mutate the vault on disk without the write path: rewrites (sometimes
/// byte-identical, which changes the stamp but not the hash), deletions,
/// new notes and source changes.
fn mutate(vault: &Vault, rng: &mut Rng, sources: &mut [Vec<u8>]) {
    for _ in 0..1 + rng.below(6) {
        let path = note_path(rng.below(NOTES));
        let full = vault.root().join(&path);
        match rng.below(10) {
            0 => {
                let _ = std::fs::remove_file(&full);
            }
            1 => {
                if let Ok(bytes) = std::fs::read(&full) {
                    std::fs::write(&full, bytes).unwrap();
                }
            }
            _ => {
                let text = content(rng, sources);
                write_raw(vault, &path, &text);
            }
        }
    }
    if rng.chance(20) {
        let which = rng.below(SOURCES.len());
        sources[which] = format!("# Source {which}\n\nrevision {}\n", rng.below(1000)).into_bytes();
        write_raw(
            vault,
            SOURCES[which],
            &String::from_utf8_lossy(&sources[which]),
        );
    }
}

/// Everything a cold current read delivers, as JSON.
fn observe(vault: &Vault, query: &str) -> Value {
    let found = vault
        .search_current(
            &Query::text(query),
            &Value::Null,
            SearchOptions { catch_up: false },
        )
        .unwrap();
    json!(found)
}

fn observe_lists(vault: &Vault) -> Value {
    let rules = vault.current_rules().unwrap();
    let procedures = vault.procedures(ProcedureMode::Direct).unwrap();
    json!({"rules": rules, "procedures": procedures})
}

/// Two fresh handles on identical private copies of the index (a search
/// that finds a changed hit updates its index, which must not leak from one
/// side to the other): one trusts materialized snapshots, one reads files.
fn handles(fixture: &Fixture, now: wi_lifecycle::Timestamp) -> (Vault, Vault, TempDir) {
    let db = fixture.cache.path().join("index.sqlite3");
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
        .unwrap();
    let copies = TempDir::new().unwrap();
    let open = |name: &str| {
        let path = copies.path().join(name);
        std::fs::copy(&db, &path).unwrap();
        let vault = Vault::open_with(
            fixture.root_input(),
            OpenOptions {
                index_path: Some(path),
            },
        )
        .unwrap();
        vault.set_lifecycle_clock(Some(now));
        vault
    };
    let materialized = open("materialized.sqlite3");
    let files = open("files.sqlite3");
    files.set_lifecycle_files_only(true);
    (materialized, files, copies)
}

#[test]
fn index_snapshots_judge_like_file_reads_on_random_vaults() {
    let mut cases = 0usize;
    let mut stale_rounds = 0usize;
    for seed in 1..=SEEDS {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ seed.wrapping_mul(0x1000_0001));
        let (fixture, vault) = Fixture::new();
        let mut sources: Vec<Vec<u8>> = SOURCES
            .iter()
            .enumerate()
            .map(|(i, _)| format!("# Source {i}\n\nport 443\n").into_bytes())
            .collect();
        for (path, bytes) in SOURCES.iter().zip(&sources) {
            write_raw(&vault, path, &String::from_utf8_lossy(bytes));
        }
        for i in 0..NOTES {
            let text = content(&mut rng, &sources);
            write_raw(&vault, &note_path(i), &text);
        }
        vault.reindex().unwrap();
        let now = Utc::now();
        for round in 0..ROUNDS {
            if round > 0 {
                mutate(&vault, &mut rng, &mut sources);
                if rng.chance(60) {
                    vault.catch_up().unwrap();
                } else {
                    stale_rounds += 1;
                }
            }
            let (materialized, files, _copies) = handles(&fixture, now);
            for query in WORDS {
                assert_eq!(
                    observe(&materialized, query),
                    observe(&files, query),
                    "seed {seed} round {round} query {query}"
                );
                cases += 1;
            }
            let (materialized, files, _copies) = handles(&fixture, now);
            assert_eq!(
                observe_lists(&materialized),
                observe_lists(&files),
                "seed {seed} round {round} rules/procedures"
            );
            cases += 1;
        }
    }
    assert!(cases >= 500, "{cases} cases");
    assert!(stale_rounds > 0, "no stale-index round was exercised");
    println!("r4 differential: {cases} cases, {stale_rounds} stale-index rounds");
}

/// The comparison above is not vacuous: a fresh handle reads unchanged
/// notes from the index (a doctored row changes its judgment), and a row of
/// another format is ignored.
#[test]
fn unchanged_notes_come_from_the_index_and_foreign_formats_are_ignored() {
    let (fixture, vault) = Fixture::new();
    let claim = "---\ntype: memory\nkind: fact\nclaim_kind: historical\nstatus: active\nsummary: alpha claim\nentities: [{\"id\": \"e:1\", \"label\": null, \"source\": null}]\nscope: work\napplicability: {\"from\": null, \"until\": null, \"conditions\": [], \"supersedes\": []}\nobserved_at: 2026-01-01T00:00:00Z\norigin: {\"kind\": \"user\", \"locator\": \"file\", \"captured_at\": null}\nsources: []\n---\nalpha body.\n";
    write_raw(&vault, "memory/a.md", claim);
    vault.reindex().unwrap();
    let db = fixture.cache.path().join("index.sqlite3");
    let connection = rusqlite::Connection::open(&db).unwrap();
    let doctored: String = connection
        .query_row(
            "SELECT snapshot FROM lifecycle_snapshots WHERE path='memory/a.md'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(doctored.contains("\"kind\":\"fact\""));
    // Present the note to the index as a rule: only a handle that trusts
    // the row lists it among rule-like notes.
    connection
        .execute(
            "UPDATE lifecycle_snapshots SET snapshot=?1 WHERE path='memory/a.md'",
            [doctored.replace("\"kind\":\"fact\"", "\"kind\":\"rule\"")],
        )
        .unwrap();
    let (materialized, files, _copies) = handles(&fixture, Utc::now());
    assert_ne!(observe_lists(&materialized), observe_lists(&files));
    connection
        .execute(
            "UPDATE lifecycle_snapshots SET format='other' WHERE path='memory/a.md'",
            [],
        )
        .unwrap();
    let (materialized, files, _copies) = handles(&fixture, Utc::now());
    assert_eq!(observe_lists(&materialized), observe_lists(&files));
}
