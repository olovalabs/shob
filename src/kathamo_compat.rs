//! Scoped compatibility with Kathamo's native reporting subset, not its general API.
//! Protocol compatible with Herdr native reporting.
use crate::status_bus::StatusEvent;
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::LazyLock;

#[derive(Default)]
struct Source {
    external_sequence: Option<u64>,
    sequence: u64,
    session: Option<String>,
    state: String,
}

struct Route {
    app: String,
    pane: u64,
    sources: HashMap<String, Source>,
}

static ROUTES: LazyLock<Mutex<HashMap<String, Route>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn register(app: &str, pane: usize, generation: &str) {
    ROUTES.lock().insert(
        generation.into(),
        Route {
            app: app.into(),
            pane: pane as u64,
            sources: HashMap::new(),
        },
    );
}

pub fn unregister(generation: &str) {
    ROUTES.lock().remove(generation);
}

/// Only the documented reporting commands are exposed through KATHAMO_BIN_PATH / HERDR_BIN_PATH.
/// Requests for unsupported API commands fail instead of starting a GUI window.
pub fn run_cli(args: &[String]) -> anyhow::Result<()> {
    use anyhow::{Context, bail};
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    let method = match args.first().map(String::as_str) {
        Some("report-agent") => "pane.report_agent",
        Some("report-agent-session") => "pane.report_agent_session",
        Some("release-agent") => "pane.release_agent",
        _ => {
            bail!(
                "Kathamo supports only pane report-agent, report-agent-session, and release-agent"
            )
        }
    };
    let pane = args.get(1).context("missing pane token")?;
    if std::env::var("KATHAMO_PANE_GENERATION")
        .or_else(|_| std::env::var("MARKUP_PANE_GENERATION"))
        .ok()
        .as_ref()
        != Some(pane)
    {
        bail!("report must target the launching Kathamo pane");
    }
    let mut params = serde_json::Map::new();
    params.insert("pane_id".into(), pane.clone().into());
    let mut options = args[2..].iter();
    while let Some(option) = options.next() {
        if option == "--" {
            break;
        } // Resume argv is outside the reporting subset.
        let key = match option.as_str() {
            "--source" => "source",
            "--agent" => "agent",
            "--state" => "state",
            "--seq" => "seq",
            "--message" => "message",
            "--agent-session-id" => "agent_session_id",
            _ => bail!("unsupported reporting option: {option}"),
        };
        let value = options
            .next()
            .with_context(|| format!("missing value for {option}"))?;
        let value = if key == "seq" {
            Value::from(value.parse::<u64>().context("invalid sequence")?)
        } else {
            Value::from(value.clone())
        };
        params.insert(key.into(), value);
    }
    let socket = std::env::var_os("KATHAMO_STATUS_SOCKET")
        .or_else(|| std::env::var_os("MARKUP_STATUS_SOCKET"))
        .context("not running in a Kathamo terminal")?;
    let mut stream = UnixStream::connect(std::path::Path::new(&socket))?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(1)))?;
    serde_json::to_writer(
        &mut stream,
        &json!({"id":"kathamo-report", "method":method,"params":params}),
    )?;
    stream.write_all(b"\n")?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    let response: Value = serde_json::from_str(&line)?;
    if response.get("ok") != Some(&Value::Bool(true)) {
        bail!(
            "report rejected: {}",
            response["error"]["message"]
                .as_str()
                .unwrap_or("invalid response")
        );
    }
    Ok(())
}

