use serde::{Deserialize, Serialize};
use std::process::Command;
use std::time::{Duration, Instant};

/// The initial response from an RFC 8628 device authorization endpoint.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeviceAuthResponse {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    #[serde(default)]
    pub verification_uri_complete: Option<String>,
    #[serde(default = "default_expires_in")]
    pub expires_in: u64,
    #[serde(default = "default_interval")]
    pub interval: u64,
}

fn default_expires_in() -> u64 {
    900
}

fn default_interval() -> u64 {
    5
}

/// Token response on successful device code authorization.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeviceTokenResponse {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub token_type: Option<String>,
    #[serde(default)]
    pub expires_in: Option<u64>,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub error_description: Option<String>,
}

/// Request a new device code from the provider's device authorization endpoint.
pub fn initiate_device_flow(
    endpoint: &str,
    client_id: &str,
    scope: Option<&str>,
    extra_headers: &[(&str, &str)],
) -> Result<DeviceAuthResponse, String> {
    let mut cmd = Command::new("curl");
    cmd.args(["-s", "-X", "POST", endpoint])
        .args(["-H", "Accept: application/json"])
        .args(["-H", "Content-Type: application/x-www-form-urlencoded"]);

    for (k, v) in extra_headers {
        cmd.args(["-H", &format!("{k}: {v}")]);
    }

    let mut body = format!("client_id={client_id}");
    if let Some(scope) = scope {
        body.push_str(&format!("&scope={scope}"));
    }
    cmd.args(["-d", &body]);

    let output = cmd
        .output()
        .map_err(|e| format!("Failed to execute curl for device code request: {e}"))?;

    if !output.status.success() {
        return Err(format!(
            "Device authorization request failed (status {}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    let response: DeviceAuthResponse = serde_json::from_slice(&output.stdout).map_err(|e| {
        format!(
            "Invalid device authorization response: {e}. Output: {}",
            String::from_utf8_lossy(&output.stdout)
        )
    })?;

    Ok(response)
}

/// Poll the token endpoint until the user authorizes the device, or the code expires.
pub fn poll_device_token(
    token_url: &str,
    client_id: &str,
    device_code: &str,
    initial_interval: u64,
    expires_in: u64,
    extra_headers: &[(&str, &str)],
    on_progress: impl Fn(&str),
) -> Result<DeviceTokenResponse, String> {
    let mut interval = Duration::from_secs(std::cmp::max(1, initial_interval));
    let deadline = Instant::now() + Duration::from_secs(expires_in);

    while Instant::now() < deadline {
        std::thread::sleep(interval);

        let mut cmd = Command::new("curl");
        cmd.args(["-s", "-X", "POST", token_url])
            .args(["-H", "Accept: application/json"])
            .args(["-H", "Content-Type: application/x-www-form-urlencoded"]);

        for (k, v) in extra_headers {
            cmd.args(["-H", &format!("{k}: {v}")]);
        }

        let body = format!(
            "client_id={client_id}&device_code={device_code}&grant_type=urn:ietf:params:oauth:grant-type:device_code"
        );
        cmd.args(["-d", &body]);

        let output = match cmd.output() {
            Ok(out) => out,
            Err(e) => {
                on_progress(&format!("Network error polling token: {e}"));
                continue;
            }
        };

        if let Ok(res) = serde_json::from_slice::<DeviceTokenResponse>(&output.stdout) {
            if let Some(err) = &res.error {
                match err.as_str() {
                    "authorization_pending" => {
                        on_progress("Waiting for user authorization in browser...");
                        continue;
                    }
                    "slow_down" => {
                        interval += Duration::from_secs(5);
                        on_progress("Polling throttled by provider (slow_down); backing off...");
                        continue;
                    }
                    "expired_token" => {
                        return Err("Device code expired. Please restart login.".into());
                    }
                    "access_denied" => {
                        return Err("Access was denied by user.".into());
                    }
                    other => {
                        let desc = res.error_description.as_deref().unwrap_or(other);
                        return Err(format!("Device login failed: {desc}"));
                    }
                }
            }

            if !res.access_token.is_empty() {
                return Ok(res);
            }
        }
    }

    Err("Device flow timed out waiting for authorization".into())
}
