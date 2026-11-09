#[cfg(target_os = "macos")]
use std::cell::RefCell;
#[cfg(target_os = "macos")]
use std::collections::VecDeque;
#[cfg(not(target_os = "linux"))]
use std::path::Path;
#[cfg(target_os = "macos")]
use std::process::Output;
#[cfg(not(target_os = "linux"))]
use wi_export::install::Error;
use wi_export::service::plan;
#[cfg(target_os = "macos")]
use wi_export::service::{Launchctl, status, uninstall};

#[cfg(target_os = "macos")]
fn output(code: i32, stderr: &str) -> Output {
    use std::os::unix::process::ExitStatusExt;
    Output {
        status: std::process::ExitStatus::from_raw(code << 8),
        stdout: Vec::new(),
        stderr: stderr.as_bytes().to_vec(),
    }
}

#[cfg(target_os = "macos")]
#[derive(Default)]
struct Fake {
    calls: RefCell<Vec<Vec<String>>>,
    script: RefCell<VecDeque<Output>>,
    bootstrap_files: RefCell<Vec<Option<String>>>,
}

#[cfg(target_os = "macos")]
impl Fake {
    fn scripted(outputs: Vec<Output>) -> Self {
        Self {
            calls: RefCell::default(),
            bootstrap_files: RefCell::default(),
            script: RefCell::new(outputs.into()),
        }
    }
    fn calls(&self) -> Vec<Vec<String>> {
        self.calls.borrow().clone()
    }
}

#[cfg(target_os = "macos")]
impl Launchctl for Fake {
    fn run(&self, args: &[&str]) -> std::io::Result<Output> {
        if args.first() == Some(&"bootstrap") {
            self.bootstrap_files
                .borrow_mut()
                .push(std::fs::read_to_string(args[2]).ok());
        }
        self.calls
            .borrow_mut()
            .push(args.iter().map(|a| (*a).to_owned()).collect());
        Ok(self
            .script
            .borrow_mut()
            .pop_front()
            .unwrap_or_else(|| output(0, "")))
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    const ID: &str = "vault_1-a";

    struct Env {
        _dir: tempfile::TempDir,
        home: std::path::PathBuf,
        binary: std::path::PathBuf,
        vault: std::path::PathBuf,
        uid: u32,
    }

    fn env() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        let uid = std::fs::metadata(&home).unwrap().uid();
        Env {
            binary: home.join("bin dir/wi"),
            vault: home.join("Notes & Things/vault"),
            home,
            uid,
            _dir: dir,
        }
    }

    fn do_plan(e: &Env) -> wi_export::service::ServicePlan {
        plan(&e.home, &e.binary, &e.vault, ID).unwrap()
    }

    fn backups(e: &Env) -> Vec<std::path::PathBuf> {
        std::fs::read_dir(e.vault.join(".wiki-island/local/backups"))
            .into_iter()
            .flatten()
            .map(|d| d.unwrap().path())
            .filter(|p| p.to_string_lossy().contains("wi-backup-"))
            .collect()
    }

    #[test]
    fn dev_binary_detection_uses_cargo_markers_and_canonical_paths() {
        use wi_export::service::plan_with_options;
        let e = env();
        for layout in [
            "custom-cache/debug/wi",
            "cross-cache/aarch64-apple-darwin/release/wi",
        ] {
            let binary = e.home.join(layout);
            std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
            std::fs::write(&binary, "binary").unwrap();
            let cache = e.home.join(layout.split('/').next().unwrap());
            let marker = if layout.starts_with("custom") {
                "CACHEDIR.TAG"
            } else {
                ".rustc_info.json"
            };
            std::fs::write(cache.join(marker), "cargo target cache").unwrap();
            assert!(matches!(
                plan(&e.home, &binary, &e.vault, ID),
                Err(Error::Config(_))
            ));
            assert!(plan_with_options(&e.home, &binary, &e.vault, ID, true).is_ok());
            let link = e.home.join("linked-wi");
            std::os::unix::fs::symlink(&binary, &link).unwrap();
            assert!(matches!(
                plan(&e.home, &link, &e.vault, ID),
                Err(Error::Config(_))
            ));
            std::fs::remove_file(link).unwrap();
        }
        let installed = e.home.join("target/release/wi");
        std::fs::create_dir_all(installed.parent().unwrap()).unwrap();
        std::fs::write(&installed, "installed binary").unwrap();
        assert!(plan(&e.home, &installed, &e.vault, ID).is_ok());
    }

