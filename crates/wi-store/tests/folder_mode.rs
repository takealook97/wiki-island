//! S1, S2, S4: a vault in a file-synced folder keeps its history and
//! per-device state in the data directory, honours its exclude rules, and
//! runs lifecycle maintenance only on the primary device.

mod common;

use std::path::{Path, PathBuf};

use tempfile::TempDir;
use wi_store::{
    Ingress, MaintainOptions, OpenOptions, Placement, StorageMode, Vault, WriteRequest,
};

struct Folder {
    _dir: TempDir,
    root: PathBuf,
    data: PathBuf,
    cache: PathBuf,
}

impl Folder {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("notes");
        let data = dir.path().join("data");
        let cache = dir.path().join("cache/index.sqlite3");
        Self {
            _dir: dir,
            root,
            data,
            cache,
        }
    }
    fn options(&self) -> OpenOptions {
        OpenOptions {
            index_path: Some(self.cache.clone()),
        }
    }
    fn placement(&self, data: &Path) -> Placement {
        Placement {
            data_root: Some(data.to_path_buf()),
            storage: Some(StorageMode::Folder),
        }
    }
    fn init(&self) -> Vault {
        Vault::init_placed(&self.root, self.options(), self.placement(&self.data))
            .unwrap()
            .0
    }
    fn open_as(&self, data: &Path) -> Vault {
        Vault::open_placed(&self.root, self.options(), self.placement(data)).unwrap()
    }
}

fn write(vault: &Vault, path: &str, content: &str) {
    vault
        .write(WriteRequest {
            path: path.into(),
            content: content.as_bytes().to_vec(),
            expected_checksum: None,
            op_id: None,
            ingress: Ingress::Cli,
        })
        .unwrap();
}

#[test]
fn folder_mode_vault_holds_only_notes_and_engine_folder() {
    let f = Folder::new();
    let vault = f.init();
    assert_eq!(vault.storage_mode(), StorageMode::Folder);
    assert!(!f.root.join(".git").exists());
    assert!(!f.root.join(".gitignore").exists());
    let state = f.data.join("vaults").join(vault.vault_id());
    assert!(state.join("git").is_dir());
    assert_eq!(
        vault.local_dir(),
        state.canonicalize().unwrap().join("local")
    );
    write(&vault, "a.md", "# a\n");
    vault.flush().unwrap();
    assert_eq!(vault.history("a.md").unwrap().len(), 1);
    let names: Vec<_> = std::fs::read_dir(&f.root)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    let mut names = names;
    names.sort();
    assert_eq!(names, vec![".wiki-island", "a.md"]);
}

#[test]
fn folder_mode_exclude_rules_are_honoured_by_ingest_and_index() {
    let f = Folder::new();
    let vault = f.init();
    let exclude_path = f
        .data
        .join("vaults")
        .join(vault.vault_id())
        .join("git/info/exclude");
    let mut exclude = std::fs::read_to_string(&exclude_path).unwrap();
    assert!(exclude.contains(".obsidian/"), "{exclude}");
    // An owner rule that only the exclude file knows.
    exclude.push_str("private/\n");
    std::fs::write(&exclude_path, exclude).unwrap();
    std::fs::create_dir_all(f.root.join("private")).unwrap();
    std::fs::write(f.root.join("private/secret.md"), "x\n").unwrap();
    std::fs::write(f.root.join("kept.md"), "kept\n").unwrap();
    let report = vault.reconcile().unwrap();
    let text = format!("{report:?}");
    assert!(!text.contains("private/secret.md"), "{text}");
    assert!(text.contains("kept.md"), "{text}");
    assert!(wi_store::ignored(&f.root, Path::new("private/secret.md")));
    let single = vault.ingest_path("private/secret.md").unwrap();
    assert!(
        matches!(single.outcome, wi_store::IngestOutcome::Skipped { .. }),
        "{single:?}"
    );
    vault.flush().unwrap();
    assert!(vault.history("private/secret.md").unwrap().is_empty());
}

