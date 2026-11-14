//! cargo run -p rpi-runtime --example embedded --locked
//! This host supplies in-memory configuration and a no-network provider. It
//! does not depend on rpi-cli, discover user files, or access environment keys.
use rpi_ai::{
    providers::faux::{FauxProvider, FauxScript},
    Api, Context, Model, SimpleStreamOptions,
};
use rpi_runtime::{ModelRequest, ProviderDeclaration, ProviderRegistry, Runtime, RuntimeConfig};
use std::collections::BTreeMap;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let model = Model::new(
        "embedded-model",
        "Embedded model",
        Api::Faux,
        "faux",
        "http://localhost:0",
    );
    let mut providers = ProviderRegistry::default();
    providers.declare(ProviderDeclaration {
        id: "faux".into(),
        models: vec![model.clone()],
        api_key_env: None,
        requires_auth: false,
    });
    providers.register(
        FauxProvider::with_models(
            FauxScript::new().with_text("Hello from the embedded runtime."),
            vec![model],
        ),
        Api::Faux,
    );
    let config = RuntimeConfig::default();
    let environment = BTreeMap::<String, String>::new();
    let request = ModelRequest {
        provider: Some("faux"),
        model: Some("embedded-model"),
        ..Default::default()
    };
    let runtime = Runtime::new(&config, &providers, &environment, &request)?;
    let active = runtime.current();
    let response = active
        .provider
        .stream_simple(
            &active.model,
            &Context::new(vec![]),
            &SimpleStreamOptions::default(),
        )
        .await
        .result()
        .await?;
    println!("{}", serde_json::to_string(&runtime.snapshot().selection)?);
    for content in response.content {
        if let rpi_ai::Content::Text(text) = content {
            println!("{}", text.text);
        }
    }
    Ok(())
}
