use super::*;
fn put(p: impl AsRef<Path>, b: impl AsRef<[u8]>) {
    fs::create_dir_all(p.as_ref().parent().unwrap()).unwrap();
    fs::write(p, b).unwrap();
}
fn options(op: &str, kind: Kind, name: &str, source: Option<PathBuf>) -> Options {
    Options {
        op: op.into(),
        kind: Some(kind),
        name: Some(name.into()),
        scope: Some(Scope::Project),
        source,
        ..Default::default()
    }
}
fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let d = tempfile::tempdir().unwrap();
    let root = d.path().join("native");
    let source = d.path().join("source");
    put(
        source.join("SKILL.md"),
        "---\nname: demo\ndescription: fixture\n---\nbody",
    );
    fs::create_dir(&root).unwrap();
    (d, root, source)
}

#[test]
fn leases_distinguish_contention_from_errors_and_keep_observation_guard() {
    let (_dir, root, _) = fixture();
    let (guard, busy) = observation_lease(&root).unwrap();
    assert!(guard.is_none() && !busy);
    assert!(!lease_path(&root).exists());
    let a = Lease::acquire(&root, false, true).unwrap().unwrap();
    let b = Lease::acquire(&root, false, true).unwrap().unwrap();
    assert!(matches!(
        Lease::acquire(&root, true, false),
        Err(LeaseError::Contended)
    ));
    let (catalog_guard, busy) = observation_lease(&root).unwrap();
    assert!(busy);
    drop(a);
    drop(b);
    assert!(matches!(
        Lease::acquire(&root, true, false),
        Err(LeaseError::Contended)
    ));
    drop(catalog_guard);
    let (idle, busy) = observation_lease(&root).unwrap();
    assert!(!busy);
    assert!(matches!(
        Lease::acquire(&root, false, false),
        Err(LeaseError::Contended)
    ));
    drop(idle);
    assert!(matches!(
        lock_error(std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
        LeaseError::Other(_)
    ));
    fs::remove_file(lease_path(&root)).unwrap();
    fs::create_dir(lease_path(&root)).unwrap();
    assert!(matches!(
        Lease::acquire(&root, true, false),
        Err(LeaseError::Other(_))
    ));
    fs::remove_dir(lease_path(&root)).unwrap();
}

#[test]
fn host_lease_does_not_create_project_resource_directory() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join(".rpi");
    let host = shared_lease(&root).unwrap();
    assert!(!root.exists());
    assert!(lease_path(&root).is_file());
    assert!(matches!(
        Lease::acquire(&root, true, true),
        Err(LeaseError::Contended)
    ));
    drop(host);
    let mutation = mutation_lease(&root).unwrap();
    assert!(root.is_dir());
    drop(mutation);
    fs::remove_file(lease_path(&root)).unwrap();
}

#[test]
fn copy_is_bounded_and_failure_preserves_old_install() {
    let (_dir, root, source) = fixture();
    mutate(
        &root,
        &options("install", Kind::Skill, "demo", Some(source.clone())),
    )
    .unwrap();
    let old = inventory(&root.join("skills/demo")).unwrap();
    let receipt = fs::read(root.join(RECEIPTS)).unwrap();
    let expected = inventory(&source).unwrap();
    put(
        source.join("SKILL.md"),
        "changed and larger than the previously inventoried input",
    );
    let temp = tempfile::tempdir_in(&root).unwrap();
    assert!(copy_snapshot(&source, &temp.path().join("copy"), &expected).is_err());
    assert_eq!(old, inventory(&root.join("skills/demo")).unwrap());
    assert_eq!(receipt, fs::read(root.join(RECEIPTS)).unwrap());
    fs::remove_file(source.join("SKILL.md")).unwrap();
    assert!(mutate(&root, &options("update", Kind::Skill, "demo", Some(source))).is_err());
    assert_eq!(receipt, fs::read(root.join(RECEIPTS)).unwrap());
}

