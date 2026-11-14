//! Native cloud credential chains. Discovery is local; token acquisition happens
//! only immediately before a cloud request. SDK providers cache and renew tokens.
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::LazyLock;
use std::time::Duration;

use aws_credential_types::provider::ProvideCredentials;
use google_cloud_auth::credentials::{CacheableResource, Credentials};
use tokio::sync::OnceCell;

use crate::ValueSource;

static AWS: OnceCell<aws_config::SdkConfig> = OnceCell::const_new();
static GOOGLE: OnceCell<Credentials> = OnceCell::const_new();

fn value(values: &dyn ValueSource, name: &str) -> Option<String> {
    values.env(name).filter(|value| !value.trim().is_empty())
}

fn home(values: &dyn ValueSource) -> Option<PathBuf> {
    value(values, "HOME")
        .or_else(|| value(values, "USERPROFILE"))
        .map(PathBuf::from)
}

fn adc_path(values: &dyn ValueSource) -> Option<PathBuf> {
    if let Some(path) = value(values, "GOOGLE_APPLICATION_CREDENTIALS") {
        return Some(path.into());
    }
    #[cfg(windows)]
    {
        value(values, "APPDATA")
            .map(|path| PathBuf::from(path).join("gcloud/application_default_credentials.json"))
    }
    #[cfg(not(windows))]
    {
        home(values).map(|path| path.join(".config/gcloud/application_default_credentials.json"))
    }
}

fn profile_section(text: &str, wanted: &str) -> BTreeMap<String, String> {
    let mut active = false;
    let mut result = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim().trim_start_matches('\u{feff}');
        if line.is_empty() || line.starts_with(['#', ';']) {
            continue;
        }
        if let Some(section) = line
            .strip_prefix('[')
            .and_then(|line| line.strip_suffix(']'))
        {
            active = section.trim() == wanted;
        } else if active {
            if let Some((key, value)) = line.split_once('=') {
                result.insert(key.trim().to_lowercase(), value.trim().to_string());
            }
        }
    }
    result
}

fn aws_profile_fields(values: &dyn ValueSource) -> BTreeMap<String, String> {
    let profile = value(values, "AWS_PROFILE")
        .or_else(|| value(values, "AWS_DEFAULT_PROFILE"))
        .unwrap_or_else(|| "default".into());
    let credentials = value(values, "AWS_SHARED_CREDENTIALS_FILE")
        .map(PathBuf::from)
        .or_else(|| home(values).map(|path| path.join(".aws/credentials")));
    let config = value(values, "AWS_CONFIG_FILE")
        .map(PathBuf::from)
        .or_else(|| home(values).map(|path| path.join(".aws/config")));
    let section = if profile == "default" {
        profile.clone()
    } else {
        format!("profile {profile}")
    };
    let mut fields = config
        .and_then(|path| std::fs::read_to_string(path).ok())
        .map(|text| profile_section(&text, &section))
        .unwrap_or_default();
    if let Some(text) = credentials.and_then(|path| std::fs::read_to_string(path).ok()) {
        fields.extend(profile_section(&text, &profile));
    }
    fields
}

pub(crate) fn aws_region(values: &dyn ValueSource) -> Option<String> {
    value(values, "AWS_REGION")
        .or_else(|| value(values, "AWS_DEFAULT_REGION"))
        .or_else(|| {
            aws_profile_fields(values)
                .remove("region")
                .filter(|region| !region.is_empty())
        })
}

fn aws_profile_configured(values: &dyn ValueSource) -> bool {
    let fields = aws_profile_fields(values);
    let has = |key: &str| fields.get(key).is_some_and(|value| !value.is_empty());
    (has("aws_access_key_id") && has("aws_secret_access_key"))
        || has("credential_process")
        || has("sso_session")
        || (has("sso_start_url") && has("sso_account_id") && has("sso_role_name"))
        || (has("role_arn")
            && (has("source_profile")
                || has("credential_source")
                || has("web_identity_token_file")))
}

fn cloud_host(needle: &str) -> bool {
    [
        "/sys/devices/virtual/dmi/id/sys_vendor",
        "/sys/devices/virtual/dmi/id/product_name",
        "/sys/hypervisor/uuid",
    ]
    .iter()
    .any(|path| {
        std::fs::read_to_string(path).is_ok_and(|text| text.to_lowercase().contains(needle))
    })
}