/// Every route is registered before its child starts and removed with its pane.
/// A reporter cannot turn an integer pane ID into access to another generation.
pub fn request(input: Value) -> (Vec<StatusEvent>, Value) {
    let id = input.get("id").cloned().unwrap_or(Value::Null);
    let fail = |message: &str| {
        (
            Vec::new(),
            json!({"id":id,"ok":false,"error":{"code":"invalid_report","message":message}}),
        )
    };
    let Some(method) = input.get("method").and_then(Value::as_str) else {
        return fail("missing method");
    };
    let Some(params) = input.get("params").and_then(Value::as_object) else {
        return fail("missing params");
    };
    let Some(token) = params.get("pane_id").and_then(Value::as_str) else {
        return fail("missing pane token");
    };
    let mut routes = ROUTES.lock();
    let Some(route) = routes.get_mut(token) else {
        return fail("pane is closed or belongs to another generation");
    };
    if !matches!(
        method,
        "pane.report_agent" | "pane.report_agent_session" | "pane.release_agent"
    ) {
        return fail("only native agent reporting is supported");
    }
    let Some(source) = params
        .get("source")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && s.len() <= 64 && !s.chars().any(char::is_control))
    else {
        return fail("invalid reporter source");
    };
    let session = match params.get("agent_session_id") {
        None | Some(Value::Null) => None,
        Some(Value::String(s))
            if !s.is_empty() && s.len() <= 256 && !s.chars().any(char::is_control) =>
        {
            Some(s.clone())
        }
        _ => return fail("invalid session identity"),
    };
    let state = match method {
        "pane.report_agent" => match params.get("state").and_then(Value::as_str) {
            Some("idle") => "idle",
            Some("working") => "working",
            Some("blocked") => "blocked",
            _ => return fail("invalid agent state"),
        },
        "pane.report_agent_session" if session.is_some() => "register",
        "pane.report_agent_session" => return fail("missing native session identity"),
        _ => "clear",
    };
    let external_sequence = match params.get("seq") {
        None | Some(Value::Null) => None,
        Some(value) => match value.as_u64() {
            Some(n) if n > 0 => Some(n),
            _ => return fail("invalid sequence"),
        },
    };
    if !route.sources.contains_key(source) && route.sources.len() >= 32 {
        return fail("reporter limit exceeded");
    }
    let current = route.sources.entry(source.into()).or_default();
    if external_sequence.is_some_and(|n| current.external_sequence.is_some_and(|old| n <= old)) {
        return (
            Vec::new(),
            json!({"id":id,"ok":true,"result":{"accepted":false}}),
        );
    }
    if let Some(n) = external_sequence {
        current.external_sequence = Some(n);
    }
    let mut events = Vec::new();
    let mut make = |event: &str, native_session: Option<String>| {
        current.sequence += 1;
        StatusEvent {
            version: 1,
            app_id: route.app.clone(),
            pane_id: route.pane,
            pane_generation: token.into(),
            reporter: source.into(),
            reporter_generation: format!("kathamo:{token}"),
            sequence: current.sequence,
            session_id: native_session,
            turn_id: None,
            event: event.into(),
            message: params
                .get("message")
                .and_then(Value::as_str)
                .map(|s| s.chars().take(512).collect()),
            approval_id: None,
            agent_id: None,
            parent: None,
        }
    };
    if let Some(session) = session {
        if current.session.as_ref() != Some(&session) {
            current.session = Some(session.clone());
            current.state.clear();
            events.push(make("register", Some(session)));
        }
    }
    if state != "register" {
        // Initial idle means ready; only a native busy -> idle transition
        // settles a turn. Repeated idle preserves completion.
        let next = if state == "idle"
            && matches!(current.state.as_str(), "working" | "blocked" | "done")
        {
            "done"
        } else {
            state
        };
        current.state = next.into();
        events.push(make(next, current.session.clone()));
    }
    (
        events,
        json!({"id":id,"ok":true,"result":{"accepted":true}}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn report(token: &str, method: &str, state: &str, seq: Option<u64>, session: &str) -> Value {
        json!({"id":"request", "method":method,"params":{"pane_id":token,"source":"custom:cmd", "agent_session_id":session,"state":state,"seq":seq}})
    }

    #[test]
    fn native_reports_settle_repeated_turns_and_reject_late_or_closed_panes() {
        let token = "test-native-lifecycle";
        register("test-app", 17, token);
        let (events, _) = request(report(
            token,
            "pane.report_agent",
            "idle",
            Some(1),
            "session-a",
        ));
        assert_eq!(
            events.iter().map(|e| e.event.as_str()).collect::<Vec<_>>(),
            ["register", "idle"]
        );
        let (events, _) = request(report(
            token,
            "pane.report_agent",
            "working",
            Some(2),
            "session-a",
        ));
        assert_eq!(events[0].event, "working");
        let (events, _) = request(report(
            token,
            "pane.report_agent",
            "idle",
            Some(3),
            "session-a",
        ));
        assert_eq!(events[0].event, "done");
        assert!(
            request(report(
                token,
                "pane.report_agent",
                "working",
                Some(2),
                "session-a"
            ))
            .0
            .is_empty()
        );
        assert_eq!(
            request(report(
                token,
                "pane.report_agent",
                "idle",
                Some(4),
                "session-a"
            ))
            .0[0]
                .event,
            "done"
        );
        assert_eq!(
            request(report(
                token,
                "pane.report_agent",
                "working",
                Some(5),
                "session-a"
            ))
            .0[0]
                .event,
            "working"
        );
        assert_eq!(
            request(report(
                token,
                "pane.report_agent",
                "idle",
                Some(6),
                "session-b"
            ))
            .0
            .last()
            .unwrap()
            .event,
            "idle"
        );
        unregister(token);
        assert_eq!(
            request(report(
                token,
                "pane.report_agent",
                "idle",
                Some(7),
                "session-b"
            ))
            .1["ok"],
            false
        );
    }

    #[test]
    fn unsequenced_selection_does_not_collide_with_native_state_sequence() {
        let token = "test-native-selection";
        register("test-app", 18, token);
        let (first, _) = request(report(
            token,
            "pane.report_agent",
            "working",
            Some(100),
            "session-a",
        ));
        let (second, _) = request(report(
            token,
            "pane.report_agent_session",
            "",
            None,
            "session-b",
        ));
        let (third, _) = request(report(
            token,
            "pane.report_agent",
            "idle",
            Some(101),
            "session-b",
        ));
        assert!(first.last().unwrap().sequence < second[0].sequence);
        assert!(second[0].sequence < third[0].sequence);
        assert_eq!(third[0].event, "idle");
        unregister(token);
    }
}
