//! Test helper process for crash and concurrency cases.
//! Not a user command.
//!
//! - `append <root> <actor> <count>`: save `count` notes under `<actor>/`,
//!   each through blob, intent, file write and done.
//! - `flush <root>`: flush and print the report as JSON.
//! - `bench <root> <count> <standard|full>`: in a new vault, time `count`
//!   blob + ingest appends, then one flush, and print both as JSON.
#![forbid(unsafe_code)]

use std::process::ExitCode;

use std::time::Instant;

use wi_history::{
    Blob, Durability, Flusher, IngestRecord, IntentRecord, Manifest, PublisherLock, Vault,
    hash_bytes,
};

fn bench(root: &str, count: usize, mode: &str) -> Result<(), String> {
    let mode = Durability::parse(mode).ok_or("mode is standard or full")?;
    let vault = Vault::init(root)
        .map_err(|e| e.to_string())?
        .with_durability(mode);
    let start = Instant::now();
    for i in 0..count {
        let bytes = format!("# Note {i}\n");
        let guard = PublisherLock::acquire(&vault).map_err(|e| e.to_string())?;
        let blob = Blob::write_durable(&vault, bytes.as_bytes()).map_err(|e| e.to_string())?;
        Manifest::append_ingest(
            &vault,
            &guard,
            IngestRecord {
                actor: "bench".into(),
                path: format!("notes/{i}.md"),
                new_hash: hash_bytes(bytes.as_bytes()),
                new_blob: blob,
                batch: None,
            },
        )
        .map_err(|e| e.to_string())?;
    }
    let append = start.elapsed();
    let start = Instant::now();
    let report = Flusher::flush(&vault).map_err(|e| e.to_string())?;
    let flush = start.elapsed();
    println!(
        "{{\"mode\":\"{}\",\"records\":{count},\"append_s\":{:.6},\"append_ms_per_record\":{:.4},\"flush_s\":{:.6},\"flush_ms_per_commit\":{:.4},\"commits\":{},\"remaining\":{}}}",
        mode.as_str(),
        append.as_secs_f64(),
        append.as_secs_f64() * 1000.0 / count as f64,
        flush.as_secs_f64(),
        flush.as_secs_f64() * 1000.0 / report.commits.len().max(1) as f64,
        report.commits.len(),
        report.remaining
    );
    Ok(())
}

fn append(root: &str, actor: &str, count: usize) -> Result<(), String> {
    let vault = Vault::open(root).map_err(|e| e.to_string())?;
    for i in 0..count {
        let path = format!("{actor}/note-{i}.md");
        let bytes = format!("# {actor} {i}\n");
        let guard = PublisherLock::acquire(&vault).map_err(|e| e.to_string())?;
        let blob = Blob::write_durable(&vault, bytes.as_bytes()).map_err(|e| e.to_string())?;
        let seq = Manifest::append_intent(
            &vault,
            &guard,
            IntentRecord {
                op_id: Some(format!("{actor}-{i}")),
                request_digest: format!("d{i}"),
                actor: actor.to_owned(),
                path: path.clone(),
                moved_from: None,
                old_hash: None,
                new_hash: hash_bytes(bytes.as_bytes()),
                new_blob: blob,
                revert_of: None,
                batch: None,
                losses: Vec::new(),
            },
        )
        .map_err(|e| e.to_string())?;
        let full = vault.root().join(&path);
        if let Some(dir) = full.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        std::fs::write(&full, bytes.as_bytes()).map_err(|e| e.to_string())?;
        Manifest::append_done(&vault, &guard, seq).map_err(|e| e.to_string())?;
        println!("{seq}");
    }
    Ok(())
}

fn run(args: &[String]) -> Result<(), String> {
    match args {
        [cmd, root, actor, count] if cmd == "append" => {
            append(root, actor, count.parse().map_err(|_| "bad count")?)
        }
        [cmd, root] if cmd == "flush" => {
            let vault = Vault::open(root).map_err(|e| e.to_string())?;
            let report = Flusher::flush(&vault).map_err(|e| e.to_string())?;
            println!(
                "{}",
                serde_json::to_string(&report).map_err(|e| e.to_string())?
            );
            Ok(())
        }
        [cmd, root, count, mode] if cmd == "bench" => {
            bench(root, count.parse().map_err(|_| "bad count")?, mode)
        }
        _ => Err(
            "usage: append <root> <actor> <count> | flush <root> | bench <root> <count> <mode>"
                .into(),
        ),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("wi-history-testproc: {e}");
            ExitCode::FAILURE
        }
    }
}
