# Contributing

wiki-island is a Rust workspace of twelve crates; the [design](docs/DESIGN.md) has the crate map. Read the [repository contract](AGENTS.md) first.

## Setup and checks

From the repository root:

```sh
rustup toolchain install 1.99 --component clippy --component rustfmt
export PATH="$HOME/.cargo/bin:$PATH"      # non-login shells and background jobs may lack it
cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace
cargo run --bin wi -- --version
cargo run --bin wi -- init /tmp/wi-demo && cargo run --bin wi -- --vault /tmp/wi-demo write a.md --content 'hello'
```

The first line is the mechanical gate and the bar for every commit. `rust-toolchain.toml` selects the toolchain; dependency versions are in `Cargo.lock`. macOS arm64 (APFS) is the primary platform; Windows x86_64 (NTFS) runs on GitHub Actions ([VALIDATION](docs/VALIDATION.md#platform)).

Tests that open an index use a temporary cache (`WI_CACHE_DIR` or `OpenOptions::index_path`) and a temporary `HOME` (with `USERPROFILE`, `APPDATA` and `LOCALAPPDATA` on Windows), never the real ones. Crash tests set `WI_FAULT` on a child `wi` process only. Subprocess tests that need a fixed clock set `WI_NOW`. Tests never touch real host configs or the real `~/Library/LaunchAgents`.

### Linux container

```sh
docker run --rm -e RUSTUP_TOOLCHAIN=1.99.1 \
  -v "$PWD":/src:ro -v /tmp/wi-linux-target:/target rust:1.99-bookworm sh -ec '
  mkdir /work
  tar -C /src --exclude=./target --exclude=./.git -cf - . | tar -C /work -xf -
  cd /work
  rustup component add rustfmt clippy
  export CARGO_TARGET_DIR=/target
  cargo fmt --all --check
  cargo clippy --workspace --all-targets -- -D warnings
  cargo test --workspace'
```

### Windows

Windows code lives under `#[cfg(windows)]`; Unix code and tests stay as they are. On a Mac or Linux machine with mingw-w64 and the `x86_64-pc-windows-gnu` target, check that everything compiles before pushing:

```sh
cargo clippy --workspace --all-targets --target x86_64-pc-windows-gnu -- -D warnings
cargo build --tests --workspace --target x86_64-pc-windows-gnu
```

Clippy alone does not evaluate `const` assertions; the test build does. This does not run any test. Runtime tests run on GitHub Actions `windows-latest` (and Ubuntu and macOS) through `.github/workflows/ci.yml`.

### Installer

`sh scripts/tests/install_test.sh` runs `scripts/install.sh` against a local server and a temporary `HOME`; run it with `shellcheck scripts/install.sh scripts/tests/*.sh` after any change to `scripts/`. `.github/workflows/install.yml` does both on Ubuntu and macOS. The cases are in [VALIDATION](docs/VALIDATION.md#installer-and-release).

### Releases

Pushing a tag that starts with `v` runs `.github/workflows/release.yml`. It builds `wi` and `wiki-island` for `aarch64-apple-darwin`, `x86_64-apple-darwin`, `aarch64-unknown-linux-gnu`, `x86_64-unknown-linux-gnu` and `x86_64-pc-windows-msvc`, writes `SHA256SUMS` and publishes a GitHub Release with generated notes. Running the workflow by hand (`workflow_dispatch`, optionally with a `ref`) builds the same archives as workflow artifacts and publishes nothing. Linux archives are built on Ubuntu 22.04, which sets the glibc floor ([OPERATIONS](docs/OPERATIONS.md#install-and-release)). Cut a release only from a commit whose CI is green.

### Benchmarks

Measurement is opt-in and macOS only. Scratch paths must be new and outside the repository. Methods and the regression gate are in [crates/wi-bench/README.md](crates/wi-bench/README.md); targets are in [docs/PERFORMANCE.md](docs/PERFORMANCE.md).

## Code rules

- Libraries forbid unsafe code, except `wi-sys`, which holds all Windows FFI; every unsafe block there has a `SAFETY` comment. Libraries use typed errors. No `unwrap()` or `expect()` outside tests.
- Keep `wi-note` pure. Every vault write goes through `wi-store`; every fsync through `wi_history::durable` (the workspace `clippy.toml` refuses `File::sync_all` and `sync_data`).
- Each crate declares its own dependencies. Add one only with a written reason in [DEPENDENCIES](docs/DEPENDENCIES.md): what it does, what it costs, the alternative.
- Never add a model call. Network requests belong only to remote sync and view publishing (`wi sync`, `wi publish` and their background threads), outside engine locks; never on capture, search, read or MCP paths.
- Test names describe behavior. Do not put phase, review or issue codes in names, comments or assertion messages.

## Where specs live

| Change to | Update |
| --- | --- |
| Behavior and scope | [DESIGN](docs/DESIGN.md) |
| Vault layout, frontmatter, config | [FORMAT](docs/FORMAT.md) |
| Lifecycle, gates, maintenance | [KNOWLEDGE](docs/KNOWLEDGE.md) |
| Write path, history, watcher, service | [OPERATIONS](docs/OPERATIONS.md) |
| MCP tools and results | [PROTOCOL](docs/PROTOCOL.md) |
| Trust and origin | [AUTHORITY](docs/AUTHORITY.md) |
| Host registration, skills export | [INTEGRATIONS](docs/INTEGRATIONS.md), [SKILLS](docs/SKILLS.md) |
| Tests and gates | [VALIDATION](docs/VALIDATION.md) |
| Targets | [PERFORMANCE](docs/PERFORMANCE.md) |

A change to the vault layout, frontmatter, lifecycle states, scope rules, protocol or index schema must update the matching spec in the same change, say whether existing vaults keep working and how an index rebuild behaves, add a validation entry, and preserve unknown frontmatter keys and Markdown syntax it does not understand. Lint and checks report findings and never rewrite a note.

## Proposing a change

1. Describe the problem, not only the solution.
2. Name which document changes.
3. Mark every claim as a decision, a proposal, an observed fact (with source) or unknown. A performance, size or quality number is a hypothesis until it names a corpus, a method, a machine and a measurement.
4. Keep changes small. One topic per change.

## Privacy

Never commit private documents, company or personal data, personal filesystem paths, tokens or secrets.

## Commits

Format: `{category}: {description}`, at most 72 characters, English.

Categories: `feat`, `fix`, `refactor`, `docs`, `test`, `chore`, `perf`, `ci`.

The description is imperative, lowercase and has no final period. Explain why in the body when it is not obvious. Example: `docs: add vault conflict scenarios`.

## License

wiki-island is licensed under the [GNU Affero General Public License v3.0 only](LICENSE). By submitting a contribution, you agree that it is licensed under the same terms and that you have the right to submit it. Do not add code copied from projects whose licenses are incompatible with AGPL-3.0. A new dependency must use an AGPL-3.0-compatible license; record it in docs/DEPENDENCIES.md as usual.
