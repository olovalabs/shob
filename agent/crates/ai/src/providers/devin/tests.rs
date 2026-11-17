use super::*;
use serde_json::json;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

async fn request(socket: &mut tokio::net::TcpStream) -> (String, Vec<u8>) {
    let mut bytes = Vec::new();
    let end = loop {
        let mut chunk = [0; 4096];
        let n = socket.read(&mut chunk).await.unwrap();
        assert!(n > 0, "request ended before HTTP headers");
        bytes.extend_from_slice(&chunk[..n]);
        if let Some(end) = bytes.windows(4).position(|b| b == b"\r\n\r\n") {
            break end + 4;
        }
    };
    let headers = String::from_utf8(bytes[..end].to_vec()).unwrap();
    let size: usize = headers
        .lines()
        .find_map(|line| {
            line.split_once(':')
                .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                .map(|(_, value)| value.trim().parse().unwrap())
        })
        .unwrap();
    while bytes.len() - end < size {
        let mut chunk = [0; 4096];
        let n = socket.read(&mut chunk).await.unwrap();
        assert!(n > 0, "request ended before HTTP body");
        bytes.extend_from_slice(&chunk[..n]);
    }
    (headers, bytes[end..end + size].to_vec())
}
async fn respond(socket: &mut tokio::net::TcpStream, body: &[u8], streaming: bool) {
    let content_type = if streaming {
        "application/connect+proto"
    } else {
        "application/proto"
    };
    socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len()).as_bytes()).await.unwrap();
    // Fragment every envelope boundary to exercise incremental frame decoding.
    for fragment in body.chunks(3) {
        socket.write_all(fragment).await.unwrap();
    }
    socket.shutdown().await.unwrap();
}
#[tokio::test]
async fn cascade_auth_router_and_compressed_tool_stream_use_native_rpcs() {
    use std::io::Write;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let custom = format!("{base}/account");
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let (headers, body) = request(&mut socket).await;
        assert!(headers.starts_with(&format!("POST {AUTH_PATH} ")));
        let root = fields(&body).unwrap();
        let metadata = fields(data(&root, 1)).unwrap();
        assert_eq!(
            text(&metadata, 3).unwrap(),
            "devin-session-token$header.payload.signature"
        );
        assert_eq!(text(&metadata, 28).unwrap(), "chisel");
        respond(
            &mut socket,
            &Proto::default().bytes(1, "user-jwt").bytes(2, custom).0,
            false,
        )
        .await;

        let (mut socket, _) = listener.accept().await.unwrap();
        let (headers, body) = request(&mut socket).await;
        assert!(headers.starts_with(&format!("POST /account{ASSIGN_PATH} ")));
        let root = fields(&body).unwrap();
        assert_eq!(text(&root, 2).unwrap(), "adaptive");
        assert_eq!(text(&root, 3).unwrap(), "cascade");
        let prompt = fields(data(&root, 5)).unwrap();
        assert_eq!(text(&prompt, 3).unwrap(), "Find it");
        respond(
            &mut socket,
            &Proto::default()
                .msg(
                    1,
                    Proto::default()
                        .bytes(1, "assignment-jwt")
                        .bytes(2, "actual-model"),
                )
                .0,
            false,
        )
        .await;

        let (mut socket, _) = listener.accept().await.unwrap();
        let (headers, body) = request(&mut socket).await;
        assert!(headers.starts_with(&format!("POST /account{CHAT_PATH} ")));
        assert!(headers
            .to_ascii_lowercase()
            .contains("content-type: application/connect+proto"));
        let mut decoder = wire::Decoder::default();
        decoder.push(&body);
        let (_, body) = decoder.next().unwrap().unwrap();
        decoder.finish().unwrap();
        let root = fields(&body).unwrap();
        assert_eq!(text(&root, 21).unwrap(), "actual-model");
        assert_eq!(text(&root, 26).unwrap(), "assignment-jwt");
        assert_eq!(text(&root, 16).unwrap(), "cascade");
        assert_eq!(number(&root, 7), 5);
        let metadata = fields(data(&root, 1)).unwrap();
        assert_eq!(text(&metadata, 21).unwrap(), "user-jwt");
        let tool = fields(data(&root, 10)).unwrap();
        assert_eq!(text(&tool, 1).unwrap(), "lookup");
        let first = Proto::default()
            .bytes(1, "response-id")
            .bytes(9, "reason")
            .bytes(10, "signature")
            .bytes(3, "Searching")
            .msg(
                6,
                Proto::default()
                    .bytes(1, "call")
                    .bytes(2, "lookup")
                    .bytes(3, "{\"query\":"),
            );
        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gzip.write_all(&first.0).unwrap();
        let mut response = wire::frame(&gzip.finish().unwrap());
        response[0] = 1;
        response.extend(wire::frame(
            &Proto::default()
                .msg(6, Proto::default().bytes(3, "{\"query\":\"needle\"}"))
                .int(5, 10)
                .msg(
                    7,
                    Proto::default()
                        .int(2, 100)
                        .int(3, 20)
                        .int(4, 10)
                        .int(5, 30),
                )
                .0,
        ));
        let mut trailer = wire::frame(b"{}");
        trailer[0] = 2;
        response.extend(trailer);
        respond(&mut socket, &response, true).await;
    });
    let mut model = Model::new(
        "adaptive",
        "Adaptive",
        crate::Api::Other(API.into()),
        "devin",
        base,
    );
    model.sampling_params = Some(
        serde_json::from_value(
            json!({"compatConfig":{"modelRouter":true,"supportsParallelToolCalls":true}}),
        )
        .unwrap(),
    );
    let context = Context {
        system_prompt: Some("System".into()),
        messages: vec![serde_json::from_value(
            json!({"role":"user","content":"Find it","timestamp":0}),
        )
        .unwrap()],
        tools: vec![crate::Tool {
            name: "lookup".into(),
            description: "Lookup".into(),
            parameters: serde_json::from_value(
                json!({"type":"object","properties":{"query":{"type":"string"}}}),
            )
            .unwrap(),
            constrained_sampling: None,
        }],
    };
    let provider = DevinProvider::with_models_without_env_api_key(
        "devin",
        None,
        reqwest::Client::new(),
        vec![model.clone()],
    );
    let options = SimpleStreamOptions {
        api_key: Some(
            json!({"token":"header.payload.signature","accountId":"account"}).to_string(),
        ),
        session_id: Some("cascade".into()),
        ..Default::default()
    };
    let result = provider
        .stream_simple(&model, &context, &options)
        .await
        .result()
        .await
        .unwrap();
    server.await.unwrap();
    assert_eq!(result.stop_reason, StopReason::ToolUse);
    assert_eq!(result.response_id.as_deref(), Some("response-id"));
    assert_eq!(result.response_model.as_deref(), Some("actual-model"));
    assert!(
        matches!(&result.content[0],Content::Thinking(t) if t.thinking=="reason" && t.thinking_signature.as_deref()==Some("signature"))
    );
    assert!(matches!(&result.content[1],Content::Text(t) if t.text=="Searching"));
    assert!(
        matches!(&result.content[2],Content::ToolCall(t) if t.name=="lookup" && t.arguments==json!({"query":"needle"}))
    );
    assert_eq!(result.usage.input, 100);
    assert_eq!(result.usage.output, 20);
    assert_eq!(result.usage.total_tokens, 160);
}
