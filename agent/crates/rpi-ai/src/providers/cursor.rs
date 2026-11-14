//! Native Cursor Agent Connect transport (`cursor-agent`).
//! Uses the released HTTP/1-compatible RunSSE/BidiAppend RPCs, including KV,
//! request-context and MCP execution handshakes. Tool calls remain pending on
//! the real server run until the host supplies their results on its next turn.
use crate::event_stream::{
    create_assistant_message_event_stream, AssistantMessageEventStreamProducer,
};
use crate::types::{AssistantMessageEvent as Event, ErrorReason, StopReason};
use crate::{AssistantMessageEventStream, Context, Message, Model, Provider, SimpleStreamOptions};
use futures::{Stream, StreamExt};
use serde_json::Value;
use std::{
    collections::HashMap,
    pin::Pin,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

mod request;
mod schema;
pub(super) mod wire;
use wire::{data, fields, number, text, Output, Proto, Result};

pub const API: &str = "cursor-agent";
pub const DEFAULT_BASE_URL: &str = "https://api2.cursor.sh";
pub const CLIENT_VERSION: &str = "cli-2026.09.02-c22c1a3";
const RUN_PATH: &str = "/agent.v1.AgentService/RunSSE";
const APPEND_PATH: &str = "/aiserver.v1.BidiService/BidiAppend";
const PARK_TTL: Duration = Duration::from_secs(15 * 60);

type ResponseStream =
    Pin<Box<dyn Stream<Item = std::result::Result<bytes::Bytes, reqwest::Error>> + Send>>;
struct PendingCall {
    exec_id: u64,
    exec_key: String,
}
#[derive(Clone)]
struct Append {
    http: reqwest::Client,
    url: reqwest::Url,
    headers: reqwest::header::HeaderMap,
    request_id: String,
    sequence: Arc<AtomicU64>,
    // Serialize append submission: sequence numbers describe actual wire order.
    lock: Arc<Mutex<()>>,
}
impl Append {
    async fn send(&self, message: Proto) -> Result<()> {
        let _guard = self.lock.lock().await;
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed);
        let body = Proto::default()
            .msg(2, Proto::default().bytes(1, &self.request_id))
            .int(3, sequence)
            .msg(4, message);
        wire::checked(
            self.http
                .post(self.url.clone())
                .headers(self.headers.clone())
                .header("content-type", "application/proto")
                .body(body.0)
                .timeout(Duration::from_secs(120))
                .send()
                .await
                .map_err(|e| e.to_string())?,
        )
        .await?;
        Ok(())
    }
    async fn exec(&self, id: u64, key: &str, variant: u32, result: Proto) -> Result<()> {
        self.send(
            Proto::default().msg(
                2,
                Proto::default()
                    .int(1, id)
                    .bytes(15, key)
                    .msg(variant, result),
            ),
        )
        .await
    }
    async fn reject_exec(&self, id: u64, message: &str) -> Result<()> {
        self.send(Proto::default().msg(
            5,
            Proto::default().msg(2, Proto::default().int(1, id).bytes(2, message)),
        ))
        .await?;
        self.send(Proto::default().msg(5, Proto::default().msg(1, Proto::default().int(1, id))))
            .await
    }
}
struct Run {
    response: ResponseStream,
    decoder: wire::Decoder,
    append: Append,
    blobs: request::Blobs,
    pending: HashMap<String, PendingCall>,
    context: Vec<u8>,
    listed: Option<u64>,
    served: u64,
    total_output: i64,
    reported_output: i64,
    turn_ended: bool,
    trailer: bool,
    lifetime: CancellationToken,
    heartbeat_error: Arc<Mutex<Option<String>>>,
    created: Instant,
    model: String,
    session: Option<String>,
}
impl Drop for Run {
    fn drop(&mut self) {
        self.lifetime.cancel();
    }
}
impl Run {
    fn handoff_ready(&self) -> bool {
        !self.pending.is_empty()
            && self
                .listed
                .is_some_and(|n| self.pending.len() as u64 + self.served >= n)
    }
}