/// Locally configured cloud authentication, without probing a network or
/// exposing credentials. Profile readiness is not proof that SSO is logged in.
pub(crate) fn configured(provider: &str, values: &dyn ValueSource) -> Option<&'static str> {
    match provider {
        "amazon-bedrock" | "bedrock-mantle" => {
            static EC2: LazyLock<bool> =
                LazyLock::new(|| cloud_host("amazon ec2") || cloud_host("ec2"));
            let explicit = (value(values, "AWS_ACCESS_KEY_ID").is_some()
                && value(values, "AWS_SECRET_ACCESS_KEY").is_some())
                || value(values, "AWS_BEARER_TOKEN_BEDROCK").is_some()
                || (value(values, "AWS_WEB_IDENTITY_TOKEN_FILE").is_some()
                    && value(values, "AWS_ROLE_ARN").is_some())
                || value(values, "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI").is_some()
                || value(values, "AWS_CONTAINER_CREDENTIALS_FULL_URI").is_some();
            let instance = !value(values, "AWS_EC2_METADATA_DISABLED")
                .is_some_and(|value| value.eq_ignore_ascii_case("true"))
                && (value(values, "AWS_EC2_METADATA_SERVICE_ENDPOINT").is_some() || *EC2);
            (explicit || instance || aws_profile_configured(values))
                .then_some("AWS credential chain")
        }
        "google-vertex" => {
            static GCE: LazyLock<bool> = LazyLock::new(|| cloud_host("google"));
            let ready = value(values, "GOOGLE_OAUTH_ACCESS_TOKEN").is_some()
                || adc_path(values).is_some_and(|path| path.is_file())
                || [
                    "GCE_METADATA_HOST",
                    "GCE_METADATA_IP",
                    "K_SERVICE",
                    "GAE_ENV",
                ]
                .iter()
                .any(|name| value(values, name).is_some())
                || *GCE;
            ready.then_some("Google application default credentials")
        }
        _ => None,
    }
}

/// Secret request headers; callers must never add these to public descriptors,
/// diagnostics, or persisted transcripts. Protocol adapters strip private keys.
pub(crate) async fn request_headers(provider: &str) -> Result<BTreeMap<String, String>, String> {
    tokio::time::timeout(Duration::from_secs(30), async {
        match provider {
            "amazon-bedrock" | "bedrock-mantle" => aws_headers().await,
            "google-vertex" => google_headers().await,
            _ => Ok(BTreeMap::new()),
        }
    })
    .await
    .map_err(|_| {
        "Cloud credential acquisition timed out; check the selected cloud account and network."
            .to_string()
    })?
}

async fn aws_headers() -> Result<BTreeMap<String, String>, String> {
    if let Ok(token) = std::env::var("AWS_BEARER_TOKEN_BEDROCK") {
        if !token.trim().is_empty() {
            return Ok(BTreeMap::from([(
                "Authorization".into(),
                format!("Bearer {token}"),
            )]));
        }
    }
    let config = AWS
        .get_or_init(|| aws_config::defaults(aws_config::BehaviorVersion::latest()).load())
        .await;
    let credentials = config.credentials_provider()
        .ok_or("AWS credential provider is not configured")?
        .provide_credentials().await
        .map_err(|_| "AWS credentials unavailable; configure an AWS profile, renew `aws sso login`, or provide IAM credentials.".to_string())?;
    let mut headers = BTreeMap::from([
        (
            "x-omp-aws-access-key-id".into(),
            credentials.access_key_id().into(),
        ),
        (
            "x-omp-aws-secret-access-key".into(),
            credentials.secret_access_key().into(),
        ),
    ]);
    if let Some(token) = credentials.session_token() {
        headers.insert("x-omp-aws-session-token".into(), token.into());
    }
    if let Some(region) = config.region() {
        headers.insert("x-omp-aws-region".into(), region.as_ref().into());
    }
    Ok(headers)
}

struct Environment;
impl ValueSource for Environment {
    fn env(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }
}

