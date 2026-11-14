//! `rpi events …` — inspect the extension event journal (P3).
//!
//! The journal is written only when `RPI_EVENT_LOG=1` is set (see
//! [`rpi_extensions::event_log`]). This subcommand reads the same file:
//!
//! - `rpi events path` — print the resolved log path.
//! - `rpi events tail` — print the log and follow new entries (Ctrl+C to stop).

use std::io::{Read, Seek, SeekFrom, Write};
use std::time::Duration;

use crate::app::{EXIT_RUNTIME, EXIT_USAGE};

/// Dispatch `rpi events <subcommand>`.
pub async fn run(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("tail") => tail().await,
        Some("path") => match rpi_extensions::event_log_path() {
            Some(path) => {
                println!("{}", path.display());
                0
            }
            None => {
                eprintln!("error: could not resolve the event log path (set RPI_EVENT_LOG_PATH)");
                EXIT_RUNTIME
            }
        },
        Some("--help" | "-h") | None => {
            print_help();
            0
        }
        Some(other) => {
            eprintln!("error: unknown `events` subcommand `{other}`");
            print_help();
            EXIT_USAGE
        }
    }
}

fn print_help() {
    println!("Usage: rpi events <command>");
    println!();
    println!("Commands:");
    println!("  path    Print the event log path (JSONL)");
    println!("  tail    Print the log and follow new entries (Ctrl+C to stop)");
    println!();
    println!("The journal is written only when RPI_EVENT_LOG=1 is set at run time.");
}

/// Follow the event log, printing complete lines as they appear. Never returns
/// (Ctrl+C stops the process). A partial trailing line is re-read on the next
/// tick rather than emitted split.
async fn tail() -> i32 {
    let Some(path) = rpi_extensions::event_log_path() else {
        eprintln!("error: could not resolve the event log path (set RPI_EVENT_LOG_PATH)");
        return EXIT_RUNTIME;
    };
    eprintln!("tailing {} — Ctrl+C to stop", path.display());
    let mut offset = 0u64;
    loop {
        if let Ok(mut file) = std::fs::File::open(&path) {
            let len = file.metadata().map(|m| m.len()).unwrap_or(0);
            // Truncation / rotation: restart from the top.
            if len < offset {
                offset = 0;
            }
            if len > offset && file.seek(SeekFrom::Start(offset)).is_ok() {
                let mut chunk = String::new();
                if file.read_to_string(&mut chunk).is_ok() {
                    if let Some(newline) = chunk.rfind('\n') {
                        print!("{}", &chunk[..=newline]);
                        let _ = std::io::stdout().flush();
                        offset += (newline + 1) as u64;
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn help_and_unknown_are_handled() {
        assert_eq!(run(&[]).await, 0);
        assert_eq!(run(&["--help".to_string()]).await, 0);
        assert_eq!(run(&["bogus".to_string()]).await, EXIT_USAGE);
    }

    #[tokio::test]
    async fn path_command_returns_a_valid_code() {
        // No env mutation (process-global, parallel-test unsafe): just assert the
        // command completes with a valid code rather than panicking.
        let code = run(&["path".to_string()]).await;
        assert!(code == 0 || code == EXIT_RUNTIME);
    }
}
