//! Real HTTP regressions for request controls and SSE handling.
#![cfg(feature = "providers")]

use std::{sync::Arc, time::Duration};

use rpi_ai::{
    providers::{
        anthropic::AnthropicProvider, openai_completions::OpenAiCompletionsProvider,
        openai_responses::OpenAiResponsesProvider,
    },
    Api, Content, Context, Model, Provider, SimpleStreamOptions, StopReason,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn read_request(socket: &mut tokio::net::TcpStream) -> String {
    let mut request = Vec::new();
    loop {
        let mut chunk = [0; 4096];
        let count = socket.read(&mut chunk).await.unwrap();
        assert_ne!(count, 0);
        request.extend_from_slice(&chunk[..count]);
        if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
            let length: usize = String::from_utf8_lossy(&request[..end])
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse().unwrap())
                })
                .unwrap();
            if request.len() >= end + 4 + length {
                return String::from_utf8(request[..end].to_vec()).unwrap();
            }
        }
    }
}

#[tokio::test]
async fn openai_request_headers_override_model_defaults_regardless_of_case() {
    for api in [Api::OpenaiCompletions, Api::OpenaiResponses] {
        for resolved_key in [None, Some("ResolvedToken")] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let headers = read_request(&mut socket).await;
                socket
                .write_all(
                    b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
                headers
            });
            let mut model = Model::new(
                "fixture",
                "Fixture",
                api.clone(),
                "fixture",
                format!("http://{address}"),
            );
            model.headers = Some(std::collections::BTreeMap::from([
                ("X-Tenant".into(), "model-default".into()),
                ("Authorization".into(), "Bearer ModelToken".into()),
                ("X-Model-Only".into(), "retained".into()),
            ]));
            let client = reqwest::Client::builder().no_proxy().build().unwrap();
            let provider: Arc<dyn Provider> = match api {
                Api::OpenaiCompletions => {
                    Arc::new(OpenAiCompletionsProvider::with_models_without_env_api_key(
                        "fixture",
                        None,
                        client,
                        vec![model.clone()],
                    ))
                }
                Api::OpenaiResponses => {
                    Arc::new(OpenAiResponsesProvider::with_models_without_env_api_key(
                        "fixture",
                        None,
                        client,
                        vec![model.clone()],
                    ))
                }
                _ => unreachable!(),
            };
            let options = SimpleStreamOptions {
                api_key: resolved_key.map(str::to_owned),
                headers: Some(std::collections::BTreeMap::from([
                    ("x-tenant".into(), "request-wins".into()),
                    ("aUtHoRiZaTiOn".into(), "Bearer RequestToken".into()),
                ])),
                ..Default::default()
            };
            let stream = provider
                .stream_simple(&model, &Context::new(vec![]), &options)
                .await;
            let message = tokio::time::timeout(Duration::from_secs(2), stream.result())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(message.stop_reason, StopReason::Error);
            let headers = server.await.unwrap();
            for (name, expected) in [
                ("x-tenant", "request-wins"),
                (
                    "authorization",
                    if resolved_key.is_some() {
                        "Bearer ResolvedToken"
                    } else {
                        "Bearer RequestToken"
                    },
                ),
                ("x-model-only", "retained"),
            ] {
                let values: Vec<_> = headers
                    .lines()
                    .filter_map(|line| {
                        let (key, value) = line.split_once(':')?;
                        key.eq_ignore_ascii_case(name).then(|| value.trim())
                    })
                    .collect();
                assert_eq!(values, [expected], "{api:?}: {name}");
            }
        }
    }
}

#[tokio::test]
async fn every_http_provider_cancels_while_reading_a_stalled_error_body() {
    for api in [
        Api::OpenaiCompletions,
        Api::OpenaiResponses,
        Api::AnthropicMessages,
    ] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            read_request(&mut socket).await;
            // Keep a partial diagnostic body open beyond cancellation.
            socket.write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Type: application/json\r\nContent-Length: 4096\r\n\r\n{\"error\":").await.unwrap();
            ready_tx.send(()).unwrap();
            let _ = release_rx.await;
        });
        let model = Model::new(
            "fixture",
            "Fixture",
            api.clone(),
            "fixture",
            format!("http://{address}"),
        );
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let key = Some("fixture-only".to_string());
        let models = vec![model.clone()];
        let provider: Arc<dyn Provider> = match api {
            Api::OpenaiCompletions => {
                Arc::new(OpenAiCompletionsProvider::with_models_without_env_api_key(
                    "fixture", key, client, models,
                ))
            }
            Api::OpenaiResponses => {
                Arc::new(OpenAiResponsesProvider::with_models_without_env_api_key(
                    "fixture", key, client, models,
                ))
            }
            Api::AnthropicMessages => Arc::new(AnthropicProvider::with_models_without_env_api_key(
                key, client, models,
            )),
            _ => unreachable!(),
        };
        let options = SimpleStreamOptions {
            timeout: Some(Duration::from_secs(3)),
            max_retries: Some(1),
            ..Default::default()
        };
        let stream = provider
            .stream_simple(&model, &Context::new(vec![]), &options)
            .await;
        tokio::time::timeout(Duration::from_secs(2), ready_rx)
            .await
            .unwrap()
            .unwrap();
        // Give the provider time to enter body collection after the headers.
        tokio::time::sleep(Duration::from_millis(100)).await;
        options.signal.cancel();
        let outcome = tokio::time::timeout(Duration::from_millis(500), stream.result()).await;
        let _ = release_tx.send(());
        server.await.unwrap();
        let message = outcome
            .expect("cancellation must not wait for the total request deadline")
            .unwrap();
        assert_eq!(
            message.stop_reason,
            StopReason::Aborted,
            "{api:?}: {message:?}"
        );
    }
}

#[tokio::test]
async fn completions_ignores_named_and_empty_keepalives_before_json_events() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        let body = concat!(
            "event: ping\r\n\r\n",
            "data: \r\n\r\n",
            "event: ping\ndata:    \n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"alive\"},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n"
        );
        socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
    });
    let model = Model::new(
        "fixture",
        "Fixture",
        Api::OpenaiCompletions,
        "fixture",
        format!("http://{address}"),
    );
    let provider = OpenAiCompletionsProvider::with_models_without_env_api_key(
        "fixture",
        Some("fixture-only".into()),
        reqwest::Client::builder().no_proxy().build().unwrap(),
        vec![model.clone()],
    );
    let stream = provider
        .stream_simple(
            &model,
            &Context::new(vec![]),
            &SimpleStreamOptions::default(),
        )
        .await;
    let result = tokio::time::timeout(Duration::from_secs(2), stream.result())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.stop_reason, StopReason::Stop, "{result:?}");
    assert_eq!(Content::text_only(&result.content, ""), "alive");
    server.await.unwrap();
}