#[test]
fn only_the_primary_device_runs_lifecycle_maintenance() {
    let f = Folder::new();
    let vault = f.init();
    let me = vault.device_id().unwrap();
    assert_eq!(vault.primary_device().as_deref(), Some(me.as_str()));
    assert!(vault.is_primary_device());
    vault
        .maintain(&MaintainOptions::default())
        .expect("primary maintains");
    let generation = vault.generation().unwrap();
    drop(vault);

    // A second device: same notes folder, its own data directory.
    let other_data = f.data.parent().unwrap().join("other-data");
    let source = f.data.join("vaults");
    let id = std::fs::read_dir(&source)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .file_name();
    // Its history is its own; a copy of this device's stands in for it.
    std::fs::create_dir_all(other_data.join("vaults")).unwrap();
    let status = std::process::Command::new("cp")
        .arg("-R")
        .arg(source.join(&id))
        .arg(other_data.join("vaults").join(&id))
        .status()
        .unwrap();
    assert!(status.success());
    let other = f.open_as(&other_data);
    assert!(!other.is_primary_device());
    let report = other.maintain(&MaintainOptions::default()).unwrap();
    assert_eq!(report.work_created, 0);
    assert_eq!(report.migrated, 0);
    assert_eq!(other.generation().unwrap(), generation, "nothing written");
    // `wi setup --primary` takes over.
    other.set_primary_device().unwrap();
    assert!(other.is_primary_device());
}

#[test]
fn git_mode_vault_opens_unchanged_without_a_data_directory() {
    let (fx, vault) = common::Fixture::new();
    assert_eq!(vault.storage_mode(), StorageMode::Git);
    assert!(vault.is_primary_device());
    assert!(vault.root().join(".git").is_dir());
    assert!(vault.root().join(".gitignore").is_file());
    assert_eq!(vault.local_dir(), vault.root().join(".wiki-island/local"));
    drop(vault);
    let again = fx.open();
    assert_eq!(again.storage_mode(), StorageMode::Git);
    assert!(
        again.primary_device().is_none(),
        "git mode records no primary"
    );
}

#[test]
fn init_inside_a_file_synced_folder_chooses_folder_mode() {
    let dir = TempDir::new().unwrap();
    let root = dir
        .path()
        .join("Library/Mobile Documents/com~apple~CloudDocs/notes");
    let data = dir.path().join("data");
    let options = OpenOptions {
        index_path: Some(dir.path().join("index.sqlite3")),
    };
    let placement = Placement {
        data_root: Some(data.clone()),
        storage: None,
    };
    let (vault, _) = Vault::init_placed(&root, options, placement).unwrap();
    assert_eq!(vault.storage_mode(), StorageMode::Folder);
    assert!(!root.join(".git").exists());
    assert!(
        data.join("vaults")
            .join(vault.vault_id())
            .join("git")
            .is_dir()
    );
}

#[test]
fn gitignore_reinclusion_beats_external_exclude_in_reconcile() {
    let f = Folder::new();
    let vault = f.init();
    let exclude = vault.history_vault().exclude_file();
    let mut rules = std::fs::read_to_string(&exclude).unwrap();
    rules.push_str("*.md\n");
    std::fs::write(exclude, rules).unwrap();
    std::fs::write(f.root.join(".gitignore"), "!kept.md\n").unwrap();
    std::fs::write(f.root.join("kept.md"), "kept\n").unwrap();
    assert!(!wi_store::ignored(vault.root(), Path::new("kept.md")));
    let report = vault.reconcile().unwrap();
    assert!(
        report.ingested.iter().any(|r| r.path == "kept.md"),
        "{report:?}"
    );
}

