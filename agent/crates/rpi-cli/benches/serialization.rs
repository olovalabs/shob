use criterion::{black_box, criterion_group, criterion_main, Criterion};
use rpi_plugin_sdk::binary::{self, BinaryOptions};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TypedPayload {
    request_id: String,
    tool_name: String,
    arguments: Value,
    metadata: PayloadMetadata,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PayloadMetadata {
    cwd: String,
    session_id: String,
    flags: Vec<String>,
}

fn sample_value() -> Value {
    json!({
        "type": "tool_execution_update",
        "toolCallId": "call_01HZZZZZZZZZZZZZZZZZZZZZZ",
        "toolName": "read",
        "args": {
            "path": "src/agent_loop.rs",
            "line_start": 1,
            "line_end": 420,
            "include_hidden": false,
        },
        "partialResult": {
            "content": [
                {"type": "text", "text": "A representative tool result with enough text to exercise the serializer."},
                {"type": "text", "text": "The binary path is intended for repeated extension and provider payloads."}
            ],
            "details": {
                "bytes": 16384,
                "lines": 420,
                "cached": false,
                "paths": ["src/agent_loop.rs", "src/events.rs", "Cargo.toml"]
            },
            "terminate": false
        }
    })
}

fn sample_typed() -> TypedPayload {
    TypedPayload {
        request_id: "req_01HZZZZZZZZZZZZZZZZZZZZZZ".into(),
        tool_name: "read".into(),
        arguments: json!({
            "path": "src/agent_loop.rs",
            "line_start": 1,
            "line_end": 420,
            "include_hidden": false,
        }),
        metadata: PayloadMetadata {
            cwd: "D:/Projects/pi-rust".into(),
            session_id: "01HZZZZZZZZZZZZZZZZZZZZZZ".into(),
            flags: vec!["binary".into(), "streaming".into(), "trusted".into()],
        },
    }
}

fn bench_serialization(c: &mut Criterion) {
    let options = BinaryOptions::default();
    let value = sample_value();
    let typed = sample_typed();
    let json_bytes = serde_json::to_vec(&value).unwrap();
    let typed_json_bytes = serde_json::to_vec(&typed).unwrap();
    let binary_bytes = binary::encode_json(&value, &options).unwrap();
    let typed_binary_bytes = binary::encode(&typed, &options).unwrap();

    eprintln!(
        "serialization payload sizes: json-value={} bytes, json-typed={} bytes, binary-json-value={} bytes, binary-typed={} bytes",
        json_bytes.len(),
        typed_json_bytes.len(),
        binary_bytes.len(),
        typed_binary_bytes.len()
    );

    let mut group = c.benchmark_group("serialization");
    group.bench_function("json_encode_value", |b| {
        b.iter(|| serde_json::to_vec(black_box(&value)).unwrap())
    });
    group.bench_function("json_encode_typed", |b| {
        b.iter(|| serde_json::to_vec(black_box(&typed)).unwrap())
    });
    group.bench_function("binary_encode_json_value", |b| {
        b.iter(|| binary::encode_json(black_box(&value), &options).unwrap())
    });
    group.bench_function("binary_encode_typed", |b| {
        b.iter(|| binary::encode(black_box(&typed), &options).unwrap())
    });
    group.bench_function("json_decode_value", |b| {
        b.iter(|| serde_json::from_slice::<Value>(black_box(&json_bytes)).unwrap())
    });
    group.bench_function("json_decode_typed", |b| {
        b.iter(|| serde_json::from_slice::<TypedPayload>(black_box(&typed_json_bytes)).unwrap())
    });
    group.bench_function("binary_decode_json_value", |b| {
        b.iter(|| binary::decode_json(black_box(&binary_bytes), &options).unwrap())
    });
    group.bench_function("binary_decode_typed", |b| {
        b.iter(|| binary::decode::<TypedPayload>(black_box(&typed_binary_bytes), &options).unwrap())
    });
    group.finish();
}

criterion_group!(benches, bench_serialization);
criterion_main!(benches);
