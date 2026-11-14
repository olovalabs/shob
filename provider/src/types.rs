use serde::{Deserialize, Serialize};

/// High-level authentication type for a provider.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AuthType {
    /// OAuth 2.0 with authorization code + PKCE flow
    OAuthCode,
    /// OAuth 2.0 Device Code flow (RFC 8628)
    DeviceCode,
    /// Custom interactive login flow (e.g. CLI hook / IDE auth)
    Custom,
    /// Direct API key authentication
    ApiKey,
    /// No authentication required or provider-managed
    None,
}

/// A specification of an AI model belonging to a provider.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ModelSpec {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub context_window: Option<u64>,
    #[serde(default)]
    pub max_tokens: Option<u64>,
    #[serde(default)]
    pub reasoning: bool,
    #[serde(default)]
    pub input: Vec<String>,
}

/// OAuth login flow configuration extracted from oh-my-pi catalog rules.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct LoginConfig {
    pub kind: String,
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default)]
    pub scope_separator: Option<String>,
    #[serde(default)]
    pub pkce: Option<bool>,
    #[serde(default)]
    pub client_id: Option<String>,
    #[serde(default)]
    pub authorize_url: Option<String>,
    #[serde(default)]
    pub token_url: Option<String>,
    #[serde(default)]
    pub device_url: Option<String>,
    #[serde(default)]
    pub hook: Option<String>,
    #[serde(default)]
    pub instructions: Option<String>,
}

/// Token refresh configuration.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RefreshConfig {
    pub kind: String,
    #[serde(default)]
    pub token_url: Option<String>,
    #[serde(default)]
    pub hook: Option<String>,
}

/// The complete definition of a provider from oh-my-pi.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProviderSpec {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub env_vars: Vec<String>,
    #[serde(default = "default_api_key_format")]
    pub api_key_format: String,
    #[serde(default)]
    pub login: Option<LoginConfig>,
    #[serde(default)]
    pub refresh: Option<RefreshConfig>,
    #[serde(default)]
    pub callback_port: Option<u16>,
    #[serde(default)]
    pub paste_code: bool,
    #[serde(default)]
    pub default_model: Option<String>,
    #[serde(default)]
    pub models: Vec<ModelSpec>,
}

fn default_api_key_format() -> String {
    "bearer".to_string()
}

impl ProviderSpec {
    /// Return the primary authentication category for this provider.
    pub fn auth_type(&self) -> AuthType {
        if let Some(login) = &self.login {
            match login.kind.as_str() {
                "oauth-code" => AuthType::OAuthCode,
                "device-code" => AuthType::DeviceCode,
                "custom" => AuthType::Custom,
                "api-key" => AuthType::ApiKey,
                _ => {
                    if !self.env_vars.is_empty() {
                        AuthType::ApiKey
                    } else {
                        AuthType::None
                    }
                }
            }
        } else if !self.env_vars.is_empty() {
            AuthType::ApiKey
        } else {
            AuthType::None
        }
    }

    /// Whether this provider supports OAuth authentication.
    pub fn supports_oauth(&self) -> bool {
        matches!(
            self.auth_type(),
            AuthType::OAuthCode | AuthType::DeviceCode | AuthType::Custom
        ) || self.refresh.is_some()
    }

    /// Whether this provider supports API key authentication.
    pub fn supports_api_key(&self) -> bool {
        !self.env_vars.is_empty() || matches!(self.auth_type(), AuthType::ApiKey)
    }

    /// Returns the preferred environment variable for an API key.
    pub fn primary_env_var(&self) -> Option<&str> {
        self.env_vars.first().map(|s| s.as_str())
    }
}

/// The runtime authentication status of a provider.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthStatus {
    pub provider: String,
    pub ready: bool,
    pub source: Option<String>,
    pub auth_type: AuthType,
}

/// A stored credential.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum StoredCredential {
    #[serde(rename = "api_key")]
    ApiKey {
        key: String,
        #[serde(default)]
        source: Option<String>,
    },
    #[serde(rename = "oauth")]
    OAuth {
        access: String,
        #[serde(default)]
        refresh: Option<String>,
        #[serde(default)]
        expires: Option<u64>,
        #[serde(default)]
        email: Option<String>,
        #[serde(default)]
        account_id: Option<String>,
        #[serde(default)]
        org_id: Option<String>,
        #[serde(default)]
        org_name: Option<String>,
        #[serde(default)]
        authorized_at: Option<u64>,
    },
}
