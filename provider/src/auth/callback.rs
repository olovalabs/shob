use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

/// Result of an OAuth callback redirect.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallbackPayload {
    pub code: Option<String>,
    pub state: Option<String>,
    pub error: Option<String>,
    pub error_description: Option<String>,
}

/// A lightweight local loopback server for capturing OAuth 2.0 authorization callbacks.
pub struct CallbackServer {
    listener: TcpListener,
    port: u16,
}

impl CallbackServer {
    /// Bind to a preferred port on localhost, or fall back to an ephemeral port if unavailable.
    pub fn bind(preferred_port: Option<u16>) -> Result<Self, String> {
        if let Some(port) = preferred_port {
            if let Ok(listener) = TcpListener::bind(("127.0.0.1", port)) {
                return Ok(Self { listener, port });
            }
        }
        // Fallback to ephemeral port on 127.0.0.1
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .map_err(|e| format!("Failed to bind local OAuth callback listener: {e}"))?;
        let port = listener
            .local_addr()
            .map_err(|e| format!("Failed to get local address: {e}"))?
            .port();
        Ok(Self { listener, port })
    }

    /// The port this server is listening on.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The loopback redirect URI for this server.
    pub fn redirect_uri(&self, path: &str) -> String {
        let path = if path.starts_with('/') {
            path
        } else {
            &format!("/{path}")
        };
        format!("http://localhost:{}{path}", self.port)
    }

    /// Wait for a single HTTP GET request with a timeout, parse query parameters, send response, and return the payload.
    pub fn wait_for_callback(&self, timeout: Duration) -> Result<CallbackPayload, String> {
        self.listener
            .set_nonblocking(true)
            .map_err(|e| format!("Could not set nonblocking listener: {e}"))?;

        let start = std::time::Instant::now();
        loop {
            if start.elapsed() > timeout {
                return Err("Timed out waiting for OAuth authorization in browser".into());
            }

            match self.listener.accept() {
                Ok((mut stream, _)) => {
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
                    return self.handle_connection(&mut stream);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                }
                Err(e) => return Err(format!("Error accepting OAuth callback: {e}")),
            }
        }
    }

    fn handle_connection(&self, stream: &mut TcpStream) -> Result<CallbackPayload, String> {
        let mut buffer = [0u8; 4096];
        let bytes_read = stream
            .read(&mut buffer)
            .map_err(|e| format!("Failed to read OAuth request: {e}"))?;
        let request = String::from_utf8_lossy(&buffer[..bytes_read]);

        // Parse GET request line: GET /callback?code=...&state=... HTTP/1.1
        let first_line = request.lines().next().unwrap_or_default();
        let mut parts = first_line.split_whitespace();
        let method = parts.next().unwrap_or_default();
        let target = parts.next().unwrap_or_default();

        if method != "GET" {
            let response = "HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\n\r\n";
            let _ = stream.write_all(response.as_bytes());
            return Err("Expected GET request for OAuth callback".into());
        }

        let query = target.split_once('?').map(|(_, q)| q).unwrap_or_default();
        let mut payload = CallbackPayload {
            code: None,
            state: None,
            error: None,
            error_description: None,
        };

        for param in query.split('&') {
            if let Some((key, val)) = param.split_once('=') {
                let decoded_val = urlencoding_decode(val);
                match key {
                    "code" => payload.code = Some(decoded_val),
                    "state" => payload.state = Some(decoded_val),
                    "error" => payload.error = Some(decoded_val),
                    "error_description" => payload.error_description = Some(decoded_val),
                    _ => {}
                }
            }
        }

        // Send a friendly styled HTML response
        let success = payload.code.is_some() && payload.error.is_none();
        let html = if success { SUCCESS_HTML } else { ERROR_HTML };

        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            html.len(),
            html
        );
        let _ = stream.write_all(response.as_bytes());
        let _ = stream.flush();

        Ok(payload)
    }
}

fn urlencoding_decode(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '%' {
            let h1 = chars.next();
            let h2 = chars.next();
            if let (Some(h1), Some(h2)) = (h1, h2) {
                if let Ok(byte) = u8::from_str_radix(&format!("{h1}{h2}"), 16) {
                    result.push(byte as char);
                    continue;
                }
            }
            result.push('%');
        } else if c == '+' {
            result.push(' ');
        } else {
            result.push(c);
        }
    }
    result
}

const SUCCESS_HTML: &str = r#"<!DOCTYPE html>
<html>
<head>
    <meta charset="utf-8">
    <title>Authentication Successful</title>
    <style>
        body {
            font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, Helvetica, Arial, sans-serif;
            display: flex;
            align-items: center;
            justify-content: center;
            height: 100vh;
            margin: 0;
            background: #111418;
            color: #f0f3f6;
        }
        .card {
            background: #1a1f26;
            border: 1px solid #2d3440;
            border-radius: 12px;
            padding: 36px 48px;
            text-align: center;
            max-width: 440px;
            box-shadow: 0 12px 32px rgba(0, 0, 0, 0.4);
        }
        .icon {
            font-size: 48px;
            margin-bottom: 16px;
            color: #38d39f;
        }
        h1 { font-size: 20px; font-weight: 600; margin: 0 0 10px 0; }
        p { color: #8b949e; font-size: 14px; margin: 0; }
    </style>
</head>
<body>
    <div class="card">
        <div class="icon">&#10003;</div>
        <h1>Authentication Successful</h1>
        <p>You can close this tab and return to Kathamo.</p>
    </div>
</body>
</html>"#;

const ERROR_HTML: &str = r#"<!DOCTYPE html>
<html>
<head>
    <meta charset="utf-8">
    <title>Authentication Failed</title>
    <style>
        body {
            font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, Helvetica, Arial, sans-serif;
            display: flex;
            align-items: center;
            justify-content: center;
            height: 100vh;
            margin: 0;
            background: #111418;
            color: #f0f3f6;
        }
        .card {
            background: #1a1f26;
            border: 1px solid #2d3440;
            border-radius: 12px;
            padding: 36px 48px;
            text-align: center;
            max-width: 440px;
        }
        .icon { font-size: 48px; margin-bottom: 16px; color: #f85149; }
        h1 { font-size: 20px; font-weight: 600; margin: 0 0 10px 0; }
        p { color: #8b949e; font-size: 14px; margin: 0; }
    </style>
</head>
<body>
    <div class="card">
        <div class="icon">&#10007;</div>
        <h1>Authentication Error</h1>
        <p>Login was not completed. Please check Kathamo for details.</p>
    </div>
</body>
</html>"#;
