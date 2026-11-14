//! Non-interactive Editor workload for an external process resource sampler.
//! JSONL on stdin/stdout only; never opens a terminal, network, or history file.
//! Build: cargo build -p rpi-tui --example editor_resource_probe --release
//!        --target-dir target/resource-tests-cargo

use std::io::{self, BufRead, Write};
use std::process::ExitCode;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use rpi_tui::component::Component;
use rpi_tui::Editor;
use serde_json::{json, Value};

const MAX_REQUEST_BYTES: usize = 4096;
const MAX_PASTE_BYTES: u64 = 4 * 1024 * 1024;
const MAX_EDITS: u64 = 512;

#[derive(Debug, PartialEq)]
enum Request {
    Prepare { paste_bytes: usize, edits: usize },
    Round(u64),
    Reset,
    Stop,
}

fn positive_integer(value: &Value, field: &str, max: u64) -> Result<u64, String> {
    value
        .get(field)
        .and_then(Value::as_u64)
        .filter(|number| (1..=max).contains(number))
        .ok_or_else(|| format!("{field} must be an integer in 1..={max}"))
}

fn parse_request(bytes: &[u8]) -> Result<Request, String> {
    // Do not echo parser diagnostics: malformed input may contain large or
    // sensitive strings, and all diagnostics must remain bounded.
    let value: Value = serde_json::from_slice(bytes).map_err(|_| "invalid JSON request")?;
    match value.get("type").and_then(Value::as_str) {
        Some("prepare") => Ok(Request::Prepare {
            paste_bytes: positive_integer(&value, "pasteBytes", MAX_PASTE_BYTES)? as usize,
            edits: positive_integer(&value, "edits", MAX_EDITS)? as usize,
        }),
        Some("round") => Ok(Request::Round(positive_integer(&value, "round", u64::MAX)?)),
        Some("reset") => Ok(Request::Reset),
        Some("stop") => Ok(Request::Stop),
        _ => Err("unknown or missing command type".into()),
    }
}

