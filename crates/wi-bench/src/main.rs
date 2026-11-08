use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use wi_bench::{check, generate, measure, median_report, read_report, write_report};

#[cfg(feature = "korean")]
mod quality;

#[derive(Parser)]
#[command(about = "Opt-in reproducible wiki-island measurement")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}
#[derive(Subcommand)]
enum Commands {
    Generate {
        #[arg(long)]
        root: PathBuf,
        #[arg(long, default_value_t = 10_000)]
        notes: usize,
    },
    /// Generate, initialize and index a clean corpus for process measurements.
    Prepare {
        #[arg(long)]
        root: PathBuf,
        /// Per-device cache directory, not a database filename.
        #[arg(long)]
        cache: PathBuf,
        #[arg(long, default_value_t = 10_000)]
        notes: usize,
    },
    Run {
        #[arg(long)]
        root: PathBuf,
        #[arg(long)]
        cache: PathBuf,
        #[arg(long, default_value_t = 10_000)]
        notes: usize,
        #[arg(long, default_value_t = 200)]
        samples: usize,
        #[arg(long)]
        output: PathBuf,
    },
    Check {
        #[arg(long)]
        baseline: PathBuf,
        /// Existing candidate report(s); repeat for k runs, gated on the
        /// per-metric median. Omit to run fresh complete measurements.
        #[arg(long)]
        current: Vec<PathBuf>,
        /// Number of fresh complete runs when no --current is given.
        #[arg(long, default_value_t = 3)]
        runs: usize,
        /// Release wi binary needed by the process measurements on a fresh run.
        #[arg(long, default_value = "target/release/wi")]
        wi: PathBuf,
    },
    /// Combine k comparable reports into their per-metric median report.
    Median {
        #[arg(long, required = true)]
        input: Vec<PathBuf>,
        #[arg(long)]
        output: PathBuf,
    },
    #[cfg(feature = "korean")]
    Quality {
        #[arg(long)]
        dataset: PathBuf,
        #[arg(long)]
        output: PathBuf,
    },
}
fn main() -> Result<()> {
    match Cli::parse().command {
        Commands::Generate { root, notes } => println!(
            "{}",
            serde_json::to_string_pretty(&generate(&root, notes)?)?
        ),
        Commands::Prepare { root, cache, notes } => println!(
            "{}",
            serde_json::to_string_pretty(&wi_bench::prepare_process_corpus(&root, &cache, notes)?)?
        ),
        Commands::Run {
            root,
            cache,
            notes,
            samples,
            output,
        } => {
            let report = measure(&root, &cache, notes, samples)?;
            write_report(&report, &output)?;
            println!("{}", report.markdown());
        }
        Commands::Median { input, output } => {
            let reports = input
                .iter()
                .map(|p| read_report(p))
                .collect::<Result<Vec<_>>>()?;
            let report = median_report(&reports)?;
            write_report(&report, &output)?;
            println!("{}", report.markdown());
        }
        Commands::Check {
            baseline,
            current,
            runs,
            wi,
        } => {
            let base = read_report(&baseline)?;
            let reports = if current.is_empty() {
                anyhow::ensure!(runs >= 1, "--runs must be at least 1");
                (0..runs)
                    .map(|run| {
                        let current = fresh_run(&base, &wi)?;
                        let path =
                            std::env::temp_dir().join(format!("wi-bench-current-{run}.json"));
                        write_report(&current, &path)?;
                        Ok(current)
                    })
                    .collect::<Result<Vec<_>>>()?
            } else {
                current
                    .iter()
                    .map(|p| read_report(p))
                    .collect::<Result<Vec<_>>>()?
            };
            let candidate = median_report(&reports)?;
            write_report(
                &candidate,
                &std::env::temp_dir().join("wi-bench-current.json"),
            )?;
            check(&base, &candidate)?;
            println!(
                "PASS: median p95 of {} run(s) within regression thresholds for every metric",
                reports.len()
            );
        }
        #[cfg(feature = "korean")]
        Commands::Quality { dataset, output } => quality::run(&dataset, &output)?,
    }
    Ok(())
}

/// One fresh complete measurement (library + process) in a new temporary scratch directory.
fn fresh_run(base: &wi_bench::Report, wi: &std::path::Path) -> Result<wi_bench::Report> {
    let scratch = tempfile::Builder::new()
        .prefix("wi-bench-")
        .tempdir_in(std::env::temp_dir())?;
    let root = scratch.path().join("vault");
    let cache = scratch.path().join("cache");
    std::fs::create_dir_all(&cache)?;
    let index = cache.join("library.sqlite3");
    let samples = base
        .metrics
        .get("save_standard")
        .ok_or_else(|| anyhow::anyhow!("baseline missing save_standard"))?
        .n;
    let report = measure(&root, &index, base.corpus.notes, samples)?;
    #[cfg(windows)]
    {
        let _ = wi;
        let mut report = report;
        report
            .method
            .push_str("; process metrics: unknown on Windows (Python process probes unavailable)");
        Ok(report)
    }
    #[cfg(not(windows))]
    {
        let process_root = scratch.path().join("process-vault");
        let process_cache = scratch.path().join("process-cache");
        let process_corpus =
            wi_bench::prepare_process_corpus(&process_root, &process_cache, base.corpus.notes)?;
        anyhow::ensure!(process_corpus == report.corpus, "process corpus differs");
        let output = scratch.path().join("current.json");
        write_report(&report, &output)?;
        let status = std::process::Command::new("python3")
            .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/processes.py"))
            .arg("--wi")
            .arg(std::fs::canonicalize(wi)?)
            .arg("--root")
            .arg(&process_root)
            .arg("--cache")
            .arg(&process_cache)
            .arg("--report")
            .arg(&output)
            .status()?;
        anyhow::ensure!(status.success(), "process measurement failed");
        read_report(&output)
    }
}
