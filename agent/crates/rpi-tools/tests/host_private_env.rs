//! A spawned shell must not receive the host's private plumbing.
//!
//! Two families of variables are in scope, and both are answered by the same
//! exclusion:
//!
//! * `RPI_SESSION_ID` — the host writes it so plugins can name the session they
//!   observe. A bash/powershell child inherits the environment by default, so
//!   without an exclusion it reaches *everything* the shell runs, including a
//!   nested `rpi`, which would then report an ancestor's session.
//! * `LANGFUSE_PI_PARENT_*` — an extension publishes these so a *launcher* can
//!   attach a nested `rpi` to the turn that spawned it (the extension looks at
//!   `LANGFUSE_PI_PARENT_TRACE_ID` to decide it is a subagent). Inherited by a
//!   plain shell they are a false positive: a nested `rpi` adopts its
//!   ancestor's trace instead of opening its own.
//!
//! These run against a real shell, because the property under test is about the
//! process environment of a real child — an in-memory fake spawns nothing and
//! could not observe it.

use rpi_tools::{is_host_private_env, OsExecutionEnv, Shell, ShellExecOptions};

/// Skip when the platform has no usable shell (the env resolves git-bash on
/// Windows, `/bin/bash` on POSIX). Mirrors the guard other shell tests use.
async fn shell_or_skip() -> Option<OsExecutionEnv> {
    let env = OsExecutionEnv::new();
    match env.exec("true", ShellExecOptions::default()).await {
        Ok(_) => Some(env),
        Err(error) => {
            eprintln!("skipping: no usable shell ({error})");
            None
        }
    }
}

#[tokio::test]
async fn a_host_private_variable_does_not_reach_the_shell() {
    let Some(env) = shell_or_skip().await else {
        return;
    };

    std::env::set_var("RPI_SESSION_ID", "01a0f60b-parent-session");

    // A variable the host does NOT mark private still inherits, so this is a
    // targeted exclusion rather than "the shell sees no environment".
    std::env::set_var("RPI_TOOLS_INHERIT_PROBE", "visible");

    let out = env
        .exec(
            r#"printf 'session=[%s] probe=[%s]\n' "$RPI_SESSION_ID" "$RPI_TOOLS_INHERIT_PROBE""#,
            ShellExecOptions::default(),
        )
        .await
        .expect("exec");

    assert_eq!(
        out.stdout.trim(),
        "session=[] probe=[visible]",
        "the host-private variable must not inherit while ordinary ones do"
    );

    std::env::remove_var("RPI_TOOLS_INHERIT_PROBE");
    std::env::remove_var("RPI_SESSION_ID");
}

#[tokio::test]
async fn an_explicit_private_variable_still_reaches_the_shell() {
    let Some(env) = shell_or_skip().await else {
        return;
    };

    // The exclusion is about *inheriting*, not about forbidding the name: a
    // caller that deliberately passes a value (a test fixture, a wrapper script)
    // must still be able to. Per-call env is applied after the inherit loop.
    std::env::set_var("RPI_SESSION_ID", "01a0f60b-parent-session");

    let mut per_call = std::collections::HashMap::new();
    per_call.insert(
        "RPI_SESSION_ID".to_string(),
        "01a0f60b-explicit".to_string(),
    );
    let out = env
        .exec(
            r#"printf 'session=[%s]\n' "$RPI_SESSION_ID""#,
            ShellExecOptions {
                env: Some(per_call),
                ..ShellExecOptions::default()
            },
        )
        .await
        .expect("exec");

    assert_eq!(out.stdout.trim(), "session=[01a0f60b-explicit]");

    std::env::remove_var("RPI_SESSION_ID");
}

#[test]
fn the_private_list_covers_what_the_contract_declares() {
    // Derived from the SDK's declarations, not restated: the point of moving
    // these names out of literals was that a copy can drift. Iterating the
    // declared slice means a newly added family member is covered (and checked)
    // without anyone remembering to extend this test.
    for name in rpi_plugin_sdk::SUBAGENT_PARENT_ENV
        .iter()
        .chain(std::iter::once(&rpi_plugin_sdk::SESSION_ID_ENV))
    {
        assert!(
            is_host_private_env(name),
            "`{name}` tells a nested rpi about its ancestor; it must not inherit"
        );
    }

    // Narrow: the user's own configuration must still reach the shell.
    assert!(!is_host_private_env("RPI_OFFLINE"));
    assert!(!is_host_private_env("PATH"));
}

/// The subagent channel is host-private in a spawned shell, but a launcher that
/// *deliberately* sets it on a child it starts must still get through — that is
/// the supported way to nest a subagent. This pins both halves so a future
/// widening of the exclusion cannot quietly disable nesting.
#[tokio::test]
async fn the_subagent_channel_is_inherited_from_augment_not_the_ambient_env() {
    let Some(env) = shell_or_skip().await else {
        return;
    };

    // Ambient (inherited) values are excluded…
    std::env::set_var("LANGFUSE_PI_PARENT_TRACE_ID", "ambient-trace");
    let out = env
        .exec(
            "printf '%s' \"${LANGFUSE_PI_PARENT_TRACE_ID:-unset}\"",
            ShellExecOptions::default(),
        )
        .await
        .expect("exec");
    assert_eq!(
        out.stdout.trim(),
        "unset",
        "an inherited subagent marker must not reach the shell"
    );
    std::env::remove_var("LANGFUSE_PI_PARENT_TRACE_ID");

    // …while an explicit per-call value still does (the nesting path).
    let mut env_vars = std::collections::HashMap::new();
    env_vars.insert(
        "LANGFUSE_PI_PARENT_TRACE_ID".to_string(),
        "deliberate-trace".to_string(),
    );
    let out = env
        .exec(
            "printf '%s' \"${LANGFUSE_PI_PARENT_TRACE_ID:-unset}\"",
            ShellExecOptions {
                env: Some(env_vars),
                ..ShellExecOptions::default()
            },
        )
        .await
        .expect("exec");
    assert_eq!(
        out.stdout.trim(),
        "deliberate-trace",
        "a launcher that sets the value on purpose must not be blocked"
    );
}