    #[test]
    fn renders_golden_plist_with_escaped_paths() {
        let e = env();
        let p = do_plan(&e);
        let h = e.home.display();
        let expected = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>com.wiki-island.watch.vault_1-a</string>
	<key>ProgramArguments</key>
	<array>
		<string>{h}/bin dir/wi</string>
		<string>watch</string>
		<string>--vault</string>
		<string>{h}/Notes &amp; Things/vault</string>
	</array>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<true/>
	<key>ProcessType</key>
	<string>Background</string>
	<key>StandardOutPath</key>
	<string>{h}/Library/Logs/wiki-island/vault_1-a.log</string>
	<key>StandardErrorPath</key>
	<string>{h}/Library/Logs/wiki-island/vault_1-a.log</string>
	<key>EnvironmentVariables</key>
	<dict>
		<key>HOME</key>
		<string>{h}</string>
		<key>PATH</key>
		<string>/usr/bin:/bin:/usr/sbin:/sbin:/opt/homebrew/bin:/usr/local/bin</string>
	</dict>
</dict>
</plist>
"#
        );
        assert_eq!(p.contents, expected);
        assert_eq!(p.label, "com.wiki-island.watch.vault_1-a");
        assert_eq!(
            p.plist_path,
            e.home
                .join("Library/LaunchAgents/com.wiki-island.watch.vault_1-a.plist")
        );
        assert!(p.existing.is_none());
        assert!(!p.is_noop());
        assert!(p.diff().contains("+<key>") || p.diff().contains("+\t<key>Label</key>"));
    }

    #[test]
    fn apply_writes_plist_and_runs_bootout_then_bootstrap() {
        let e = env();
        let p = do_plan(&e);
        let fake = Fake::scripted(vec![output(113, "Could not find service"), output(0, "")]);
        p.apply(&fake).unwrap();
        assert_eq!(std::fs::read_to_string(&p.plist_path).unwrap(), p.contents);
        assert!(e.home.join("Library/Logs/wiki-island").is_dir());
        assert!(backups(&e).is_empty());
        let label = &p.label;
        assert_eq!(
            fake.calls(),
            vec![
                vec!["bootout".to_owned(), format!("gui/{}/{label}", e.uid)],
                vec![
                    "bootstrap".to_owned(),
                    format!("gui/{}", e.uid),
                    p.plist_path.to_string_lossy().into_owned()
                ],
            ]
        );
    }

    #[test]
    fn apply_backs_up_differing_file_with_unique_name() {
        let e = env();
        let first = do_plan(&e);
        std::fs::create_dir_all(first.plist_path.parent().unwrap()).unwrap();
        std::fs::write(&first.plist_path, "old plist").unwrap();
        for round in 1..=2 {
            let p = do_plan(&e);
            if round == 2 {
                std::fs::write(&p.plist_path, "old plist 2").unwrap();
                let p = do_plan(&e);
                p.apply(&Fake::default()).unwrap();
            } else {
                assert_eq!(p.existing.as_deref(), Some("old plist"));
                p.apply(&Fake::default()).unwrap();
            }
        }
        let found = backups(&e);
        assert_eq!(found.len(), 2);
        let mut texts: Vec<_> = found
            .iter()
            .map(|p| std::fs::read_to_string(p).unwrap())
            .collect();
        texts.sort();
        assert_eq!(texts, vec!["old plist", "old plist 2"]);
    }

    #[test]
    fn stale_preview_fails_without_touching_file() {
        let e = env();
        let p = do_plan(&e);
        std::fs::create_dir_all(p.plist_path.parent().unwrap()).unwrap();
        std::fs::write(&p.plist_path, "changed after preview").unwrap();
        let fake = Fake::default();
        let err = p.apply(&fake).unwrap_err();
        assert!(matches!(err, Error::Stale), "{err:?}");
        assert_eq!(
            std::fs::read_to_string(&p.plist_path).unwrap(),
            "changed after preview"
        );
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn symlink_plist_is_rejected_at_plan_and_apply() {
        let e = env();
        let p = do_plan(&e);
        let dir = p.plist_path.parent().unwrap().to_path_buf();
        std::fs::create_dir_all(&dir).unwrap();
        let target = e.home.join("elsewhere.plist");
        std::fs::write(&target, "x").unwrap();
        std::os::unix::fs::symlink(&target, &p.plist_path).unwrap();
        let err = plan(&e.home, &e.binary, &e.vault, ID).unwrap_err();
        assert!(matches!(err, Error::Config(_)), "{err:?}");
        // Symlink appears after the preview.
        std::fs::remove_file(&p.plist_path).unwrap();
        let fresh = do_plan(&e);
        std::os::unix::fs::symlink(&target, &fresh.plist_path).unwrap();
        let fake = Fake::default();
        assert!(fresh.apply(&fake).is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "x");
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn bootout_not_loaded_is_tolerated_but_other_failures_propagate() {
        for (code, stderr) in [
            (3, "No such process"),
            (5, "Boot-out failed: No such process"),
        ] {
            let e = env();
            let fake = Fake::scripted(vec![output(code, stderr), output(0, "")]);
            do_plan(&e).apply(&fake).unwrap();
        }
        let e = env();
        let fake = Fake::scripted(vec![output(5, "Input/output error")]);
        let err = do_plan(&e).apply(&fake).unwrap_err();
        assert!(matches!(err, Error::Service(_)), "{err:?}");
        assert_eq!(fake.calls().len(), 1, "bootstrap must not run");
        let fake = Fake::scripted(vec![output(1, "Operation not permitted")]);
        let err = do_plan(&e).apply(&fake).unwrap_err();
        assert!(err.to_string().contains("Operation not permitted"), "{err}");
    }

    #[test]
    fn bootstrap_failure_propagates_with_stderr() {
        let e = env();
        let fake = Fake::scripted(vec![
            output(0, ""),
            output(5, "Bootstrap failed: bad plist"),
        ]);
        let err = do_plan(&e).apply(&fake).unwrap_err();
        assert!(matches!(err, Error::Service(_)), "{err:?}");
        assert!(err.to_string().contains("Bootstrap failed: bad plist"));
    }

    #[test]
    fn bootstrap_failure_restores_old_plist_and_reloads_previous_service() {
        let e = env();
        let first = do_plan(&e);
        std::fs::create_dir_all(first.plist_path.parent().unwrap()).unwrap();
        let previous = "previous plist bytes\n";
        std::fs::write(&first.plist_path, previous).unwrap();
        let plan = do_plan(&e);
        let fake = Fake::scripted(vec![
            output(0, ""),
            output(5, "new service refused"),
            output(0, ""),
        ]);
        let error = plan.apply(&fake).unwrap_err();
        assert!(error.to_string().contains("new service refused"));
        assert_eq!(std::fs::read_to_string(&plan.plist_path).unwrap(), previous);
        assert_eq!(fake.calls().len(), 3);
        assert_eq!(fake.calls()[2][0], "bootstrap");
        assert_eq!(
            *fake.bootstrap_files.borrow(),
            vec![Some(plan.contents.clone()), Some(previous.into())]
        );
        assert_eq!(std::fs::read_to_string(&backups(&e)[0]).unwrap(), previous);
    }

    #[test]
    fn first_bootstrap_failure_removes_new_plist() {
        let e = env();
        let plan = do_plan(&e);
        let fake = Fake::scripted(vec![output(113, "not loaded"), output(5, "bad plist")]);
        assert!(plan.apply(&fake).is_err());
        assert!(!plan.plist_path.exists());
        assert_eq!(fake.calls().len(), 2);
    }

    #[test]
    fn service_plan_refuses_development_binary_paths_unless_explicitly_allowed() {
        let e = env();
        for profile in ["debug", "release"] {
            let binary = e.home.join(format!("checkout/target/{profile}/wi"));
            std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
            std::fs::write(&binary, "binary").unwrap();
            std::fs::write(
                e.home.join("checkout/target/CACHEDIR.TAG"),
                "cargo target cache",
            )
            .unwrap();
            let error = plan(&e.home, &binary, &e.vault, ID).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("cargo install --path crates/wi-cli")
            );
            assert!(
                wi_export::service::plan_with_options(&e.home, &binary, &e.vault, ID, true).is_ok()
            );
        }
        assert!(!e.home.join("Library").exists());
    }

    #[test]
    fn second_plan_is_noop_and_apply_still_loads() {
        let e = env();
        do_plan(&e).apply(&Fake::default()).unwrap();
        let again = do_plan(&e);
        assert!(again.is_noop());
        assert!(again.diff().is_empty());
        let fake = Fake::default();
        again.apply(&fake).unwrap();
        assert_eq!(fake.calls().len(), 2);
        assert!(backups(&e).is_empty());
    }

    #[test]
    fn uninstall_removes_once_then_reports_false() {
        let e = env();
        let p = do_plan(&e);
        p.apply(&Fake::default()).unwrap();
        let fake = Fake::default();
        assert!(uninstall(&e.home, ID, &fake).unwrap());
        assert_eq!(fake.calls()[0][0], "bootout");
        assert!(!p.plist_path.exists());
        let fake = Fake::default();
        assert!(!uninstall(&e.home, ID, &fake).unwrap());
        let calls = fake.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0][0], "bootout");
        assert!(calls[0][1].starts_with(&format!("gui/{}/", e.uid)));
    }

    #[test]
    fn uninstall_with_missing_plist_still_boots_out() {
        let e = env();
        let fake = Fake::default();
        assert!(!uninstall(&e.home, ID, &fake).unwrap());
        assert_eq!(fake.calls().len(), 1);
        assert_eq!(fake.calls()[0][0], "bootout");
    }

    #[test]
    fn noop_plan_with_modified_plist_fails_before_launchctl() {
        let e = env();
        do_plan(&e).apply(&Fake::default()).unwrap();
        let again = do_plan(&e);
        assert!(again.is_noop());
        std::fs::write(&again.plist_path, "tampered").unwrap();
        let fake = Fake::default();
        let err = again.apply(&fake).unwrap_err();
        assert!(matches!(err, Error::Stale), "{err:?}");
        assert!(fake.calls().is_empty());
        assert_eq!(
            std::fs::read_to_string(&again.plist_path).unwrap(),
            "tampered"
        );
    }

    #[test]
    fn noop_plan_with_symlinked_plist_fails_before_launchctl() {
        let e = env();
        do_plan(&e).apply(&Fake::default()).unwrap();
        let again = do_plan(&e);
        assert!(again.is_noop());
        let target = e.home.join("elsewhere.plist");
        std::fs::write(&target, "x").unwrap();
        std::fs::remove_file(&again.plist_path).unwrap();
        std::os::unix::fs::symlink(&target, &again.plist_path).unwrap();
        let fake = Fake::default();
        assert!(again.apply(&fake).is_err());
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn uninstall_keeps_plist_when_bootout_fails() {
        let e = env();
        let p = do_plan(&e);
        p.apply(&Fake::default()).unwrap();
        let fake = Fake::scripted(vec![output(1, "denied")]);
        assert!(uninstall(&e.home, ID, &fake).is_err());
        assert!(p.plist_path.exists());
    }

    #[test]
    fn status_reflects_install_and_print_exit_code() {
        let e = env();
        let fake = Fake::scripted(vec![output(113, "")]);
        let s = status(&e.home, ID, &fake).unwrap();
        assert!(!s.installed && !s.loaded);
        assert_eq!(
            fake.calls()[0],
            vec!["print".to_owned(), format!("gui/{}/{}", e.uid, s.label)]
        );
        do_plan(&e).apply(&Fake::default()).unwrap();
        let s = status(&e.home, ID, &Fake::scripted(vec![output(0, "")])).unwrap();
        assert!(s.installed && s.loaded);
        assert!(
            s.log_path
                .ends_with("Library/Logs/wiki-island/vault_1-a.log")
        );
    }

    #[test]
    fn rejects_invalid_vault_id_and_relative_paths() {
        let e = env();
        for bad in ["", "a/b", "a b", "../x", "a.b", "é"] {
            let err = plan(&e.home, &e.binary, &e.vault, bad).unwrap_err();
            assert!(matches!(err, Error::Config(_)), "{bad}: {err:?}");
        }
        assert!(plan(&e.home, Path::new("wi"), &e.vault, ID).is_err());
        assert!(plan(&e.home, &e.binary, Path::new("vault"), ID).is_err());
        assert!(plan(Path::new("home"), &e.binary, &e.vault, ID).is_err());
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
#[test]
fn plan_is_unsupported_without_a_service_backend() {
    let err = plan(
        Path::new("/home/u"),
        Path::new("/usr/bin/wi"),
        Path::new("/v"),
        "id",
    )
    .unwrap_err();
    assert!(matches!(err, Error::Unsupported(_)), "{err:?}");
}

#[cfg(target_os = "linux")]
#[test]
fn linux_plan_renders_a_systemd_user_unit_under_the_config_home() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().canonicalize().unwrap();
    let plan = plan(&home, &home.join("bin/wi"), &home.join("vault"), "id").unwrap();
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| home.join(".config"));
    assert_eq!(
        plan.unit_path,
        config.join("systemd/user/wiki-island-id.service")
    );
    assert_eq!(plan.label, "wiki-island-id.service");
    assert!(plan.contents.contains("WantedBy=default.target"));
    assert!(plan.contents.contains(&format!(
        "ExecStart=\"{}\" \"watch\" \"--vault\" \"{}\"",
        home.join("bin/wi").display(),
        home.join("vault").display()
    )));
}

#[cfg(target_os = "macos")]
#[test]
fn rotation_keeps_one_bounded_previous_file_and_the_open_descriptor() {
    use std::io::Write;
    let temp = tempfile::tempdir().unwrap();
    let directory = temp.path().join("Library/Logs/wiki-island");
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("test.log");
    std::fs::write(&path, vec![b'x'; 1024 * 1024 + 100]).unwrap();
    let mut open = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    wi_export::service::rotate_log(temp.path(), "test").unwrap();
    assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
    assert_eq!(
        std::fs::metadata(path.with_extension("log.old"))
            .unwrap()
            .len(),
        1024 * 1024
    );
    open.write_all(b"next change\n").unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "next change\n");
    open.write_all(&vec![b'y'; 1024 * 1024]).unwrap();
    wi_export::service::rotate_log(temp.path(), "test").unwrap();
    assert_eq!(std::fs::read_dir(directory).unwrap().count(), 2);
}
