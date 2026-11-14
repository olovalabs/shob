use crate::types::ProviderSpec;
use std::sync::LazyLock;

pub static PROVIDERS_JSON: &str = include_str!("data/providers.json");

pub static ALL_PROVIDERS: LazyLock<Vec<ProviderSpec>> = LazyLock::new(|| {
    serde_json::from_str(PROVIDERS_JSON)
        .expect("Embedded provider data must be valid JSON matching ProviderSpec")
});
