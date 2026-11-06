use serde_json::json;
use tempfile::TempDir;
use wi_export::skills::{self, SkillTarget};
use wi_store::{OpenOptions, Vault};

struct Fixture {
    temp: TempDir,
    vault: Vault,
}
impl Fixture {
    fn new() -> Self {
        let temp = TempDir::new().unwrap();
        let (vault, _) = Vault::init_with(
            temp.path().join("vault"),
            OpenOptions {
                index_path: Some(temp.path().join("cache")),
            },
        )
        .unwrap();
        vault.set_lifecycle_clock(Some(
            wi_lifecycle::parse_time("2026-11-05T12:00:00Z").unwrap(),
        ));
        Self { temp, vault }
    }
    fn home(&self) -> std::path::PathBuf {
        self.temp.path().join("home")
    }
    fn seed(&self, path: &str, id: &str, title: &str, extra: serde_json::Value) {
        let mut fields = json!({"type":"memory","kind":"procedure","skill_id":id,"title":title,"claim_kind":"intent","status":"active","summary":"Follow this procedure. Extra explanation.","entities":[{"id":"owner:generic"}],"scope":"personal","origin":{"kind":"user","locator":"file","captured_at":"2026-11-01T00:00:00Z"},"applicability":{"conditions":[],"supersedes":[]},"sources":[]});
        for (k, v) in extra.as_object().unwrap() {
            fields[k] = v.clone();
        }
        let text = format!(
            "---\n{}---\n\n# Steps\n\n1. Read the note.\n2. Keep every qualifier.\n",
            fields
                .as_object()
                .unwrap()
                .iter()
                .map(|(k, v)| format!("{k}: {v}\n"))
                .collect::<String>()
        );
        std::fs::write(self.vault.root().join(path), text).unwrap();
    }
}
fn skill_name(slug: &str, id: &str) -> String {
    let hash = wi_lifecycle::sha256(id.as_bytes());
    format!("wi-{slug}-{}", &hash[7..15])
}
const TARGETS: &[SkillTarget] = &[SkillTarget::Claude, SkillTarget::Agents];
#[test]
fn skills_preview_sync_incremental_update_and_inactive_cleanup() {
    let f = Fixture::new();
    f.seed("proc.md", "demo.read", "Read a note", json!({}));
    f.seed(
        "excluded.md",
        "demo.excluded",
        "Excluded",
        json!({"excluded":true,"exclusion_reason":"do not execute"}),
    );
    f.seed(
        "candidate.md",
        "demo.candidate",
        "Candidate",
        json!({"status":"candidate"}),
    );
    let home = f.home();
    let preview = skills::sync(&f.vault, &home, TARGETS, false).unwrap();
    assert_eq!(preview.created, 2);
    assert!(!home.exists());
    let report = skills::sync(&f.vault, &home, TARGETS, true).unwrap();
    assert_eq!(report.created, 2);
    for target in TARGETS {
        let path = target
            .root(&home)
            .join(skill_name("read-a-note", "demo.read"))
            .join("SKILL.md");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.starts_with(&format!(
                "---\nname: \"{}\"\ndescription: \"Follow this procedure.\"\n---",
                skill_name("read-a-note", "demo.read")
            )),
            "{text}"
        );
        assert!(text.contains("# Steps\n\n1. Read the note."));
        assert!(text.contains("skill_id=\"demo.read\" checksum=sha256:"));
        assert!(path.parent().unwrap().join(".wi-owned").exists());
        assert!(!target.root(&home).join("wi-excluded").exists());
    }
    assert_eq!(
        skills::sync(&f.vault, &home, TARGETS, true)
            .unwrap()
            .unchanged,
        2
    );
    f.seed(
        "proc.md",
        "demo.read",
        "Read a note",
        json!({"summary":"Use the updated procedure."}),
    );
    assert_eq!(
        skills::sync(&f.vault, &home, TARGETS, true)
            .unwrap()
            .updated,
        2
    );
    f.seed(
        "proc.md",
        "demo.read",
        "Read a note",
        json!({"status":"archived"}),
    );
    assert_eq!(
        skills::sync(&f.vault, &home, TARGETS, true)
            .unwrap()
            .removed,
        2
    );
    assert_eq!(
        skills::sync(&f.vault, &home, TARGETS, true)
            .unwrap()
            .removed,
        0
    );
}
#[test]
fn skills_keep_foreign_directories_extra_files_and_other_vault_ownership() {
    let f = Fixture::new();
    f.seed("proc.md", "demo.read", "Read a note", json!({}));
    let home = f.home();
    let foreign = SkillTarget::Claude
        .root(&home)
        .join(skill_name("read-a-note", "demo.read"));
    std::fs::create_dir_all(&foreign).unwrap();
    std::fs::write(foreign.join("SKILL.md"), "Owner text").unwrap();
    let report = skills::sync(&f.vault, &home, TARGETS, true).unwrap();
    assert_eq!(report.created, 2);
    assert_eq!(report.renamed.len(), 1);
    assert!(report.skipped.is_empty());
    let owned = SkillTarget::Agents
        .root(&home)
        .join(skill_name("read-a-note", "demo.read"));
    std::fs::write(owned.join("notes.txt"), "Keep this too").unwrap();
    let report = skills::uninstall(&f.vault, &home).unwrap();
    assert_eq!(report.removed, 2);
    assert!(!owned.join("SKILL.md").exists());
    assert!(owned.join("notes.txt").exists());
    assert_eq!(
        std::fs::read_to_string(foreign.join("SKILL.md")).unwrap(),
        "Owner text"
    );
    std::fs::remove_file(owned.join("notes.txt")).unwrap();
    std::fs::write(owned.join("SKILL.md"), "Other vault skill").unwrap();
    std::fs::write(
        owned.join(".wi-owned"),
        r#"{"vault_id":"another-vault","skill_id":"demo.read"}"#,
    )
    .unwrap();
    assert_eq!(skills::uninstall(&f.vault, &home).unwrap().removed, 0);
    assert!(owned.join("SKILL.md").exists());
}
#[test]
fn skills_config_is_incremental_preserves_unknown_keys_and_can_be_disabled() {
    let f = Fixture::new();
    let path = f.vault.root().join(".wiki-island/config.toml");
    let text = std::fs::read_to_string(&path).unwrap();
    std::fs::write(
        &path,
        format!("# keep this comment\n{text}\ncustom = 'kept'\n"),
    )
    .unwrap();
    skills::enable(&f.vault, &[SkillTarget::Agents]).unwrap();
    skills::enable(&f.vault, &[SkillTarget::Claude]).unwrap();
    assert_eq!(skills::configured(&f.vault), TARGETS);
    let before = std::fs::read_to_string(&path).unwrap();
    skills::enable(&f.vault, TARGETS).unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    assert!(before.starts_with("# keep this comment\n"));
    assert!(before.contains("custom = 'kept'"));
    skills::disable(&f.vault).unwrap();
    assert!(skills::configured(&f.vault).is_empty());
}
#[test]
fn colliding_titles_have_unique_names_and_duplicate_skill_ids_are_omitted() {
    let f = Fixture::new();
    f.seed("a.md", "demo.a", "Same title", json!({}));
    f.seed("b.md", "demo.b", "Same title", json!({}));
    f.seed("c.md", "demo.duplicate", "First title", json!({}));
    f.seed("d.md", "demo.duplicate", "Second title", json!({}));
    let report = skills::sync(&f.vault, &f.home(), TARGETS, true).unwrap();
    assert_eq!(report.created, 4);
    let root = SkillTarget::Agents.root(&f.home());
    let dirs = std::fs::read_dir(root)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert_eq!(dirs.len(), 2);
    assert!(dirs.iter().all(|name| name.starts_with("wi-same-title-")));
}
#[cfg(unix)]
#[test]
fn skill_export_never_follows_directory_marker_or_skill_symlinks() {
    use std::os::unix::fs::symlink;
    let f = Fixture::new();
    f.seed("proc.md", "demo.read", "Read a note", json!({}));
    let home = f.home();
    let root = SkillTarget::Agents.root(&home);
    std::fs::create_dir_all(&root).unwrap();
    let outside = f.temp.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("SKILL.md"), "private").unwrap();
    let dir = root.join(skill_name("read-a-note", "demo.read"));
    symlink(&outside, &dir).unwrap();
    assert_eq!(
        skills::sync(&f.vault, &home, &[SkillTarget::Agents], true)
            .unwrap()
            .skipped,
        vec![dir.clone()]
    );
    assert_eq!(
        std::fs::read_to_string(outside.join("SKILL.md")).unwrap(),
        "private"
    );
    std::fs::remove_file(&dir).unwrap();
    skills::sync(&f.vault, &home, &[SkillTarget::Agents], true).unwrap();
    std::fs::remove_file(dir.join("SKILL.md")).unwrap();
    symlink(outside.join("SKILL.md"), dir.join("SKILL.md")).unwrap();
    assert_eq!(
        skills::sync(&f.vault, &home, &[SkillTarget::Agents], true)
            .unwrap()
            .skipped
            .len(),
        1
    );
    assert_eq!(skills::uninstall(&f.vault, &home).unwrap().removed, 0);
}

