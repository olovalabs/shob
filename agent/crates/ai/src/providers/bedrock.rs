//! Native Amazon Bedrock ConverseStream over AWS binary eventstream framing.
mod auth;
mod eventstream;
mod request;
mod stream;

use crate::event_stream::{
    create_assistant_message_event_stream_with_cancellation, AssistantMessageEventStreamProducer,
};
use crate::{
    Api, AssistantMessage, AssistantMessageEvent, AssistantMessageEventStream, Context, DoneReason,
    ErrorReason, Model, Provider, SimpleStreamOptions, StopReason,
};
use async_trait::async_trait;
use futures::StreamExt;
use std::collections::BTreeMap;

pub struct BedrockProvider {
    id: String,
    api_key: Option<String>,
    http: reqwest::Client,
    models: Vec<Model>,
}
impl BedrockProvider {
    /// Credentials are runtime-resolved: `api_key` is a Bedrock bearer token;
    /// SigV4 credentials arrive through private `x-omp-aws-*` headers, never sent.
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
        }
    }
}
#[async_trait]
impl Provider for BedrockProvider {
    fn id(&self) -> &str {
        &self.id
    }
    fn models(&self) -> &[Model] {
        &self.models
    }
    async fn stream_simple(
        &self,
        model: &Model,
        ctx: &Context,
        opts: &SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        let (mut producer, stream) =
            create_assistant_message_event_stream_with_cancellation(&opts.signal);
        let preparation = prepare(model, ctx, opts, self.api_key.as_deref());
        let http = self.http.clone();
        let model = model.clone();
        let mut opts = opts.clone();
        opts.signal = producer.cancellation_token();
        tokio::spawn(async move {
            let now = time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000;
            let mut out = AssistantMessage::empty(
                Api::BedrockConverseStream,
                &model.provider,
                &model.id,
                now as i64,
            );
            let work = async {
                let request = preparation?;
                run(&mut producer, &mut out, &model, &opts, http, request).await
            };
            let timeout =
                tokio::time::sleep(opts.timeout.unwrap_or(crate::DEFAULT_LLM_API_TIMEOUT));
            tokio::pin!(timeout);
            let result = tokio::select! {
                biased;
                _ = opts.signal.cancelled() => Err("Request aborted".into()),
                _ = &mut timeout => Err("Bedrock request timed out".into()),
                result = work => result,
            };
            match result {
                Ok(()) => {
                    let reason = match out.stop_reason {
                        StopReason::Length => DoneReason::Length,
                        StopReason::ToolUse => DoneReason::ToolUse,
                        _ => DoneReason::Stop,
                    };
                    producer
                        .push(AssistantMessageEvent::Done {
                            reason,
                            message: out,
                        })
                        .await;
                }
                Err(error) => {
                    let aborted = opts.signal.is_cancelled();
                    out.stop_reason = if aborted {
                        StopReason::Aborted
                    } else {
                        StopReason::Error
                    };
                    out.error_message = Some(error);
                    producer
                        .push(AssistantMessageEvent::Error {
                            reason: if aborted {
                                ErrorReason::Aborted
                            } else {
                                ErrorReason::Error
                            },
                            error: out,
                        })
                        .await;
                }
            }
        });
        stream
    }
}
struct Prepared {
    url: reqwest::Url,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}
