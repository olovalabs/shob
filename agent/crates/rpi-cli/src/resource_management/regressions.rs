use super::*;
fn put(p: &Path, b: &[u8]) {
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, b).unwrap();
}
fn install(kind: Kind, name: &str, source: PathBuf) -> Options {
    Options {
        op: "install".into(),
        kind: Some(kind),
        name: Some(name.into()),
        source: Some(source),
        scope: Some(Scope::Project),
        ..Default::default()
    }
}
#[test]
fn safe_legacy_registry_names_do_not_relax_managed_or_metadata_validation() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path();
    let record = |name: &str, artifacts: Value| {
        json!({
            "name":name,"version":"1","source":"fixture","cargo_package":"fixture","artifacts":artifacts
        })
    };
    put(
        &root.join("packages.json"),
        json!({"packages":[
            record("_legacy", json!(["helper.dll"])), record(".tools", json!([]))
        ]})
        .to_string()
        .as_bytes(),
    );
    assert_eq!(packages(root).unwrap().packages.len(), 2);
    for name in ["_legacy", ".tools"] {
        assert!(identifier(name).is_err(), "managed grammar widened");
        assert!(
            parse(&["inspect", "package", name, "--scope", "project"].map(str::to_owned)).is_ok()
        );
        assert!(
            parse(&["remove", "package", name, "--scope", "project"].map(str::to_owned)).is_err()
        );
    }
    for records in [
        vec![record("..", json!([]))],
        vec![record("../escape", json!([]))],
        vec![record("trailing.", json!([]))],
        vec![record("CON", json!([]))],
        vec![record("_legacy", json!([])), record("_LEGACY", json!([]))],
        vec![record(".tools", json!(["../outside.dll"]))],
        vec![record(".tools", json!(["nested/helper.dll"]))],
        vec![record(".tools", json!(["C:helper.dll"]))],
        vec![record(".tools", json!(["CON.dll"]))],
        vec![record(".tools", json!(["helper.dll", "HELPER.dll"]))],
    ] {
        let bytes = json!({"packages":records}).to_string();
        put(&root.join("packages.json"), bytes.as_bytes());
        assert!(packages(root).is_err(), "unsafe registry accepted: {bytes}");
        assert_eq!(
            fs::read(root.join("packages.json")).unwrap(),
            bytes.as_bytes()
        );
    }
}