#[test]
fn skill_names_stay_stable_when_non_ascii_procedures_are_added() {
    let f = Fixture::new();
    f.seed("a.md", "demo.a", "배포 절차", json!({}));
    skills::sync(&f.vault, &f.home(), TARGETS, true).unwrap();
    let root = SkillTarget::Agents.root(&f.home());
    let first = std::fs::read_dir(&root)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let name = first.file_name().unwrap().to_str().unwrap();
    let hash = wi_lifecycle::sha256(b"demo.a");
    assert_eq!(name, format!("wi--{}", &hash[7..15]));
    f.seed("b.md", "demo.b", "검토 절차", json!({}));
    let report = skills::sync(&f.vault, &f.home(), TARGETS, true).unwrap();
    assert_eq!(report.removed, 0);
    assert!(first.join("SKILL.md").is_file());
}
#[test]
fn retired_skills_remove_owned_files_and_keep_foreign_files() {
    let f = Fixture::new();
    f.seed("a.md", "demo.a", "Deploy", json!({}));
    skills::sync(&f.vault, &f.home(), TARGETS, true).unwrap();
    let root = SkillTarget::Agents.root(&f.home());
    let dir = std::fs::read_dir(&root)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    std::fs::write(dir.join("notes.txt"), "keep").unwrap();
    f.seed("a.md", "demo.a", "Deploy", json!({"status":"superseded"}));
    skills::sync(&f.vault, &f.home(), TARGETS, true).unwrap();
    assert!(!dir.join("SKILL.md").exists());
    assert!(!dir.join(".wi-owned").exists());
    assert_eq!(
        std::fs::read_to_string(dir.join("notes.txt")).unwrap(),
        "keep"
    );
}