fn transaction(root: &Path) -> (tempfile::TempDir, Vec<(PathBuf, Option<PathBuf>)>) {
    put(root.join("skills/demo/SKILL.md"), "old resource");
    put(root.join(RECEIPTS), "old receipts");
    put(root.join("packages.json"), "old registry");
    let s = stage_dir(root).unwrap();
    put(s.path().join("snapshot/SKILL.md"), "new resource");
    put(s.path().join("receipts"), "new receipts");
    put(s.path().join("registry"), "new registry");
    let c = vec![
        (
            PathBuf::from("skills/demo"),
            Some(s.path().join("snapshot")),
        ),
        (PathBuf::from(RECEIPTS), Some(s.path().join("receipts"))),
        (
            PathBuf::from("packages.json"),
            Some(s.path().join("registry")),
        ),
    ];
    (s, c)
}
fn assert_old(root: &Path) {
    assert_eq!(
        fs::read(root.join("skills/demo/SKILL.md")).unwrap(),
        b"old resource"
    );
    assert_eq!(fs::read(root.join(RECEIPTS)).unwrap(), b"old receipts");
    assert_eq!(
        fs::read(root.join("packages.json")).unwrap(),
        b"old registry"
    );
    assert!(!root.join(JOURNAL).exists());
}
#[test]
fn publication_failures_and_crashes_restore_every_target_boundary() {
    for checkpoint in 0..=6 {
        let (_dir, root, _) = fixture();
        let _lease = mutation_lease(&root).unwrap();
        let (stage, changes) = transaction(&root);
        let error = publish(&root, changes, stage, |step| {
            if step == checkpoint {
                Err("injected filesystem publication failure".into())
            } else {
                Ok(())
            }
        })
        .unwrap_err();
        assert!(error.contains("injected"));
        assert_old(&root);
        let (stage, changes) = transaction(&root);
        let crash = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            publish(&root, changes, stage, |step| {
                assert_ne!(step, checkpoint, "simulated process interruption");
                Ok(())
            })
        }));
        assert!(crash.is_err());
        assert!(root.join(JOURNAL).is_file());
        assert!(ensure_complete(&root).is_err());
        assert!(recover(&root).unwrap());
        assert!(!recover(&root).unwrap());
        assert_old(&root);
    }
}
#[test]
fn actual_rename_registry_write_and_remove_failures_preserve_old_bytes() {
    for target in 0..3 {
        let (_dir, root, _) = fixture();
        let _lease = mutation_lease(&root).unwrap();
        let (stage, changes) = transaction(&root);
        let doomed = changes[target].1.clone().unwrap();
        let error = publish(&root, changes, stage, |step| {
            if step == target * 2 + 1 {
                remove_path(&doomed)?;
            }
            Ok(())
        })
        .unwrap_err();
        assert!(!error.is_empty());
        assert_old(&root);
    }
    let (_dir, root, source) = fixture();
    mutate(
        &root,
        &options("install", Kind::Skill, "demo", Some(source)),
    )
    .unwrap();
    let receipt = fs::read(root.join(RECEIPTS)).unwrap();
    let content = fs::read(root.join("skills/demo/SKILL.md")).unwrap();
    let stage = stage_dir(&root).unwrap();
    // A real create_new failure while staging registry bytes, before any activation.
    fs::create_dir(stage.path().join("receipts")).unwrap();
    assert!(write_new(&stage.path().join("receipts"), b"x").is_err());
    assert_eq!(fs::read(root.join(RECEIPTS)).unwrap(), receipt);
    assert_eq!(
        fs::read(root.join("skills/demo/SKILL.md")).unwrap(),
        content
    );
    let changes = vec![
        (PathBuf::from("skills/demo"), None),
        (PathBuf::from(RECEIPTS), None),
    ];
    assert!(publish(&root, changes, stage, |step| if step == 3 {
        Err("remove publication failed".into())
    } else {
        Ok(())
    })
    .is_err());
    assert_eq!(fs::read(root.join(RECEIPTS)).unwrap(), receipt);
    assert_eq!(
        fs::read(root.join("skills/demo/SKILL.md")).unwrap(),
        content
    );
}
#[test]
fn recovery_refuses_changed_targets_and_unsafe_journal_paths() {
    let (_dir, root, _) = fixture();
    let (stage, changes) = transaction(&root);
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        publish(&root, changes, stage, |step| {
            assert_ne!(step, 2);
            Ok(())
        })
    }));
    put(root.join("skills/demo/user-file"), "do not delete");
    assert!(recover(&root).unwrap_err().contains("changed"));
    assert_eq!(
        fs::read(root.join("skills/demo/user-file")).unwrap(),
        b"do not delete"
    );
    let mut j: Journal = serde_json::from_slice(&fs::read(root.join(JOURNAL)).unwrap()).unwrap();
    j.changes[0].target = "../outside".into();
    fs::write(root.join(JOURNAL), json_bytes(&j).unwrap()).unwrap();
    assert!(recover(&root).is_err());
}
#[test]
fn catalog_cannot_observe_a_paused_half_publication_and_never_creates_locks() {
    let (_dir, root, source) = fixture();
    let o = Options {
        op: "list".into(),
        scope: Some(Scope::Project),
        ..Default::default()
    };
    assert_eq!(catalog(&root, &o).unwrap()["resources"], json!([]));
    assert!(!lease_path(&root).exists());
    mutate(
        &root,
        &options("install", Kind::Skill, "demo", Some(source)),
    )
    .unwrap();
    let (entered, wait) = std::sync::mpsc::channel();
    let (resume, go) = std::sync::mpsc::channel();
    let thread_root = root.clone();
    let thread = std::thread::spawn(move || {
        let _lease = mutation_lease(&thread_root).unwrap();
        let stage = stage_dir(&thread_root).unwrap();
        put(stage.path().join("snapshot/SKILL.md"), "new snapshot");
        let changes = vec![(
            PathBuf::from("skills/demo"),
            Some(stage.path().join("snapshot")),
        )];
        publish(&thread_root, changes, stage, |step| {
            if step == 1 {
                entered.send(()).unwrap();
                go.recv().unwrap();
            }
            Ok(())
        })
        .unwrap();
    });
    wait.recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    assert!(catalog(&root, &o).unwrap_err().contains("in use"));
    resume.send(()).unwrap();
    thread.join().unwrap();
    assert!(catalog(&root, &o).unwrap()["resources"][0]["diagnostics"]
        .to_string()
        .contains("differs"));
}
#[test]
fn bounded_input_depth_width_receipts_and_next_state_do_not_publish_invalid_installs() {
    let (_dir, root, source) = fixture();
    let inv = inventory(&source).unwrap();
    let mut r = Receipts::default();
    for n in 0..128 {
        let name = format!("skill-{n}");
        r.resources.push(Receipt {
            kind: Kind::Skill,
            target: Kind::Skill.target(&name),
            name,
            source: source.clone(),
            version: None,
            inventory: inv.clone(),
        });
    }
    fs::write(root.join(RECEIPTS), json_bytes(&r).unwrap()).unwrap();
    let before = fs::read(root.join(RECEIPTS)).unwrap();
    let e = mutate(
        &root,
        &options("install", Kind::Skill, "overflow", Some(source.clone())),
    )
    .unwrap_err();
    assert!(e.contains("128"), "{e}");
    assert_eq!(before, fs::read(root.join(RECEIPTS)).unwrap());
    assert!(!root.join("skills/overflow").exists());
    let args = [
        "install",
        "skill",
        "v",
        "--scope",
        "project",
        "--path",
        "source",
        "--version",
        "",
    ];
    assert!(parse(&args.map(str::to_owned)).is_err());
    let mut deep = source.clone();
    for _ in 0..34 {
        deep.push("d");
        fs::create_dir(&deep).unwrap();
    }
    assert!(inventory(&source).unwrap_err().contains("depth"));
    fs::remove_dir_all(source.join("d")).unwrap();
    for n in 0..MAX_FILES {
        put(source.join(format!("file-{n}")), "x");
    }
    assert!(inventory(&source).unwrap_err().contains("budget"));
}
#[test]
fn corrupt_metadata_missing_legacy_owners_mismatch_and_modified_installs_fail_closed() {
    let (_dir, root, source) = fixture();
    let o = options("install", Kind::Skill, "demo", Some(source.clone()));
    fs::write(root.join(RECEIPTS), "broken").unwrap();
    assert!(mutate(&root, &o).is_err());
    assert_eq!(fs::read(root.join(RECEIPTS)).unwrap(), b"broken");
    fs::remove_file(root.join(RECEIPTS)).unwrap();
    let lib = source.with_extension(std::env::consts::DLL_EXTENSION);
    put(&lib, "opaque native candidate");
    fs::write(root.join("native-packages.json"),json!([{"name":"native-owner","version":"1","artifacts":[format!("native.{}",std::env::consts::DLL_EXTENSION)]}]).to_string()).unwrap();
    assert!(mutate(
        &root,
        &options("install", Kind::Extension, "native", Some(lib))
    )
    .unwrap_err()
    .contains("legacy"));
    mutate(
        &root,
        &options("install", Kind::Package, "bundle", Some(source.clone())),
    )
    .unwrap();
    let before = fs::read(root.join(RECEIPTS)).unwrap();
    let mut p = packages(&root).unwrap();
    p.packages[0].source = "another-provider".into();
    fs::write(root.join("packages.json"), json_bytes(&p).unwrap()).unwrap();
    assert!(
        mutate(&root, &options("remove", Kind::Package, "bundle", None))
            .unwrap_err()
            .contains("disagrees")
    );
    assert_eq!(fs::read(root.join(RECEIPTS)).unwrap(), before);
    fs::write(
        root.join("packages.json"),
        json_bytes(&Packages::default()).unwrap(),
    )
    .unwrap();
    fs::write(
        root.join(RECEIPTS),
        json_bytes(&Receipts::default()).unwrap(),
    )
    .unwrap();
    mutate(&root, &o).unwrap();
    put(root.join("skills/demo/unowned"), "keep");
    assert!(mutate(&root, &options("remove", Kind::Skill, "demo", None))
        .unwrap_err()
        .contains("changed"));
    assert_eq!(fs::read(root.join("skills/demo/unowned")).unwrap(), b"keep");
}
#[cfg(windows)]
#[test]
fn legacy_target_aliases_cannot_bypass_owned_windows_paths() {
    let (_dir, root, source) = fixture();
    let library = source.with_extension("dll");
    put(&library, "native");
    mutate(
        &root,
        &options("install", Kind::Extension, "Foo", Some(library)),
    )
    .unwrap();
    assert!(legacy_unowned_targets(&root, &[root.join("extensions/foo.dll")]).is_err());
    let outside = root.parent().unwrap().join("outside");
    fs::create_dir(&outside).unwrap();
    let link = root.join("alias");
    let status = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(&link)
        .arg(&outside)
        .status()
        .unwrap();
    assert!(status.success());
    assert!(legacy_unowned_targets(&root, &[link.join("anything.dll")]).is_err());
    fs::remove_dir(link).unwrap();
}