#[test]
fn unknown_package_metadata_is_never_discarded() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path().join("native");
    let source = d.path().join("source");
    put(&source.join("skills/demo/SKILL.md"), b"body");
    fs::create_dir(&root).unwrap();
    for bytes in [
        r#"{ "unexpected-owner-data": {"keep":"exact bytes"} }"#,
        r#"{"packages":[{"name":"other","version":"1","source":"local","cargo_package":"other","artifacts":[],"owner-private":"keep"}]}"#,
    ] {
        fs::write(root.join("packages.json"), bytes).unwrap();
        assert!(mutate(&root, &install(Kind::Package, "demo", source.clone())).is_err());
        assert_eq!(
            fs::read(root.join("packages.json")).unwrap(),
            bytes.as_bytes()
        );
        assert!(!root.join("packages/demo").exists());
        assert!(!root.join(RECEIPTS).exists());
    }
}
#[test]
fn explicit_parent_source_is_normalized_but_owned_traversal_stays_forbidden() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path().join("native");
    let source = d.path().join("source");
    fs::create_dir(d.path().join("nested")).unwrap();
    put(&source.join("SKILL.md"), b"body");
    let addressed = d.path().join("nested/../source");
    let result = mutate(&root, &install(Kind::Skill, "demo", addressed)).unwrap();
    assert_eq!(result["outcome"], "installed");
    assert_eq!(
        fs::read(root.join("skills/demo/SKILL.md")).unwrap(),
        b"body"
    );
    assert!(relative(Path::new("skills/../other")).is_err());
    assert!(checked_path(&root.join("skills/../other")).is_err());
}
#[cfg(windows)]
#[test]
fn windows_extended_case_and_ancestor_source_overlap_rejected_before_staging() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path().join("native");
    let source = d.path().join("source");
    put(&source.join("SKILL.md"), b"body");
    mutate(&root, &install(Kind::Skill, "demo", source)).unwrap();
    let before = fs::read(root.join(RECEIPTS)).unwrap();
    let installed = fs::canonicalize(root.join("skills/demo")).unwrap();
    for source in [
        installed.clone(),
        PathBuf::from(installed.to_string_lossy().to_uppercase()),
        fs::canonicalize(&root).unwrap(),
        fs::canonicalize(d.path()).unwrap(),
    ] {
        let e = mutate(&root, &install(Kind::Skill, "overlap", source)).unwrap_err();
        assert!(e.contains("overlap"), "{e}");
        assert_eq!(before, fs::read(root.join(RECEIPTS)).unwrap());
        assert!(!root.join("skills/overlap").exists());
        assert!(!fs::read_dir(&root).unwrap().any(|e| e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".resource-txn-")));
    }
    let outside = d.path().join("outside");
    fs::create_dir(&outside).unwrap();
    let alias = d.path().join("junction");
    let status = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(&alias)
        .arg(&outside)
        .output()
        .unwrap();
    assert!(status.status.success());
    // Normalizing '..' must not erase a reparse component before checking it.
    assert!(explicit_source(&alias.join("../source")).is_err());
    fs::remove_dir(alias).unwrap();
}
#[test]
fn oversized_success_is_rejected_before_commit_with_previous_state_intact() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path().join("native");
    let source = d.path().join("source");
    put(&source.join("SKILL.md"), b"body");
    mutate(&root, &install(Kind::Skill, "demo", source.clone())).unwrap();
    let old = fs::read(root.join(RECEIPTS)).unwrap();
    let old_content = inventory(&root.join("skills/demo")).unwrap();
    for n in 0..1250 {
        put(&source.join(format!("{n:04}-{}", "x".repeat(120))), b"x");
    }
    let mut inv = inventory(&source).unwrap();
    let mut r = Receipt {
        kind: Kind::Skill,
        name: "demo".into(),
        source: fs::canonicalize(&source).unwrap(),
        version: None,
        target: Kind::Skill.target("demo"),
        inventory: inv.clone(),
    };
    // Choose a receipt that fits the metadata limit but whose full public
    // success envelope exceeds the precommitted result budget.
    loop {
        r.inventory = inv.clone();
        if json_bytes(&Receipts {
            resources: vec![r.clone()],
        })
        .is_ok()
        {
            break;
        }
        let index = inv
            .files
            .iter()
            .rposition(|f| f.path != Path::new("SKILL.md"))
            .unwrap();
        let file = inv.files.remove(index);
        fs::remove_file(source.join(file.path)).unwrap();
    }
    let mut o = install(Kind::Skill, "demo", source);
    o.op = "update".into();
    let e = mutate(&root, &o).unwrap_err();
    assert!(e.contains("success result"), "{e}");
    assert_eq!(old, fs::read(root.join(RECEIPTS)).unwrap());
    assert_eq!(old_content, inventory(&root.join("skills/demo")).unwrap());
    assert!(!root.join(JOURNAL).exists());
}

#[cfg(any(unix, windows))]
mod linked_scope_roots {
    use super::*;

    // Every target and alias belongs to the same synthetic fixture. In
    // particular, never rely on /home, /var, or the user's actual home layout.
    struct OwnedLink(PathBuf);
    impl OwnedLink {
        fn new(target: &Path, alias: &Path) -> Self {
            #[cfg(unix)]
            std::os::unix::fs::symlink(target, alias).unwrap();
            #[cfg(windows)]
            {
                // Junctions need no administrator/developer-mode privilege.
                let output = std::process::Command::new("cmd")
                    .args(["/D", "/C", "mklink", "/J"])
                    .arg(alias)
                    .arg(target)
                    .output()
                    .unwrap();
                assert!(output.status.success(), "{output:?}");
            }
            Self(alias.to_path_buf())
        }
    }
    impl Drop for OwnedLink {
        fn drop(&mut self) {
            // Unlink the junction itself, never recursively delete its target.
            #[cfg(windows)]
            let result = fs::remove_dir(&self.0);
            #[cfg(unix)]
            let result = fs::remove_file(&self.0);
            if !std::thread::panicking() {
                result.unwrap();
            }
        }
    }