#[test]
fn foreign_skill_name_collision_uses_a_stable_alternative_and_reports_it() {
    let f = Fixture::new();
    f.seed("a.md", "demo.a", "Deploy", json!({}));
    let home = f.home();
    skills::sync(&f.vault, &home, TARGETS, true).unwrap();
    let root = SkillTarget::Agents.root(&home);
    let dir = std::fs::read_dir(&root)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    std::fs::remove_file(dir.join(".wi-owned")).unwrap();
    std::fs::write(dir.join("SKILL.md"), "User skill").unwrap();
    let report =
        serde_json::to_value(skills::sync(&f.vault, &home, TARGETS, true).unwrap()).unwrap();
    assert_eq!(report["created"], 1);
    assert_eq!(report["renamed"].as_array().unwrap().len(), 1);
    assert_eq!(
        std::fs::read_to_string(dir.join("SKILL.md")).unwrap(),
        "User skill"
    );
    let next = skills::sync(&f.vault, &home, TARGETS, true).unwrap();
    assert_eq!(next.created, 0);
    assert_eq!(next.removed, 0);
}

#[test]
fn legacy_owned_names_migrate_once_and_title_changes_keep_the_assigned_name() {
    let f = Fixture::new();
    f.seed("a.md", "demo.a", "Deploy", json!({}));
    let home = f.home();
    let legacy = SkillTarget::Agents.root(&home).join("wi-deploy");
    std::fs::create_dir_all(&legacy).unwrap();
    std::fs::write(legacy.join("SKILL.md"), "legacy skill").unwrap();
    std::fs::write(
        legacy.join(".wi-owned"),
        serde_json::to_vec(&json!({"vault_id": f.vault.vault_id(), "skill_id":"demo.a"})).unwrap(),
    )
    .unwrap();
    let first = skills::sync(&f.vault, &home, &[SkillTarget::Agents], true).unwrap();
    assert_eq!(first.created, 1);
    assert_eq!(first.removed, 1);
    assert!(!legacy.exists());
    let dir = SkillTarget::Agents
        .root(&home)
        .join(skill_name("deploy", "demo.a"));
    assert!(dir.join("SKILL.md").exists());
    f.seed("a.md", "demo.a", "New title", json!({}));
    let next = skills::sync(&f.vault, &home, &[SkillTarget::Agents], true).unwrap();
    assert_eq!(next.created, 0);
    assert_eq!(next.removed, 0);
    assert!(dir.join("SKILL.md").exists());
}