fn prepare(
    model: &Model,
    ctx: &Context,
    opts: &SimpleStreamOptions,
    provider_key: Option<&str>,
) -> Result<Prepared, String> {
    let mut headers = BTreeMap::new();
    for source in [model.headers.as_ref(), opts.headers.as_ref()]
        .into_iter()
        .flatten()
    {
        for (key, value) in source {
            headers.insert(key.to_ascii_lowercase(), value.clone());
        }
    }
    let region = headers
        .remove("x-omp-aws-region")
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| region(&model.id));
    let access = headers.remove("x-omp-aws-access-key-id");
    let secret = headers.remove("x-omp-aws-secret-access-key");
    let token = headers.remove("x-omp-aws-session-token");
    let bearer = opts
        .api_key
        .as_deref()
        .or(provider_key)
        .filter(|v| !v.is_empty())
        .map(|v| format!("Bearer {v}"))
        .or_else(|| headers.remove("authorization"));
    headers.retain(|name, _| {
        !name.starts_with("x-omp-")
            && !matches!(
                name.as_str(),
                "host"
                    | "authorization"
                    | "content-type"
                    | "accept"
                    | "content-length"
                    | "x-amz-date"
                    | "x-amz-content-sha256"
                    | "x-amz-security-token"
            )
    });
    headers.insert("content-type".into(), "application/json".into());
    headers.insert("accept".into(), "application/vnd.amazon.eventstream".into());
    crate::http::ensure_user_agent(&mut headers);
    let default_base = format!("https://bedrock-runtime.{region}.amazonaws.com");
    let mut url = reqwest::Url::parse(if model.base_url.is_empty() {
        &default_base
    } else {
        &model.base_url
    })
    .map_err(|e| e.to_string())?;
    if url.host_str().is_some_and(|host| {
        host.strip_prefix("bedrock-runtime.")
            .and_then(|h| h.strip_suffix(".amazonaws.com"))
            .is_some_and(|r| {
                !r.is_empty()
                    && r.bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            })
    }) {
        url.set_host(Some(&format!("bedrock-runtime.{region}.amazonaws.com")))
            .map_err(|e| e.to_string())?;
    }
    let path = format!(
        "{}/model/{}/converse-stream",
        url.path().trim_end_matches('/'),
        auth::encode(&model.id)
    );
    url.set_path(&path);
    let body = serde_json::to_vec(&request::build(model, ctx, opts)?).map_err(|e| e.to_string())?;
    if let Some(bearer) = bearer {
        headers.insert("authorization".into(), bearer);
    } else {
        auth::sign(
            &url,
            &body,
            &region,
            access
                .as_deref()
                .filter(|v| !v.is_empty())
                .ok_or("Missing AWS access key ID or Bedrock bearer token")?,
            secret
                .as_deref()
                .filter(|v| !v.is_empty())
                .ok_or("Missing AWS secret access key")?,
            token.as_deref(),
            &mut headers,
            time::OffsetDateTime::now_utc(),
        )?;
    }
    Ok(Prepared { url, headers, body })
}
fn region(model: &str) -> String {
    let arn: Vec<_> = model.splitn(6, ':').collect();
    if arn.len() >= 4 && arn[0] == "arn" && arn[2] == "bedrock" && !arn[3].is_empty() {
        return arn[3].into();
    }
    match model.split('.').next().unwrap_or_default() {
        "eu" => "eu-west-1",
        "apac" => "ap-southeast-1",
        "au" => "ap-southeast-2",
        "jp" => "ap-northeast-1",
        "us-gov" => "us-gov-west-1",
        _ => "us-east-1",
    }
    .into()
}
async fn run(
    producer: &mut AssistantMessageEventStreamProducer,
    out: &mut AssistantMessage,
    model: &Model,
    opts: &SimpleStreamOptions,
    http: reqwest::Client,
    request: Prepared,
) -> Result<(), String> {
    let mut builder = http.post(request.url).body(request.body);
    for (key, value) in request.headers {
        builder = builder.header(key, value);
    }
    let response = builder.send().await.map_err(|e| e.to_string())?;
    if !response.status().is_success() {
        let status = response.status();
        return Err(format!(
            "Bedrock HTTP {status}: {}",
            response.text().await.map_err(|e| e.to_string())?
        ));
    }
    out.response_id = response
        .headers()
        .get("x-amzn-requestid")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let mut bytes = response.bytes_stream();
    let mut decoder = eventstream::Decoder::default();
    let mut state = stream::State::default();
    let idle = opts
        .stream_idle_timeout
        .unwrap_or(crate::DEFAULT_STREAM_IDLE_TIMEOUT);
    loop {
        let chunk = if idle.is_zero() {
            bytes.next().await
        } else {
            tokio::time::timeout(idle, bytes.next())
                .await
                .map_err(|_| "Bedrock stream idle timeout".to_owned())?
        };
        let Some(chunk) = chunk else {
            break;
        };
        decoder.push(&chunk.map_err(|e| e.to_string())?);
        while let Some(frame) = decoder.next()? {
            let message_type = frame
                .headers
                .get(":message-type")
                .map(String::as_str)
                .unwrap_or("event");
            if matches!(message_type, "exception" | "error") {
                let code = frame
                    .headers
                    .get(":exception-type")
                    .or_else(|| frame.headers.get(":error-code"))
                    .map(String::as_str)
                    .unwrap_or("BedrockStreamException");
                let payload: serde_json::Value =
                    serde_json::from_slice(&frame.payload).unwrap_or_default();
                let message = frame
                    .headers
                    .get(":error-message")
                    .map(String::as_str)
                    .or_else(|| payload["message"].as_str())
                    .map(str::to_owned)
                    .unwrap_or_else(|| String::from_utf8_lossy(&frame.payload).into_owned());
                return Err(format!("{code}: {message}"));
            }
            if message_type != "event" {
                continue;
            }
            let kind = frame
                .headers
                .get(":event-type")
                .map(String::as_str)
                .unwrap_or_default();
            let payload = serde_json::from_slice(&frame.payload)
                .map_err(|e| format!("Invalid Bedrock event JSON: {e}"))?;
            for event in state.apply(kind, &payload, out, model)? {
                if !producer.push(event).await {
                    return Err("Request aborted".into());
                }
            }
            producer.commit_metadata(out);
        }
    }
    decoder.finish()?;
    state.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preserves_gateway_path_query_and_encodes_model_once() {
        let mut model = Model::new(
            "arn:aws:bedrock:eu-west-1:123:model/a",
            "a",
            Api::BedrockConverseStream,
            "amazon-bedrock",
            "https://gateway.test/prefix?team=a",
        );
        model.headers = Some(BTreeMap::from([(
            "X-OMP-AWS-Secret-Access-Key".into(),
            "secret".into(),
        )]));
        let opts = SimpleStreamOptions {
            api_key: Some("bearer".into()),
            ..Default::default()
        };
        let req = prepare(&model, &Context::default(), &opts, None).unwrap();
        assert_eq!(req.url.as_str(), "https://gateway.test/prefix/model/arn%3Aaws%3Abedrock%3Aeu-west-1%3A123%3Amodel%2Fa/converse-stream?team=a");
        assert_eq!(req.headers["authorization"], "Bearer bearer");
        assert!(!req.headers.keys().any(|k| k.starts_with("x-omp-")));
    }
    #[test]
    fn routes_geo_profiles_and_arn_regions() {
        assert_eq!(region("eu.anthropic.claude"), "eu-west-1");
        assert_eq!(
            region("arn:aws:bedrock:ap-south-1:123:model/a"),
            "ap-south-1"
        );
    }
}