    const STYLES: [&str; 3] = ["project", "global-home", "global-override"];
    fn addressed_root(prefix: &Path, style: &str) -> PathBuf {
        prefix.join(match style {
            "project" => "project/.rpi",
            "global-home" => "home/.rpi/agent",
            "global-override" => "custom/agent",
            _ => unreachable!(),
        })
    }
    fn selected_root(prefix: &Path, style: &str) -> Result<PathBuf> {
        let logical = addressed_root(prefix, style);
        if style == "project" {
            Scope::Project.root(logical.parent().unwrap())
        } else {
            // Exercise the same resolver as Scope::Global with synthetic home
            // and explicit-agent-dir shapes, without changing process env.
            resolve_scope_root(&logical)
        }
    }
    fn link_error(error: String) {
        assert!(error.contains("links/reparse points"), "{error}");
    }

    #[test]
    fn linked_existing_and_missing_external_parents_resolve_without_creating_state() {
        for style in STYLES {
            for existing_parent in [false, true] {
                let d = tempfile::tempdir().unwrap();
                let base = fs::canonicalize(d.path()).unwrap();
                let physical = base.join("physical");
                fs::create_dir(&physical).unwrap();
                let alias = base.join("alias");
                let _link = OwnedLink::new(&physical, &alias);
                let expected = addressed_root(&physical, style);
                if existing_parent {
                    fs::create_dir_all(expected.parent().unwrap()).unwrap();
                }
                let resolved = selected_root(&alias, style).unwrap();
                assert_eq!(resolved, expected, "{style}/{existing_parent}");
                assert_eq!(resolved, selected_root(&physical, style).unwrap());
                assert!(!expected.exists(), "root resolution created the scope");
                if !existing_parent {
                    assert!(!expected.parent().unwrap().exists());
                }
                // The generic validator still rejects the logical alias.
                link_error(checked_path(&addressed_root(&alias, style)).unwrap_err());
                checked_path(&resolved).unwrap();
                let lease = Lease::acquire(&resolved, false, true).unwrap().unwrap();
                ensure_complete(&resolved).unwrap();
                assert!(lease_path(&expected).is_file());
                assert!(!expected.exists());
                assert!(!expected.join(RECEIPTS).exists());
                assert!(!expected.join(JOURNAL).exists());
                drop(lease);
            }
        }
    }

    #[test]
    fn logical_alias_and_physical_scope_share_host_and_mutation_locks() {
        for style in STYLES {
            let d = tempfile::tempdir().unwrap();
            let base = fs::canonicalize(d.path()).unwrap();
            let physical = base.join("physical");
            fs::create_dir(&physical).unwrap();
            let alias = base.join("alias");
            let _link = OwnedLink::new(&physical, &alias);
            // Resolve before creation and again after creation: no lock split.
            let logical_root = selected_root(&alias, style).unwrap();
            let host = Lease::acquire(&logical_root, false, true).unwrap().unwrap();
            let physical_root = selected_root(&physical, style).unwrap();
            assert_eq!(logical_root, physical_root);
            assert!(matches!(
                Lease::acquire(&physical_root, true, true),
                Err(LeaseError::Contended)
            ));
            assert!(mutation_lease(&physical_root)
                .unwrap_err()
                .contains("in use"));
            drop(host);
            let mutation = mutation_lease(&physical_root).unwrap();
            assert!(matches!(
                Lease::acquire(&selected_root(&alias, style).unwrap(), false, true),
                Err(LeaseError::Contended)
            ));
            drop(mutation);
        }
    }

    #[test]
    fn owned_root_metadata_and_descendant_links_are_not_resolved_away() {
        for style in STYLES {
            let d = tempfile::tempdir().unwrap();
            let base = fs::canonicalize(d.path()).unwrap();
            let physical = base.join("physical");
            let target = base.join("owned-target");
            put(&target.join("sentinel"), b"untouched");
            fs::create_dir(&physical).unwrap();
            let alias = base.join("alias");
            let _prefix_link = OwnedLink::new(&physical, &alias);
            let root = addressed_root(&physical, style);
            fs::create_dir_all(root.parent().unwrap()).unwrap();
            {
                let _root_link = OwnedLink::new(&target, &root);
                link_error(selected_root(&alias, style).unwrap_err());
                link_error(selected_root(&physical, style).unwrap_err());
                link_error(String::from(
                    Lease::acquire(&root, false, true).unwrap_err(),
                ));
            }
            fs::create_dir(&root).unwrap();
            for filename in [RECEIPTS, JOURNAL] {
                #[cfg(unix)]
                let metadata_target = target.join("sentinel");
                #[cfg(windows)]
                let metadata_target = target.clone();
                let _metadata_link = OwnedLink::new(&metadata_target, &root.join(filename));
                let resolved = selected_root(&alias, style).unwrap();
                let error = match filename {
                    RECEIPTS => receipts(&resolved).err().unwrap(),
                    JOURNAL => {
                        link_error(recover(&resolved).unwrap_err());
                        ensure_complete(&resolved).unwrap_err()
                    }
                    _ => unreachable!(),
                };
                link_error(error);
            }
            fs::create_dir(root.join("skills")).unwrap();
            let _child_link = OwnedLink::new(&target, &root.join("skills/demo"));
            let resolved = selected_root(&alias, style).unwrap();
            link_error(inventory(&resolved.join("skills/demo")).unwrap_err());
            assert_eq!(fs::read(target.join("sentinel")).unwrap(), b"untouched");
        }
    }

