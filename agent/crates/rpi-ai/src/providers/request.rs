//! Encode each provider request once, including retries. Buffered reqwest JSON
//! bodies use shared Bytes; cloning an attempt does not clone a JSON tree.
use std::sync::Arc;
use std::time::Instant;

#[derive(Clone)]
pub(super) struct PreparedJsonRequest(Arc<PreparedRequest>);

struct PreparedRequest {
    client: reqwest::Client,
    request: reqwest::Request,
}

#[derive(Default)]
pub(super) struct PreparedJsonRequestDiagnostics {
    pub(super) json_serialize_ns: u64,
    pub(super) request_build_ns: u64,
}

impl PreparedJsonRequestDiagnostics {
    fn elapsed(started: Instant) -> u64 {
        started.elapsed().as_nanos().min(u64::MAX as u128) as u64
    }
}

impl PreparedJsonRequest {
    pub(super) fn new<T: serde::Serialize + ?Sized>(
        builder: reqwest::RequestBuilder,
        body: &T,
    ) -> Result<Self, reqwest::Error> {
        Self::new_with_diagnostics(builder, body, None)
    }

    pub(super) fn new_with_diagnostics<T: serde::Serialize + ?Sized>(
        builder: reqwest::RequestBuilder,
        body: &T,
        mut diagnostics: Option<&mut PreparedJsonRequestDiagnostics>,
    ) -> Result<Self, reqwest::Error> {
        let builder = if let Some(diagnostics) = diagnostics.as_deref_mut() {
            let started = Instant::now();
            let builder = builder.json(body);
            diagnostics.json_serialize_ns = PreparedJsonRequestDiagnostics::elapsed(started);
            builder
        } else {
            builder.json(body)
        };
        let build_started = diagnostics.as_ref().map(|_| Instant::now());
        let (client, request) = builder.build_split();
        if let (Some(diagnostics), Some(started)) = (diagnostics, build_started) {
            diagnostics.request_build_ns = PreparedJsonRequestDiagnostics::elapsed(started);
        }
        Ok(Self(Arc::new(PreparedRequest {
            client,
            request: request?,
        })))
    }

    pub(super) fn new_body(
        builder: reqwest::RequestBuilder,
        body: Vec<u8>,
        add_json_content_type: bool,
    ) -> Result<Self, reqwest::Error> {
        let builder = if add_json_content_type {
            builder.header(reqwest::header::CONTENT_TYPE, "application/json")
        } else {
            builder
        };
        let (client, request) = builder.body(body).build_split();
        Ok(Self(Arc::new(PreparedRequest {
            client,
            request: request?,
        })))
    }

    pub(super) fn body_len(&self) -> Option<usize> {
        self.0.request.body()?.as_bytes().map(|body| body.len())
    }

    pub(super) async fn send(&self) -> Result<reqwest::Response, crate::AiError> {
        let request = self
            .0
            .request
            .try_clone()
            .ok_or_else(|| crate::AiError::Provider {
                code: "request_not_replayable".into(),
                message: "encoded JSON request is not replayable".into(),
            })?;
        self.0
            .client
            .execute(request)
            .await
            .map_err(|error| crate::AiError::Http {
                status: None,
                message: format!("http transport error: {error}"),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Counted<'a>(&'a AtomicUsize);
    impl serde::Serialize for Counted<'_> {
        fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            self.0.fetch_add(1, Ordering::SeqCst);
            serializer.serialize_str(&"history 🐈\n".repeat(4096))
        }
    }

    #[test]
    fn retry_clones_share_encoded_bytes_and_preserve_custom_headers() {
        let calls = AtomicUsize::new(0);
        let client = reqwest::Client::new();
        let prepared = PreparedJsonRequest::new(
            client
                .post("http://127.0.0.1/request")
                .header("Content-Type", "application/x-rpi-json")
                .header("x-test", "kept")
                .timeout(std::time::Duration::from_secs(7)),
            &Counted(&calls),
        )
        .unwrap();
        for _ in 0..8 {
            let request = prepared.0.request.try_clone().unwrap();
            let bytes = request.body().unwrap().as_bytes().unwrap();
            assert_eq!(
                bytes.as_ptr(),
                prepared
                    .0
                    .request
                    .body()
                    .unwrap()
                    .as_bytes()
                    .unwrap()
                    .as_ptr()
            );
            assert_eq!(
                serde_json::from_slice::<String>(bytes).unwrap(),
                "history 🐈\n".repeat(4096)
            );
            assert_eq!(request.headers()["content-type"], "application/x-rpi-json");
            assert_eq!(request.headers()["x-test"], "kept");
            assert_eq!(request.timeout(), Some(&std::time::Duration::from_secs(7)));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn preencoded_body_matches_json_request_headers_and_bytes() {
        let client = reqwest::Client::new();
        let body = r#"{"history":"hello 🐈"}"#.as_bytes().to_vec();
        let encoded = PreparedJsonRequest::new(
            client.post("http://127.0.0.1/request"),
            &serde_json::json!({"history": "hello 🐈"}),
        )
        .unwrap();
        let preencoded = PreparedJsonRequest::new_body(
            client.post("http://127.0.0.1/request"),
            body.clone(),
            true,
        )
        .unwrap();

        let custom = PreparedJsonRequest::new_body(
            client
                .post("http://127.0.0.1/request")
                .header("content-type", "application/x-rpi-json"),
            body.clone(),
            false,
        )
        .unwrap();

        let encoded_request = &encoded.0.request;
        let preencoded_request = &preencoded.0.request;
        assert_eq!(
            encoded_request.body().unwrap().as_bytes(),
            preencoded_request.body().unwrap().as_bytes()
        );
        assert_eq!(
            encoded_request.headers().get("content-type"),
            preencoded_request.headers().get("content-type")
        );
        assert_eq!(
            custom.0.request.headers().get("content-type").unwrap(),
            "application/x-rpi-json"
        );
    }

    #[test]
    fn invalid_url_or_headers_are_rejected_before_sending() {
        let client = reqwest::Client::new();
        assert!(PreparedJsonRequest::new(client.post("not a URL"), &()).is_err());
        assert!(PreparedJsonRequest::new(
            client
                .post("http://127.0.0.1")
                .header("x-test", "invalid\nvalue"),
            &()
        )
        .is_err());
    }
}