/// Fixed storage, including for an unterminated or arbitrarily long request.
/// The 4096-byte limit excludes LF (but includes an optional CR).
fn read_request(reader: &mut impl BufRead) -> Result<Request, String> {
    let mut bytes = [0_u8; MAX_REQUEST_BYTES];
    let mut length = 0;
    loop {
        let available = reader.fill_buf().map_err(|_| "failed to read request")?;
        if available.is_empty() {
            return Err("unexpected EOF before stop or missing request newline".into());
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let count = newline.unwrap_or(available.len());
        if count > MAX_REQUEST_BYTES - length {
            return Err("request exceeds 4096 bytes".into());
        }
        bytes[length..length + count].copy_from_slice(&available[..count]);
        length += count;
        reader.consume(count + usize::from(newline.is_some()));
        if newline.is_some() {
            return parse_request(&bytes[..length]);
        }
    }
}

struct Probe {
    editor: Editor,
    payload: Option<String>,
    edits: usize,
    last_round: u64,
}

impl Probe {
    fn new() -> Self {
        // Warm code paths before announcing the baseline sampling point. The
        // warm-up editor is then dropped, including its text and undo history.
        let warm = Editor::simple();
        warm.handle_paste("warm small text");
        warm.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        warm.undo();
        warm.redo();
        drop(warm.get_expanded_text());
        drop(warm.render(80));
        drop(warm);
        Self {
            editor: Editor::simple(),
            payload: None,
            edits: 0,
            last_round: 0,
        }
    }

    fn reset(&mut self) {
        // Drop the entire editor, not just its visible content: history and
        // folded-paste storage must be released for the sampler's reset phase.
        self.editor = Editor::simple();
        self.payload = None;
        self.edits = 0;
        // Round identifiers remain strictly increasing for the whole process,
        // including across prepare/reset commands.
    }

    fn prepare(&mut self, paste_bytes: usize, edits: usize) -> Result<Value, String> {
        self.reset();
        let payload = "p".repeat(paste_bytes);
        self.editor.handle_paste(&payload);
        if self.editor.get_expanded_text() != payload {
            return Err("prepared paste integrity check failed".into());
        }
        if paste_bytes > 1000 && !self.editor.get_text().starts_with("[paste #") {
            return Err("large paste was not folded".into());
        }
        self.payload = Some(payload);
        self.edits = edits;
        Ok(json!({"type": "prepared", "ok": true, "pasteBytes": paste_bytes, "edits": edits}))
    }

    fn round(&mut self, round: u64) -> Result<Value, String> {
        if round <= self.last_round {
            return Err("round must be positive and strictly increasing".into());
        }
        let payload = self
            .payload
            .as_ref()
            .ok_or("prepare is required before round")?;
        self.editor.insert(&"a".repeat(self.edits));
        for _ in 0..self.edits {
            self.editor
                .handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        }
        if self.editor.get_expanded_text() != *payload {
            return Err("backspace paste integrity check failed".into());
        }
        self.editor.undo();
        let restored = self.editor.get_expanded_text();
        if restored.len() != payload.len() + 1
            || !restored.starts_with(payload.as_str())
            || !restored.ends_with('a')
        {
            return Err("undo did not restore payload plus one character".into());
        }
        drop(restored);
        self.editor.redo();
        if self.editor.render(80).is_empty() {
            return Err("editor render returned no lines".into());
        }
        if self.editor.get_expanded_text() != *payload {
            return Err("redo paste integrity check failed".into());
        }
        self.last_round = round;
        Ok(json!({"type": "round_complete", "round": round, "ok": true,
            "pasteBytes": payload.len(), "edits": self.edits}))
    }
}

fn respond(writer: &mut impl Write, response: &Value) -> Result<(), String> {
    serde_json::to_writer(&mut *writer, response).map_err(|_| "failed to write response")?;
    writer
        .write_all(b"\n")
        .map_err(|_| "failed to write response")?;
    writer
        .flush()
        .map_err(|_| "failed to flush response".into())
}

fn serve(reader: &mut impl BufRead, writer: &mut impl Write) -> Result<(), String> {
    let mut probe = Probe::new();
    respond(writer, &json!({"type": "ready", "ok": true}))?;
    loop {
        let response = match read_request(reader)? {
            Request::Prepare { paste_bytes, edits } => probe.prepare(paste_bytes, edits)?,
            Request::Round(round) => probe.round(round)?,
            Request::Reset => {
                probe.reset();
                json!({"type": "reset_complete", "ok": true})
            }
            Request::Stop => {
                respond(writer, &json!({"type": "stopped", "ok": true}))?;
                return Ok(());
            }
        };
        respond(writer, &response)?;
    }
}

fn report_error(writer: &mut impl Write, error: &str) {
    let _ = respond(
        writer,
        &json!({"type": "error", "ok": false, "error": error}),
    );
    eprintln!("editor_resource_probe: {error}");
}

fn main() -> ExitCode {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut reader = stdin.lock();
    let mut writer = stdout.lock();
    let result = if std::env::args_os().len() != 1 {
        Err("this probe accepts no CLI arguments".into())
    } else {
        serve(&mut reader, &mut writer)
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            report_error(&mut writer, &error);
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufReader, Cursor};

    fn transcript(input: &[u8]) -> (Result<(), String>, Vec<Value>) {
        // Tiny buffering also exercises requests split across fill_buf calls.
        let mut reader = BufReader::with_capacity(7, Cursor::new(input));
        let mut output = Vec::new();
        let result = serve(&mut reader, &mut output);
        if let Err(error) = &result {
            report_error(&mut output, error);
        }
        let responses = output
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice(line).unwrap())
            .collect();
        (result, responses)
    }

    #[test]
    fn bounds_and_malformed_commands() {
        for request in [
            r#"{"type":"prepare","pasteBytes":0,"edits":1}"#,
            r#"{"type":"prepare","pasteBytes":4194305,"edits":1}"#,
            r#"{"type":"prepare","pasteBytes":1,"edits":0}"#,
            r#"{"type":"prepare","pasteBytes":1,"edits":513}"#,
            r#"{"type":"prepare","pasteBytes":1.0,"edits":1}"#,
            r#"{"type":"prepare","pasteBytes":1}"#,
            r#"{"type":"round","round":0}"#,
            r#"{"type":"round","round":-1}"#,
            r#"{"type":"round","round":18446744073709551616}"#,
            r#"{"type":"unknown"}"#,
            "null",
            "{",
            "",
        ] {
            assert!(parse_request(request.as_bytes()).is_err(), "{request}");
        }
        assert_eq!(
            parse_request(br#"{"type":"prepare","pasteBytes":4194304,"edits":512}"#).unwrap(),
            Request::Prepare {
                paste_bytes: 4194304,
                edits: 512
            }
        );
        assert_eq!(
            parse_request(br#"{"type":"prepare","pasteBytes":1,"edits":1}"#).unwrap(),
            Request::Prepare {
                paste_bytes: 1,
                edits: 1
            }
        );
    }

    #[test]
    fn capped_reader_and_eof() {
        let mut exact = br#"{"type":"stop"}"#.to_vec();
        exact.resize(MAX_REQUEST_BYTES, b' ');
        exact.push(b'\n');
        assert_eq!(
            read_request(&mut Cursor::new(&exact)).unwrap(),
            Request::Stop
        );
        exact.insert(0, b' ');
        assert!(read_request(&mut Cursor::new(exact))
            .unwrap_err()
            .contains("4096"));
        for input in [b"".as_slice(), b"{\"type\":\"stop\"}", &[0xff, b'\n']] {
            assert!(read_request(&mut Cursor::new(input)).is_err());
        }
        assert_eq!(
            read_request(&mut Cursor::new(b"{\"type\":\"stop\"}\r\n")).unwrap(),
            Request::Stop
        );
    }

    #[test]
    fn two_round_protocol_and_reset() {
        let (result, responses) = transcript(
            concat!(
                "{\"type\":\"prepare\",\"pasteBytes\":1048576,\"edits\":64}\n",
                "{\"type\":\"round\",\"round\":1}\n",
                "{\"type\":\"round\",\"round\":2}\n",
                "{\"type\":\"reset\"}\n",
                "{\"type\":\"stop\"}\n"
            )
            .as_bytes(),
        );
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(
            responses,
            vec![
                json!({"type":"ready","ok":true}),
                json!({"type":"prepared","ok":true,"pasteBytes":1048576,"edits":64}),
                json!({"type":"round_complete","ok":true,"round":1,"pasteBytes":1048576,"edits":64}),
                json!({"type":"round_complete","ok":true,"round":2,"pasteBytes":1048576,"edits":64}),
                json!({"type":"reset_complete","ok":true}),
                json!({"type":"stopped","ok":true}),
            ]
        );
    }

    #[test]
    fn round_order_and_reset_release_state() {
        let mut probe = Probe::new();
        assert!(probe.round(1).is_err());
        probe.prepare(1024 * 1024, 64).unwrap();
        assert!(probe.editor.get_text().starts_with("[paste #"));
        probe.round(1).unwrap();
        assert!(probe.round(1).is_err());
        assert!(probe.round(0).is_err());
        probe.reset();
        assert!(probe.payload.is_none());
        assert_eq!(probe.editor.get_expanded_text(), "");
        probe.editor.undo();
        assert_eq!(probe.editor.get_expanded_text(), "");
        assert!(probe.round(2).is_err());
        probe.prepare(1, 1).unwrap();
        assert!(probe.round(1).is_err());
        probe.round(2).unwrap();
        probe
            .prepare(MAX_PASTE_BYTES as usize, MAX_EDITS as usize)
            .unwrap();
        probe.round(3).unwrap();
    }

    #[test]
    fn protocol_error_is_bounded_and_terminal() {
        let (result, responses) =
            transcript(b"{\"type\":\"round\",\"round\":1}\n{\"type\":\"stop\"}\n");
        assert!(result.is_err());
        assert_eq!(responses.len(), 2);
        assert_eq!(responses[1]["type"], "error");
        assert_eq!(responses[1]["ok"], false);
        assert!(responses[1]["error"].as_str().unwrap().len() < 128);
    }
}
