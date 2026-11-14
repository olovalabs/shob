use rpi_ai::{Context, SimpleStreamOptions, StopReason};
use rpi_runtime::{ModelRequest, ProviderRegistry, RuntimeConfig};
use std::collections::BTreeMap;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct Server {
    address: std::net::SocketAddr,
    hits: Arc<AtomicUsize>,
    requests: Arc<Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Server {
    async fn new() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let observed = hits.clone();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        let task = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                observed.fetch_add(1, Ordering::SeqCst);
                let mut request = Vec::new();
                let mut buffer = [0; 8192];
                loop {
                    let count = socket.read(&mut buffer).await.unwrap();
                    if count == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..count]);
                    if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                        let length: usize = headers
                            .lines()
                            .find_map(|line| line.strip_prefix("content-length:"))
                            .map_or(0, |value| value.trim().parse().unwrap());
                        if request.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                recorded
                    .lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&request).to_ascii_lowercase());
                let body = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
                let response = format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len());
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });
        Self {
            address,
            hits,
            requests,
            task,
        }
    }
}

async fn route_mixed_endpoints(direct_first: bool) {
    let direct = Server::new().await;
    let proxy = Server::new().await;
    let direct_model =
        serde_json::json!({"id":"direct", "baseUrl":format!("http://{}/v1", direct.address)});
    let proxy_model = serde_json::json!({"id":"proxied", "baseUrl":"http://proxy-only.invalid/v1"});
    let models = if direct_first {
        vec![direct_model, proxy_model]
    } else {
        vec![proxy_model, direct_model]
    };
    let config = RuntimeConfig {
        models: serde_json::from_value(serde_json::json!({"providers":{
            "account":{"api":"openai-completions", "apiKey":"fixture", "models":models}
        }}))
        .unwrap(),
        ..Default::default()
    };
    let values = BTreeMap::from([
        (
            "HTTP_PROXY".into(),
            format!("http://user:pass@{}", proxy.address),
        ),
        ("NO_PROXY".into(), "127.0.0.1".into()),
    ]);
    let result = config
        .resolve(
            &ModelRequest {
                provider: Some("account"),
                model: Some("direct"),
                ..Default::default()
            },
            &ProviderRegistry::default(),
            &values,
        )
        .unwrap();
    for model in result.provider.models() {
        let message = tokio::time::timeout(Duration::from_secs(3), async {
            result
                .provider
                .stream_simple(
                    model,
                    &Context::new(vec![]),
                    &SimpleStreamOptions {
                        timeout: Some(Duration::from_secs(1)),
                        ..Default::default()
                    },
                )
                .await
                .result()
                .await
        })
        .await
        .expect("request must settle")
        .unwrap();
        assert_eq!(
            message.stop_reason,
            StopReason::Stop,
            "{}: {:?}",
            model.id,
            message.error_message
        );
    }
    assert_eq!(
        direct.hits.load(Ordering::SeqCst),
        1,
        "NO_PROXY must apply to each URL"
    );
    assert_eq!(
        proxy.hits.load(Ordering::SeqCst),
        1,
        "proxy must apply to each URL"
    );
    assert!(proxy.requests.lock().unwrap()[0].contains("proxy-authorization: basic dxnlcjpwyxnz"));
    assert!(!direct.requests.lock().unwrap()[0].contains("proxy-authorization:"));
}

#[test]
fn invalid_proxy_is_rejected_without_exposing_its_credentials() {
    let config = RuntimeConfig {models:serde_json::from_value(serde_json::json!({"providers":{
        "account":{"api":"openai-completions", "apiKey":"fixture", "models":[{"id":"model", "baseUrl":"http://direct.invalid/v1"}]}
    }})).unwrap(),..Default::default()};
    let selection = ModelRequest {
        provider: Some("account"),
        model: Some("model"),
        ..Default::default()
    };
    for bypass in ["", "direct.invalid"] {
        let environment = BTreeMap::from([
            (
                "HTTP_PROXY".into(),
                "socks5://private-user:private-password@proxy.invalid:1080".into(),
            ),
            ("NO_PROXY".into(), bypass.into()),
        ]);
        let result = config.resolve(&selection, &ProviderRegistry::default(), &environment);
        let error = result
            .expect_err("invalid proxy must not fall back to direct access")
            .to_string();
        assert!(!error.contains("private-user") && !error.contains("private-password"));
    }
    // A wildcard is an explicit decision to connect directly for every URL,
    // including redirects; an unused proxy must not override that policy.
    let bypass_all = BTreeMap::from([
        ("HTTP_PROXY".into(), "socks5://unused.invalid:1080".into()),
        ("NO_PROXY".into(), "*, localhost".into()),
    ]);
    assert!(config
        .resolve(&selection, &ProviderRegistry::default(), &bypass_all)
        .is_ok());
}

#[tokio::test]
async fn first_direct_model_does_not_disable_proxy_for_other_models() {
    route_mixed_endpoints(true).await;
}

#[tokio::test]
async fn first_proxied_model_does_not_bypass_no_proxy_for_other_models() {
    route_mixed_endpoints(false).await;
}
