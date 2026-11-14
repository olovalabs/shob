use super::*;
fn parse(tokens: &[&str]) -> Args {
    crate::args::parse_args(&tokens.iter().map(|s| s.to_string()).collect::<Vec<_>>())
}
#[test]
fn reserved_flag_is_never_an_unknown_extension_option() {
    for tokens in [
        vec![FLAG, "native", "-p", "hello"],
        vec!["--experimental-native-store=native", "-p", "hello"],
    ] {
        let args = parse(&tokens);
        assert!(!args.unknown_flags.contains_key("experimental-native-store"));
        assert_eq!(
            args.errors.is_empty(),
            cfg!(feature = "experimental-native-store"),
            "{:?}",
            args.errors
        );
        assert_eq!(
            args.experimental_native_store,
            Some(PathBuf::from("native"))
        );
    }
}
#[test]
fn missing_empty_duplicate_and_conflicting_parameters_fail_closed() {
    for extra in [
        vec![],
        vec!["--mode=json"],
        vec!["-p", "--continue"],
        vec!["-p", "-c"],
        vec!["-p", "--resume"],
        vec!["-p", "-r"],
        vec!["-p", "--session=x"],
        vec!["-p", "--session-id=x"],
        vec!["-p", "--fork=x"],
        vec!["-p", "--no-session"],
        vec!["-p", "--session-dir=old"],
        vec!["-p", "--mode=rpc"],
        vec!["-p", "--server=false"],
        vec!["-p", "--web=false"],
        vec!["-p", "--connect=127.0.0.1:1"],
        vec!["-p", "--export=x"],
        vec!["-p", "--list-models"],
        vec!["-p", "--experimental-native-store=again"],
    ] {
        let mut tokens = vec![FLAG, "native"];
        tokens.extend(extra);
        let args = parse(&tokens);
        assert!(!args.errors.is_empty(), "accepted {tokens:?}");
    }
    for tokens in [
        vec![FLAG],
        vec!["--experimental-native-store="],
        vec![FLAG, "   ", "-p"],
    ] {
        assert!(!parse(&tokens).errors.is_empty());
    }
}
#[test]
fn early_paths_are_rejected_without_matching_prompt_substrings() {
    for command in [
        "__rpi_dev_cleanup",
        "dev",
        "dev-local",
        "auth",
        "events",
        "update",
        "install",
        "uninstall",
        "extension",
        "resource",
        "package",
        "pi-package",
    ] {
        assert!(early_check(&[command.into(), format!("{FLAG}=native")]).is_err());
    }
    assert!(early_check(&["--connect=127.0.0.1:1".into(), FLAG.into(), "native".into()]).is_err());
    assert!(early_check(&[
        "--print".into(),
        "explain --experimental-native-store=native as text".into()
    ])
    .is_ok());
    assert!(early_check(&["auth".into(), "--experimental-native-store-other=x".into()]).is_ok());
    for tokens in [
        vec![
            "--connect",
            "127.0.0.1:1",
            "--token",
            "--experimental-native-store=fixture-token",
        ],
        vec![
            "--connect=127.0.0.1:1",
            "--token=--experimental-native-store=fixture-token",
        ],
    ] {
        assert!(early_check(&tokens.into_iter().map(str::to_owned).collect::<Vec<_>>()).is_ok());
    }
}
#[test]
fn mode_and_stdio_guards_preserve_default_routes() {
    for mode in [RunMode::Print, RunMode::Json] {
        assert!(validate_mode(true, mode).is_ok());
    }
    for mode in [RunMode::Interactive, RunMode::Rpc] {
        assert!(validate_mode(true, mode).is_err());
        assert!(validate_mode(false, mode).is_ok());
    }
    assert!(validate_stdio(true, true).is_err());
    assert!(validate_stdio(true, false).is_ok());
    assert!(validate_stdio(false, true).is_ok());
}