pub struct CursorProvider {
    id: String,
    api_key: Option<String>,
    http: reqwest::Client,
    models: Vec<Model>,
    parked: Arc<Mutex<HashMap<String, Run>>>,
}
impl CursorProvider {
    pub fn with_models_without_env_api_key(
        id: impl Into<String>,
        api_key: Option<String>,
        http: reqwest::Client,
        models: Vec<Model>,
    ) -> Self {
        Self {
            id: id.into(),
            api_key,
            http,
            models,
            parked: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}
#[async_trait::async_trait]
impl Provider for CursorProvider {
    fn id(&self) -> &str {
        &self.id
    }
    fn models(&self) -> &[Model] {
        &self.models
    }
    async fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: &SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        let (mut tx, stream) = create_assistant_message_event_stream();
        let (model, context, mut options, http, parked) = (
            model.clone(),
            context.clone(),
            options.clone(),
            self.http.clone(),
            self.parked.clone(),
        );
        if options.api_key.is_none() {
            options.api_key = self.api_key.clone();
        }
        tokio::spawn(async move {
            let mut output = Output::new(&model);
            let cancellation = tx.cancellation_token();
            let timeout = options
                .timeout
                .unwrap_or(crate::provider::DEFAULT_LLM_API_TIMEOUT);
            let auth = wire::unwrap_api_key(&mut options.api_key);
            let result = tokio::select! {
                biased;
                _=options.signal.cancelled()=>Err("Request aborted".into()),
                _=cancellation.cancelled()=>Err("Request aborted".into()),
                result=tokio::time::timeout(timeout,async { auth?; turn(&http,&model,&context,&options,&parked,&mut output,&mut tx).await })=>result.unwrap_or_else(|_|Err("Cursor request timed out".into())),
            };
            if let Err(error) = result {
                let aborted = options.signal.is_cancelled() || cancellation.is_cancelled();
                output.message.stop_reason = if aborted {
                    StopReason::Aborted
                } else {
                    StopReason::Error
                };
                output.message.error_message = Some(error);
                tx.push(Event::Error {
                    reason: if aborted {
                        ErrorReason::Aborted
                    } else {
                        ErrorReason::Error
                    },
                    error: output.message,
                })
                .await;
            }
        });
        stream
    }
}
fn url(model: &Model, path: &str) -> Result<reqwest::Url> {
    reqwest::Url::parse(if model.base_url.is_empty() {
        DEFAULT_BASE_URL
    } else {
        &model.base_url
    })
    .and_then(|base| base.join(path))
    .map_err(|e| e.to_string())
}
fn request_headers(
    model: &Model,
    options: &SimpleStreamOptions,
    request_id: &str,
) -> Result<reqwest::header::HeaderMap> {
    let mut defaults = reqwest::header::HeaderMap::new();
    for (name, value) in [
        ("content-type", "application/connect+proto"),
        ("connect-protocol-version", "1"),
        ("x-cursor-client-type", "cli"),
        ("x-cursor-client-version", CLIENT_VERSION),
        ("x-ghost-mode", "true"),
        ("x-cursor-streaming", "true"),
        ("x-request-id", request_id),
    ] {
        defaults.insert(
            reqwest::header::HeaderName::from_static(name),
            value
                .parse::<reqwest::header::HeaderValue>()
                .map_err(|e| e.to_string())?,
        );
    }
    if let Some(key) = options.api_key.as_deref().filter(|s| !s.is_empty()) {
        defaults.insert(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {key}")
                .parse::<reqwest::header::HeaderValue>()
                .map_err(|e| e.to_string())?,
        );
    }
    defaults.extend(wire::headers(model, options)?);
    // Every host tool uses the MCP handoff; never silently execute built-ins remotely.
    defaults.insert(
        "x-cursor-agent-allowed-tools",
        reqwest::header::HeaderValue::from_static("mcp_tool_call,get_mcp_tools_tool_call"),
    );
    Ok(defaults)
}
async fn open(
    http: &reqwest::Client,
    model: &Model,
    context: &Context,
    options: &SimpleStreamOptions,
) -> Result<Run> {
    let request_id = uuid::Uuid::new_v4().to_string();
    let headers = request_headers(model, options, &request_id)?;
    if !headers.contains_key(reqwest::header::AUTHORIZATION) {
        return Err("Cursor API key (access token) is required".into());
    }
    let mut blobs = HashMap::new();
    let body = request::build(model, context, options, &mut blobs)?;
    let request_context = request::request_context(
        context,
        request::setting(model, "requiresCursorToolSchemaProjection")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    )?
    .0;
    let append = Append {
        http: http.clone(),
        url: url(model, APPEND_PATH)?,
        headers: headers.clone(),
        request_id: request_id.clone(),
        sequence: Arc::new(AtomicU64::new(0)),
        lock: Arc::new(Mutex::new(())),
    };
    // RunSSE and the initial BidiAppend must be live concurrently: RunSSE may
    // wait for the run request before it sends its response headers.
    let response = http
        .post(url(model, RUN_PATH)?)
        .headers(headers)
        .body(wire::frame(&Proto::default().bytes(1, &request_id).0))
        .send();
    let initial = append.send(body);
    let (response, ()) = tokio::try_join!(
        async { wire::checked(response.await.map_err(|e| e.to_string())?).await },
        initial
    )?;
    let lifetime = CancellationToken::new();
    let heartbeat_error = Arc::new(Mutex::new(None));
    let (heartbeat, stop, error) = (append.clone(), lifetime.clone(), heartbeat_error.clone());
    tokio::spawn(async move {
        let deadline = tokio::time::sleep(PARK_TTL);
        tokio::pin!(deadline);
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        interval.tick().await;
        loop {
            tokio::select! {
                _=stop.cancelled()=>break,
                _=&mut deadline=>{ *error.lock().await=Some("Cursor parked run expired".into()); break; },
                _=interval.tick()=>{
                    let sent=tokio::select! { _=stop.cancelled()=>break, result=heartbeat.send(Proto::default().msg(7,Proto::default()))=>result };
                    if let Err(e)=sent { *error.lock().await=Some(e); break; }
                }
            }
        }
    });
    Ok(Run {
        response: Box::pin(response.bytes_stream()),
        decoder: Default::default(),
        append,
        blobs,
        pending: HashMap::new(),
        context: request_context,
        listed: None,
        served: 0,
        total_output: 0,
        reported_output: 0,
        turn_ended: false,
        trailer: false,
        lifetime,
        heartbeat_error,
        created: Instant::now(),
        model: model.id.clone(),
        session: options.session_id.clone(),
    })
}
async fn turn(
    http: &reqwest::Client,
    model: &Model,
    context: &Context,
    options: &SimpleStreamOptions,
    parked: &Mutex<HashMap<String, Run>>,
    output: &mut Output,
    tx: &mut AssistantMessageEventStreamProducer,
) -> Result<()> {
    let results: HashMap<_, _> = context
        .messages
        .iter()
        .rev()
        .take_while(|m| matches!(m, Message::ToolResult(_)))
        .filter_map(|m| {
            if let Message::ToolResult(r) = m {
                Some((r.tool_call_id.as_str(), r))
            } else {
                None
            }
        })
        .collect();
    let resumed = {
        let mut runs = parked.lock().await;
        runs.retain(|_, run| run.created.elapsed() < PARK_TTL);
        let id = runs
            .iter()
            .find(|(_, run)| {
                run.model == model.id
                    && run.session == options.session_id
                    && !results.is_empty()
                    && run
                        .pending
                        .keys()
                        .any(|id| results.contains_key(id.as_str()))
            })
            .map(|(id, _)| id.clone());
        id.and_then(|id| runs.remove(&id))
    };
    let mut run = match resumed {
        Some(run) => run,
        None => open(http, model, context, options).await?,
    };
    if !run.pending.is_empty() {
        if !run
            .pending
            .keys()
            .all(|id| results.contains_key(id.as_str()))
        {
            return Err("Cursor continuation is missing a pending tool result".into());
        }
        let pending = std::mem::take(&mut run.pending);
        for (id, call) in pending {
            run.append
                .exec(
                    call.exec_id,
                    &call.exec_key,
                    11,
                    request::mcp_result(results[id.as_str()])?,
                )
                .await?;
        }
        run.listed = None;
        run.served = 0;
    }
    output.start(tx).await;
    let mut quiet: Option<Instant> = None;
    loop {
        while let Some((flag, bytes)) = run.decoder.next()? {
            if run.trailer {
                return Err("Cursor data after Connect trailer".into());
            }
            if flag & 2 != 0 {
                wire::trailer(&bytes)?;
                run.trailer = true;
                continue;
            }
            let held = run.pending.len();
            dispatch(&mut run, &bytes, output, tx).await?;
            if run.pending.len() > held {
                quiet = Some(Instant::now());
            }
            if run.handoff_ready() {
                break;
            }
        }
        if run.handoff_ready() {
            break;
        }
        if let Some(error) = run.heartbeat_error.lock().await.clone() {
            return Err(error);
        }
        if run.trailer {
            break;
        }
        let idle = options
            .stream_idle_timeout
            .unwrap_or(crate::provider::DEFAULT_STREAM_IDLE_TIMEOUT);
        let wait = if let Some(quiet) = quiet {
            Some(Duration::from_millis(150).saturating_sub(quiet.elapsed()))
        } else if idle.is_zero() {
            None
        } else {
            Some(idle)
        };
        let chunk = if let Some(wait) = wait {
            match tokio::time::timeout(wait, run.response.next()).await {
                Ok(chunk) => chunk,
                Err(_) if !run.pending.is_empty() => break,
                Err(_) => return Err("Cursor stream idle timeout".into()),
            }
        } else {
            run.response.next().await
        };
        match chunk {
            Some(Ok(chunk)) => run.decoder.push(&chunk),
            Some(Err(e)) => return Err(e.to_string()),
            None => {
                run.decoder.finish()?;
                break;
            }
        }
    }
    if !run.pending.is_empty() {
        let key = run.append.request_id.clone();
        output.message.usage.output = (run.total_output - run.reported_output).max(0);
        run.reported_output = run.total_output;
        parked.lock().await.insert(key.clone(), run);
        if let Err(error) = output.finish(tx, false).await {
            parked.lock().await.remove(&key);
            return Err(error);
        }
        return Ok(());
    }
    run.decoder.finish()?;
    if !run.trailer || !run.turn_ended {
        return Err("Cursor stream ended before TurnEnded and clean Connect trailer".into());
    }
    output.message.usage.output = (run.total_output - run.reported_output).max(0);
    output.finish(tx, false).await
}
fn mcp_call(bytes: &[u8]) -> Result<(String, String, Value, bool)> {
    let f = fields(bytes)?;
    let id = text(&f, 3)?;
    let name = text(&f, 5)?;
    let name = if name.is_empty() { text(&f, 1)? } else { name };
    let mut args = serde_json::Map::new();
    for entry in f.iter().filter(|f| f.n == 2) {
        let entry = fields(entry.value)?;
        args.insert(text(&entry, 1)?, wire::json_decode(data(&entry, 2))?);
    }
    Ok((id, name, Value::Object(args), number(&f, 7) != 0))
}
async fn dispatch(
    run: &mut Run,
    bytes: &[u8],
    output: &mut Output,
    tx: &mut AssistantMessageEventStreamProducer,
) -> Result<()> {
    for message in fields(bytes)? {
        match message.n {
            1 => {
                for update in fields(message.value)? {
                    match update.n {
                        1 | 4 => {
                            let f = fields(update.value)?;
                            output.delta(tx, text(&f, 1)?, update.n == 4, None).await;
                        }
                        5 => output.end_blocks(tx).await,
                        2 | 7 => {
                            let f = fields(update.value)?;
                            let tool = fields(data(&f, 2))?;
                            if let Some(mcp) = tool.iter().find(|f| f.n == 15) {
                                let mcp = fields(mcp.value)?;
                                let (mut id, name, args, approval) = mcp_call(data(&mcp, 1))?;
                                if approval {
                                    continue;
                                }
                                if id.is_empty() {
                                    id = text(&tool, 57)?;
                                }
                                if id.is_empty() {
                                    id = text(&f, 1)?;
                                }
                                if !id.is_empty() {
                                    let partial = if update.n == 7 {
                                        text(&f, 3)?
                                    } else {
                                        String::new()
                                    };
                                    // Announcements often have no args; the exec frame is authoritative.
                                    if !partial.is_empty() {
                                        output.tool(tx, id, name, partial).await?;
                                    } else if args.as_object().is_some_and(|a| !a.is_empty()) {
                                        output.tool_snapshot(tx, id, name, args).await?;
                                    }
                                }
                            } else if update.n == 2 {
                                run.served += 1;
                            }
                        }
                        8 => {
                            let f = fields(update.value)?;
                            run.total_output += number(&f, 1) as i64;
                            output.message.usage.output = run.total_output;
                        }
                        14 => {
                            let f = fields(update.value)?;
                            let (input, output_tokens, cache_read, cache_write) = (
                                number(&f, 1) as i64,
                                number(&f, 2) as i64,
                                number(&f, 3) as i64,
                                number(&f, 4) as i64,
                            );
                            if input > 0 {
                                output.message.usage.input =
                                    (input - cache_read - cache_write).max(0);
                            }
                            if output_tokens > 0 {
                                run.total_output = output_tokens;
                                output.message.usage.output = output_tokens;
                            }
                            if cache_read > 0 {
                                output.message.usage.cache_read = cache_read;
                            }
                            if cache_write > 0 {
                                output.message.usage.cache_write = cache_write;
                            }
                            if number(&f, 5) > 0 {
                                output.message.usage.reasoning = Some(number(&f, 5) as i64);
                            }
                            run.turn_ended = true;
                        }
                        27 => {
                            let f = fields(update.value)?;
                            run.listed = Some(number(&f, 1));
                            if run.pending.is_empty() && run.served >= number(&f, 1) {
                                run.listed = None;
                                run.served = 0;
                            }
                        }
                        _ => {}
                    }
                }
            }
            2 => {
                let f = fields(message.value)?;
                let id = number(&f, 1);
                let key = text(&f, 15)?;
                if f.iter().any(|f| f.n == 10) {
                    let result = Proto::default().msg(1, Proto::default().bytes(1, &run.context));
                    run.append.exec(id, &key, 10, result).await?;
                } else if let Some(args) = f.iter().find(|f| f.n == 11) {
                    let (call_id, name, args_value, approval) = mcp_call(args.value)?;
                    if approval {
                        run.append
                            .exec(
                                id,
                                &key,
                                11,
                                Proto::default().msg(
                                    3,
                                    Proto::default()
                                        .bytes(1, "Tool execution requires host authorization"),
                                ),
                            )
                            .await?;
                        continue;
                    }
                    if call_id.is_empty() {
                        return Err("Cursor MCP invocation has no tool call id".into());
                    }
                    if !run.pending.contains_key(&call_id) {
                        output
                            .tool_snapshot(tx, call_id.clone(), name, args_value)
                            .await?;
                        run.pending.insert(
                            call_id,
                            PendingCall {
                                exec_id: id,
                                exec_key: key,
                            },
                        );
                    }
                } else if f.iter().any(|f| f.n == 42) {
                    run.append
                        .exec(id, &key, 42, Proto::default().int(1, 0))
                        .await?;
                } else if let Some(state) = f.iter().find(|f| f.n == 36) {
                    let state = fields(state.value)?;
                    let wanted: Vec<_> =
                        state.iter().filter(|f| f.n == 1).map(|f| f.value).collect();
                    let mut success = Proto::default();
                    if wanted.is_empty() || wanted.iter().any(|id| *id == b"pi-agent") {
                        let mut server = Proto::default()
                            .bytes(1, "pi-agent")
                            .bytes(2, "pi-agent")
                            .bytes(7, "connected");
                        let mut has_tools = false;
                        for tool in fields(&run.context)?.iter().filter(|f| f.n == 7) {
                            server = server.bytes(5, tool.value);
                            has_tools = true;
                        }
                        if has_tools {
                            success = success.msg(1, server);
                        }
                    }
                    run.append
                        .exec(id, &key, 36, Proto::default().msg(1, success))
                        .await?;
                } else if let Some(hook) = f.iter().find(|f| f.n == 27) {
                    let hook = fields(hook.value)?;
                    let hook = fields(data(&hook, 1))?;
                    // No Cursor-local hooks are configured. A matching response
                    // case with no optional fields is the protocol's neutral reply.
                    if let Some(query) = hook.iter().find(|f| matches!(f.n, 1..=9 | 11)) {
                        run.append
                            .exec(
                                id,
                                &key,
                                27,
                                Proto::default()
                                    .msg(1, Proto::default().msg(query.n, Proto::default())),
                            )
                            .await?;
                    } else {
                        run.append
                            .reject_exec(id, "Unknown Cursor hook query")
                            .await?;
                    }
                } else {
                    run.append
                        .reject_exec(id, "This run delegates all tools to the host through MCP")
                        .await?;
                }
            }
            4 => {
                let f = fields(message.value)?;
                let id = number(&f, 1);
                if let Some(get) = f.iter().find(|f| f.n == 2) {
                    let get = fields(get.value)?;
                    let blob = run.blobs.get(data(&get, 1));
                    let result = if let Some(blob) = blob {
                        Proto::default().bytes(1, blob)
                    } else {
                        Proto::default()
                    };
                    run.append
                        .send(Proto::default().msg(3, Proto::default().int(1, id).msg(2, result)))
                        .await?;
                } else if let Some(set) = f.iter().find(|f| f.n == 3) {
                    let set = fields(set.value)?;
                    run.blobs
                        .insert(data(&set, 1).to_vec(), data(&set, 2).to_vec());
                    run.append
                        .send(
                            Proto::default()
                                .msg(3, Proto::default().int(1, id).msg(3, Proto::default())),
                        )
                        .await?;
                }
            }
            7 => {
                let f = fields(message.value)?;
                let id = number(&f, 1);
                if let Some(query) = f.iter().find(|f| f.n >= 2) {
                    let result = match query.n {
                        // Hosted read-only web operations use the server's native approval handshake.
                        2 | 5 | 6 | 9 => Proto::default().msg(1, Proto::default()),
                        3 => Proto::default().msg(
                            1,
                            Proto::default()
                                .msg(2, Proto::default().bytes(1, "Answer through a host tool")),
                        ),
                        4 => Proto::default().msg(
                            2,
                            Proto::default()
                                .bytes(1, "This client keeps the host's conversation mode"),
                        ),
                        7 => Proto::default().msg(
                            1,
                            Proto::default().msg(
                                2,
                                Proto::default()
                                    .bytes(1, "Plan files must be created through a host tool"),
                            ),
                        ),
                        _ => {
                            return Err(format!(
                                "Cursor requested an unsupported interactive operation {}",
                                query.n
                            ))
                        }
                    };
                    run.append
                        .send(
                            Proto::default()
                                .msg(6, Proto::default().int(1, id).msg(query.n, result)),
                        )
                        .await?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[tokio::test]
    async fn mcp_exec_is_held_until_host_returns_the_real_result() {
        let model = model();
        let append = Append {
            http: reqwest::Client::new(),
            url: "http://127.0.0.1:1/never-call".parse().unwrap(),
            headers: Default::default(),
            request_id: "request".into(),
            sequence: Arc::new(AtomicU64::new(0)),
            lock: Arc::new(Mutex::new(())),
        };
        let mut run = Run {
            response: Box::pin(futures::stream::empty()),
            decoder: Default::default(),
            append,
            blobs: Default::default(),
            pending: Default::default(),
            context: Vec::new(),
            listed: Some(1),
            served: 0,
            total_output: 0,
            reported_output: 0,
            turn_ended: false,
            trailer: false,
            lifetime: CancellationToken::new(),
            heartbeat_error: Arc::new(Mutex::new(None)),
            created: Instant::now(),
            model: model.id.clone(),
            session: None,
        };
        let args = Proto::default().bytes(1, "lookup").bytes(3, "call").msg(
            2,
            Proto::default()
                .bytes(1, "query")
                .msg(2, wire::json_encode(&json!("needle"))),
        );
        let message = Proto::default().msg(
            2,
            Proto::default()
                .int(1, 9)
                .bytes(15, "exec-key")
                .msg(11, args),
        );
        let (mut tx, stream) = create_assistant_message_event_stream();
        let mut output = Output::new(&model);
        dispatch(&mut run, &message.0, &mut output, &mut tx)
            .await
            .unwrap();
        assert!(run.handoff_ready());
        assert_eq!(run.pending["call"].exec_id, 9);
        assert_eq!(
            run.append.sequence.load(Ordering::Relaxed),
            0,
            "handoff must not send fabricated tool results"
        );
        output.finish(&mut tx, false).await.unwrap();
        let result = stream.result().await.unwrap();
        assert_eq!(result.stop_reason, StopReason::ToolUse);
        assert!(
            matches!(&result.content[0],crate::types::Content::ToolCall(call) if call.id=="call" && call.arguments==json!({"query":"needle"}))
        );
    }
    fn model() -> Model {
        Model::new(
            "test",
            "Test",
            crate::Api::Other(API.into()),
            "cursor",
            DEFAULT_BASE_URL,
        )
    }
    #[test]
    fn native_auth_headers_allow_runtime_overrides() {
        let options = SimpleStreamOptions {
            api_key: Some("token".into()),
            headers: Some(std::collections::BTreeMap::from([(
                "Authorization".into(),
                "Bearer resolved".into(),
            )])),
            ..Default::default()
        };
        let h = request_headers(&model(), &options, "id").unwrap();
        assert_eq!(h["authorization"], "Bearer resolved");
        assert_eq!(h["x-cursor-client-version"], CLIENT_VERSION);
        assert_eq!(
            h["x-cursor-agent-allowed-tools"],
            "mcp_tool_call,get_mcp_tools_tool_call"
        );
    }
    #[test]
    fn mcp_arguments_are_protobuf_values_not_json_bytes() {
        let args = Proto::default().bytes(1, "lookup").bytes(3, "call").msg(
            2,
            Proto::default()
                .bytes(1, "query")
                .msg(2, wire::json_encode(&json!("needle"))),
        );
        let (id, name, args, approval) = mcp_call(&args.0).unwrap();
        assert_eq!(id, "call");
        assert_eq!(name, "lookup");
        assert_eq!(args, json!({"query":"needle"}));
        assert!(!approval);
    }
    #[tokio::test]
    async fn native_text_and_thinking_map_to_ordered_blocks() {
        let (mut tx, stream) = create_assistant_message_event_stream();
        let mut out = Output::new(&model());
        out.delta(&mut tx, "reason".into(), true, None).await;
        out.delta(&mut tx, "answer".into(), false, None).await;
        out.finish(&mut tx, false).await.unwrap();
        let message = stream.result().await.unwrap();
        assert!(
            matches!(&message.content[0],crate::types::Content::Thinking(t) if t.thinking=="reason")
        );
        assert!(matches!(&message.content[1],crate::types::Content::Text(t) if t.text=="answer"));
    }
}