    #[test]
    fn broken_external_prefix_and_traversal_fail_without_creating_state() {
        let d = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(d.path()).unwrap();
        let target = base.join("owned-target");
        fs::create_dir(&target).unwrap();
        let alias = base.join("alias");
        let _link = OwnedLink::new(&target, &alias);
        fs::remove_dir(&target).unwrap();
        for style in STYLES {
            assert!(selected_root(&alias, style).is_err());
        }
        assert!(!target.exists(), "broken link target was recreated");
        assert!(resolve_scope_root(Path::new("relative/.rpi")).is_err());
        for suffix in [
            "../outside/.rpi",
            "absent/../.rpi",
            "absent/.rpi/..",
            "alias/../.rpi",
        ] {
            // PathBuf::push normalizes '..' for Windows verbatim prefixes.
            // Preserve the addressed traversal so the resolver must reject it.
            let mut traversal = base.as_os_str().to_os_string();
            traversal.push(std::path::MAIN_SEPARATOR_STR);
            traversal.push(suffix.replace('/', std::path::MAIN_SEPARATOR_STR));
            assert_eq!(
                resolve_scope_root(Path::new(&traversal)).unwrap_err(),
                "resource path traversal"
            );
        }
        assert_eq!(
            Scope::Project.root(Path::new("relative")).unwrap_err(),
            "resource root must be absolute"
        );
        assert!(!base.join("absent").exists());
        let filesystem_root = base.ancestors().last().unwrap();
        assert_eq!(
            resolve_scope_root(filesystem_root).unwrap(),
            filesystem_root
        );
    }

    #[cfg(windows)]
    #[test]
    fn canonical_prefix_never_changes_unresolved_dos_component_identity() {
        let d = tempfile::tempdir().unwrap();
        fs::create_dir(d.path().join("agent")).unwrap();
        for base in [d.path().to_path_buf(), fs::canonicalize(d.path()).unwrap()] {
            for suffix in [
                "agent.",
                "agent ",
                "absent./agent",
                "absent /agent",
                "CON",
                "NUL.log",
                "agent:stream",
            ] {
                let addressed = base.join(suffix.replace('/', std::path::MAIN_SEPARATOR_STR));
                assert!(resolve_scope_root(&addressed).is_err(), "{addressed:?}");
            }
            assert_eq!(
                resolve_scope_root(&base.join("agent")).unwrap(),
                fs::canonicalize(d.path()).unwrap().join("agent")
            );
        }
        assert!(!lease_path(&d.path().join("agent")).exists());
        assert!(!d.path().join("absent").exists());
    }

    #[test]
    fn incomplete_journal_through_external_alias_still_blocks_mutation() {
        for style in STYLES {
            let d = tempfile::tempdir().unwrap();
            let base = fs::canonicalize(d.path()).unwrap();
            let physical = base.join("physical");
            fs::create_dir(&physical).unwrap();
            let alias = base.join("alias");
            let _link = OwnedLink::new(&physical, &alias);
            let root = addressed_root(&physical, style);
            put(&root.join(JOURNAL), b"incomplete synthetic transaction");
            let resolved = selected_root(&alias, style).unwrap();
            assert!(ensure_complete(&resolved)
                .unwrap_err()
                .contains("incomplete"));
            assert!(mutation_lease(&resolved)
                .unwrap_err()
                .contains("incomplete"));
            assert_eq!(
                fs::read(root.join(JOURNAL)).unwrap(),
                b"incomplete synthetic transaction"
            );
            // The failed completeness check released its lease, not its journal.
            let lease = Lease::acquire(&root, true, false).unwrap().unwrap();
            drop(lease);
        }
    }
}