async fn google_headers() -> Result<BTreeMap<String, String>, String> {
    let mut headers = if let Some(token) = value(&Environment, "GOOGLE_OAUTH_ACCESS_TOKEN") {
        BTreeMap::from([("authorization".into(), format!("Bearer {token}"))])
    } else {
        let credentials = GOOGLE.get_or_try_init(|| async {
            google_cloud_auth::credentials::Builder::default()
                .with_scopes(["https://www.googleapis.com/auth/cloud-platform"])
                .build().map_err(|_| "Google ADC could not be loaded; run `gcloud auth application-default login` or configure GOOGLE_APPLICATION_CREDENTIALS.".to_string())
        }).await?;
        let CacheableResource::New { data, .. } = credentials.headers(http::Extensions::new()).await
            .map_err(|_| "Google ADC could not obtain an access token; renew the account or check service-account permissions.".to_string())?
        else { return Err("Google ADC did not return request headers".into()); };
        data.iter()
            .map(|(key, value)| {
                value
                    .to_str()
                    .map(|value| (key.as_str().to_string(), value.to_string()))
            })
            .collect::<Result<BTreeMap<_, _>, _>>()
            .map_err(|_| "Google ADC returned invalid request headers".to_string())?
    };
    let project = [
        "GOOGLE_CLOUD_PROJECT",
        "GOOGLE_CLOUD_PROJECT_ID",
        "GCLOUD_PROJECT",
    ]
    .iter()
    .find_map(|name| value(&Environment, name))
    .or_else(|| {
        adc_path(&Environment)
            .and_then(|path| std::fs::read(path).ok())
            .and_then(|data| serde_json::from_slice::<serde_json::Value>(&data).ok())
            .and_then(|json| {
                json.get("project_id")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            })
    });
    let project = match project {
        Some(project) => project,
        None => {
            let host = value(&Environment, "GCE_METADATA_HOST")
                .unwrap_or_else(|| "metadata.google.internal".into());
            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(3))
                .build()
                .map_err(|_| "Could not initialize Google metadata client")?;
            client
                .get(format!(
                    "http://{host}/computeMetadata/v1/project/project-id"
                ))
                .header("Metadata-Flavor", "Google")
                .send()
                .await
                .ok()
                .filter(|response| response.status().is_success())
                .ok_or("Set GOOGLE_CLOUD_PROJECT to the Vertex AI resource project")?
                .text()
                .await
                .map_err(|_| "Could not read Google resource project")?
                .trim()
                .to_string()
        }
    };
    if project.is_empty() {
        return Err("Set GOOGLE_CLOUD_PROJECT to the Vertex AI resource project".into());
    }
    headers.insert("x-omp-google-project".into(), project);
    headers.insert(
        "x-goog-location".into(),
        value(&Environment, "GOOGLE_CLOUD_LOCATION").unwrap_or_else(|| "global".into()),
    );
    Ok(headers)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unrelated_or_partial_cloud_environment_is_not_authentication() {
        let values = BTreeMap::from([
            ("AWS_ACCESS_KEY_ID".into(), "partial".into()),
            ("GOOGLE_CLOUD_PROJECT".into(), "project-only".into()),
            ("HOME".into(), "/nonexistent-kathamo-cloud-home".into()),
            ("AWS_EC2_METADATA_DISABLED".into(), "true".into()),
        ]);
        assert_eq!(configured("amazon-bedrock", &values), None);
        assert_eq!(configured("openai", &values), None);
    }

    #[test]
    fn named_profile_does_not_accept_credentials_from_another_section() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials");
        std::fs::write(&path, "[default]\naws_access_key_id=other\naws_secret_access_key=other\n[chosen]\nrole_arn=arn:aws:iam::123:role/Test\nsource_profile=default\n").unwrap();
        let mut values = BTreeMap::from([
            ("AWS_PROFILE".into(), "missing".into()),
            (
                "AWS_SHARED_CREDENTIALS_FILE".into(),
                path.to_string_lossy().to_string(),
            ),
            (
                "AWS_CONFIG_FILE".into(),
                dir.path().join("missing").to_string_lossy().to_string(),
            ),
            ("AWS_EC2_METADATA_DISABLED".into(), "true".into()),
        ]);
        assert_eq!(configured("amazon-bedrock", &values), None);
        values.insert("AWS_PROFILE".into(), "chosen".into());
        assert_eq!(
            configured("amazon-bedrock", &values),
            Some("AWS credential chain")
        );
    }

    #[test]
    fn region_uses_the_selected_profile_unless_environment_overrides_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        std::fs::write(
            &path,
            "[default]\nregion=us-east-1\n[profile chosen]\nregion=eu-west-1\n",
        )
        .unwrap();
        let mut values = BTreeMap::from([
            ("AWS_PROFILE".into(), "chosen".into()),
            ("AWS_CONFIG_FILE".into(), path.to_string_lossy().to_string()),
            (
                "AWS_SHARED_CREDENTIALS_FILE".into(),
                dir.path().join("missing").to_string_lossy().to_string(),
            ),
        ]);
        assert_eq!(aws_region(&values).as_deref(), Some("eu-west-1"));
        values.insert("AWS_DEFAULT_REGION".into(), "ap-southeast-1".into());
        assert_eq!(aws_region(&values).as_deref(), Some("ap-southeast-1"));
        values.insert("AWS_REGION".into(), "us-west-2".into());
        assert_eq!(aws_region(&values).as_deref(), Some("us-west-2"));
    }
}
