//! Consumer-visible streaming regressions: sparse output must not wait for exit.

use std::sync::Arc;
use std::time::Duration;

use rpi_agent::types::{AgentToolResult, TextContentOrImage};
use rpi_tools::{ExecutionEnv, ExecutionToolContext, OsExecutionEnv};
use tokio_util::sync::CancellationToken;

fn text(result: &AgentToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|content| match content {
            TextContentOrImage::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect()
}

#[cfg(unix)]
#[tokio::test]
async fn sparse_output_is_visible_while_the_command_is_still_running() {
    let dir = tempfile::tempdir().unwrap();
    let env: Arc<dyn ExecutionEnv> = Arc::new(OsExecutionEnv::with_cwd(dir.path().to_path_buf()));
    let tool = rpi_tools::create_bash_tool(&ExecutionToolContext::new(env, None), None);
    let (updates, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let on_update = Arc::new(move |result: AgentToolResult| {
        let _ = updates.send(text(&result));
    });
    let execution = tool.execute(
        "sparse",
        serde_json::json!({"command": "printf 'first\\n'; sleep 0.02; printf 'second\\n'; sleep 1; printf 'finished\\n'"}),
        CancellationToken::new(),
        on_update,
    );
    tokio::pin!(execution);
    let visible = async {
        while let Some(output) = receiver.recv().await {
            if output.contains("second\n") {
                assert!(!output.contains("finished"));
                return;
            }
        }
        panic!("stream ended without the sparse update");
    };
    tokio::select! {
        result = &mut execution => panic!("command completed before its sparse update: {result:?}"),
        result = tokio::time::timeout(Duration::from_millis(750), visible) => {
            result.expect("quiet output was hidden until command completion");
        }
    }
    let result = execution.await.expect("shell execution");
    assert_eq!(text(&result), "first\nsecond\nfinished\n");
    let mut final_update = String::new();
    while let Ok(update) = receiver.try_recv() {
        final_update = update;
    }
    assert_eq!(final_update, "first\nsecond\nfinished\n");
}

#[cfg(windows)]
#[tokio::test]
async fn powershell_streams_encoded_unicode_command_before_completion() {
    let dir = tempfile::tempdir().unwrap();
    let env: Arc<dyn ExecutionEnv> = Arc::new(OsExecutionEnv::with_cwd(dir.path().to_path_buf()));
    let tool = rpi_tools::create_powershell_tool(&ExecutionToolContext::new(env, None));
    let (updates, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let execution = tool.execute(
        "powershell-stream",
        serde_json::json!({
            "command": "$word = '界'; Write-Output 'first'; Start-Sleep -Milliseconds 20; Write-Output $word; Start-Sleep -Seconds 1; Write-Output 'finished'"
        }),
        CancellationToken::new(),
        Arc::new(move |result: AgentToolResult| { let _ = updates.send(text(&result)); }),
    );
    tokio::pin!(execution);
    let visible = async {
        while let Some(output) = receiver.recv().await {
            if output.contains('界') {
                assert!(!output.contains("finished"));
                return;
            }
        }
        panic!("PowerShell did not publish live output");
    };
    tokio::select! {
        result = &mut execution => panic!("PowerShell completed before publishing live output: {result:?}"),
        result = tokio::time::timeout(Duration::from_secs(10), visible) => {
            result.expect("PowerShell live output timed out");
        }
    }
    let result = execution.await.expect("PowerShell execution");
    assert_eq!(text(&result), "first\n界\nfinished\n");
}

#[cfg(windows)]
#[tokio::test(flavor = "current_thread")]
async fn shell_preserves_mixed_utf8_and_console_encoded_output() {
    use rpi_tools::{Shell, ShellExecOptions};
    let dir = tempfile::tempdir().unwrap();
    let env = OsExecutionEnv::with_cwd(dir.path().to_path_buf());
    let mut streamed = String::new();
    let output = env.exec(
        "$out = [Console]::OpenStandardOutput(); $bytes = [byte[]](0x61,0x73,0x63,0x69,0x69,10,0xe4,0xb8,0xad,10,0xff,10); $out.Write($bytes,0,$bytes.Length); $out.Flush()",
        ShellExecOptions {
            shell: Some(rpi_tools::env::ShellOverride {
                program: "powershell.exe".into(),
                args: vec!["-NoLogo".into(), "-NoProfile".into(), "-NonInteractive".into(), "-Command".into()],
            }),
            on_stdout: Some(Box::new(|text| streamed.push_str(text))),
            timeout: Some(10.0),
            ..Default::default()
        },
    ).await.expect("PowerShell raw output");
    let expected = rpi_tools::output_decode::decode_output(b"ascii\n\xE4\xB8\xAD\n\xff\n");
    assert_eq!(output.stdout, expected);
    assert_eq!(streamed, expected);
    assert!(output.stdout.starts_with("ascii\n中\n"));
    assert!(output.stderr.is_empty());
}