#[test]
fn gitignore_reinclusion_beats_external_exclude_in_index() {
    let f = Folder::new();
    let vault = f.init();
    let exclude = vault.history_vault().exclude_file();
    let mut rules = std::fs::read_to_string(&exclude).unwrap();
    rules.push_str("*.md\n");
    std::fs::write(exclude, rules).unwrap();
    std::fs::write(f.root.join(".gitignore"), "!kept.md\n").unwrap();
    std::fs::write(f.root.join("kept.md"), "kept\n").unwrap();
    let report = vault.catch_up().unwrap();
    assert_eq!(report.discovered, 1, "{report:?}");
}

#[test]
fn init_through_a_symlink_to_a_synced_folder_chooses_folder_mode() {
    let tmp = TempDir::new().unwrap();
    let cloud = tmp.path().join("Dropbox");
    std::fs::create_dir(&cloud).unwrap();
    let alias = tmp.path().join("alias");
    std::os::unix::fs::symlink(&cloud, &alias).unwrap();
    let (vault, _) = Vault::init_placed(
        alias.join("new-vault"),
        OpenOptions {
            index_path: Some(tmp.path().join("index.db")),
        },
        Placement {
            data_root: Some(tmp.path().join("data")),
            storage: None,
        },
    )
    .unwrap();
    assert_eq!(vault.storage_mode(), StorageMode::Folder);
    assert!(!cloud.join("new-vault/.git").exists());
}

/// Sizes of every object reachable from any ref in `git_dir`.
fn reachable_sizes(git_dir: &Path) -> Vec<u64> {
    let list = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(git_dir)
        .args(["rev-list", "--objects", "--all"])
        .output()
        .unwrap();
    assert!(list.status.success());
    let mut check = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(git_dir)
        .args(["cat-file", "--batch-check=%(objectsize)"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let ids: String = String::from_utf8(list.stdout)
        .unwrap()
        .lines()
        .map(|l| format!("{}\n", l.split(' ').next().unwrap()))
        .collect();
    std::io::Write::write_all(check.stdin.as_mut().unwrap(), ids.as_bytes()).unwrap();
    let out = check.wait_with_output().unwrap();
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| l.parse().unwrap())
        .collect()
}

#[test]
fn no_path_adds_a_file_over_fifty_megabytes_to_history_without_a_remote() {
    const LIMIT: u64 = 50 * 1024 * 1024;
    let f = Folder::new();
    let vault = f.init();
    let git_dir = f.data.join("vaults").join(vault.vault_id()).join("git");
    // Ingest: an editor or file sync drops a large file into the folder.
    let file = std::fs::File::create(f.root.join("video.mov")).unwrap();
    file.set_len(LIMIT + 1).unwrap();
    vault.reconcile().unwrap();
    // Capture and `wi write` both reach the engine write path.
    let big = "word ".repeat((LIMIT / 5 + 2) as usize);
    let _ = vault.capture(
        &wi_store::CaptureInput {
            text: big.clone(),
            title: Some("Big capture".into()),
            no_memory_reason: None,
            snapshot: None,
            proposals: vec![],
        },
        Ingress::Cli,
    );
    write(&vault, "big.md", &big);
    write(&vault, "small.md", "# small\n");
    vault.flush().unwrap();
    vault.reconcile().unwrap();
    vault.flush().unwrap();
    assert!(f.root.join("big.md").is_file());
    assert_eq!(vault.history("small.md").unwrap().len(), 1);
    assert!(vault.history("big.md").unwrap().is_empty());
    assert!(vault.history("video.mov").unwrap().is_empty());
    assert!(reachable_sizes(&git_dir).iter().all(|&s| s <= LIMIT));
    let exclude = std::fs::read_to_string(git_dir.join("info/exclude")).unwrap();
    for rule in ["/video.mov", "/big.md"] {
        assert!(exclude.lines().any(|l| l == rule), "{exclude}");
    }
    let requests = vault.work_requests().unwrap();
    for path in ["video.mov", "big.md"] {
        assert_eq!(requests.iter().filter(|r| r.path == path).count(), 1);
    }
}