#[test]
fn hosts_without_skill_discovery_use_only_mcp_resources() {
    use wi_export::hosts::HostId;
    for host in [
        HostId::ClaudeDesktop,
        HostId::Windsurf,
        HostId::Vscode,
        HostId::Zed,
    ] {
        assert_eq!(SkillTarget::for_host(host), None);
    }
    assert_eq!(
        SkillTarget::for_host(HostId::Claude),
        Some(SkillTarget::Claude)
    );
    for host in [
        HostId::Codex,
        HostId::Gemini,
        HostId::Agy,
        HostId::Cursor,
        HostId::Opencode,
    ] {
        assert_eq!(SkillTarget::for_host(host), Some(SkillTarget::Agents));
    }
}

#[test]
fn independent_sync_digest_survives_other_process_maintenance_and_target_changes() {
    let f = Fixture::new();
    f.seed("proc.md", "demo.read", "Read a note", json!({}));
    let home = f.home();
    wi_export::local::Targets {
        home: home.clone(),
        hosts: vec![],
        skill_dirs: TARGETS.iter().map(|target| target.root(&home)).collect(),
        cache_root: home.join("cache"),
    }
    .save(f.vault.root())
    .unwrap();
    skills::enable(&f.vault, &[SkillTarget::Claude]).unwrap();
    assert!(skills::sync_enabled(&f.vault, &home).unwrap().is_some());
    assert!(!SkillTarget::Agents.root(&home).exists());
    assert!(skills::sync_enabled(&f.vault, &home).unwrap().is_none());
    f.seed(
        "proc.md",
        "demo.read",
        "Read a note",
        json!({"summary":"Updated export marker."}),
    );
    f.vault
        .maintain(&wi_store::MaintainOptions::default())
        .unwrap();
    // The maintenance digest cannot swallow a pending export.
    assert!(skills::sync_enabled(&f.vault, &home).unwrap().is_some());
    let path = SkillTarget::Claude
        .root(&home)
        .join(skill_name("read-a-note", "demo.read"))
        .join("SKILL.md");
    assert!(
        std::fs::read_to_string(path)
            .unwrap()
            .contains("Updated export marker.")
    );
    assert!(skills::sync_enabled(&f.vault, &home).unwrap().is_none());
    skills::enable(&f.vault, &[SkillTarget::Agents]).unwrap();
    assert!(skills::sync_enabled(&f.vault, &home).unwrap().is_some());
    assert!(
        SkillTarget::Agents
            .root(&home)
            .join(skill_name("read-a-note", "demo.read"))
            .join("SKILL.md")
            .is_file()
    );
}

#[test]
fn recorded_sync_ignores_a_different_home_and_does_not_invent_missing_destinations() {
    let f = Fixture::new();
    f.seed("proc.md", "demo.read", "Read a note", json!({}));
    let home = f.home();
    let other = f.vault.root().parent().unwrap().join("other-home");
    std::fs::create_dir_all(&other).unwrap();
    skills::enable(&f.vault, TARGETS).unwrap();
    // An enabled config alone does not authorize environment-derived writes.
    assert!(skills::sync_enabled(&f.vault, &other).unwrap().is_none());
    assert!(std::fs::read_dir(&other).unwrap().next().is_none());
    wi_export::local::Targets {
        home: home.clone(),
        hosts: vec![],
        skill_dirs: TARGETS.iter().map(|target| target.root(&home)).collect(),
        cache_root: home.join("cache"),
    }
    .save(f.vault.root())
    .unwrap();
    assert!(skills::sync_enabled(&f.vault, &other).unwrap().is_some());
    assert!(std::fs::read_dir(&other).unwrap().next().is_none());
    assert!(SkillTarget::Agents.root(&home).is_dir());
    assert!(SkillTarget::Claude.root(&home).is_dir());
}
