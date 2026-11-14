use super::resource_lifecycle::{fixture_model_config, rejected, session_files};
use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn corrupt_binding_metadata_is_rejected_without_registration_or_rewrite() {
    let fixture = Fixture::build().await;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fixture_model_config(root);
    let selection = json!({"version":1,"extensions":[fixture.library],"tools":{"mode":"none"}});
    put(root.join("exact.json"), selection.to_string());
    let mut host = Host::start(root, "exact.json", "corrupt-binding").await;
    let state = host.state().await;
    host.stop().await;
    let files = session_files(root);
    let (path, original) = files
        .iter()
        .find(|(p, _)| p.extension().is_some_and(|e| e == "jsonl"))
        .unwrap();
    let registration = std::fs::read(root.join("registrations.log")).unwrap();
    put(root.join("transition.json"), json!({"version":1,"operation_id":"test-corruption","expected":{"revision":state["binding"]["revision"],"fingerprint":state["binding"]["fingerprint"]},"target":selection}).to_string());
    fn replace(value: &mut Value, field: &str, damaged: &Value) -> usize {
        let mut count = 0;
        if value["key"] == "rpi.resource-selection.v1" && value.get("value").is_some() {
            value["value"][field] = damaged.clone();
            return 1;
        }
        match value {
            Value::Object(o) => {
                for v in o.values_mut() {
                    count += replace(v, field, damaged);
                }
            }
            Value::Array(a) => {
                for v in a {
                    count += replace(v, field, damaged);
                }
            }
            _ => {}
        }
        count
    }
    for (field, damaged) in [
        ("revision", json!("corrupt")),
        ("revision", json!(null)),
        ("revision", json!(0)),
        ("revision", json!(-1)),
        ("operations", json!([])),
        ("operations", json!(1)),
        ("operations", json!(null)),
        ("operationId", json!(false)),
        ("operations", json!({"test-corruption":[]})),
        (
            "operations",
            json!({"test-corruption":{"target":"bad","expected":[],"revision":2,"fingerprint":"bad"}}),
        ),
    ] {
        let mut count = 0;
        let text = String::from_utf8(original.clone())
            .unwrap()
            .lines()
            .map(|line| {
                let mut entry: Value = serde_json::from_str(line).unwrap();
                count += replace(&mut entry, field, &damaged);
                entry.to_string() + "\n"
            })
            .collect::<String>();
        assert!(count > 0, "binding fixture not located");
        put(path, text);
        for flag in ["--resource-config", "--resource-transition"] {
            let config = if flag == "--resource-config" {
                "exact.json"
            } else {
                "transition.json"
            };
            rejected(
                root,
                &[flag, config, "--session-id", "corrupt-binding"],
                "invalid persisted resource binding metadata",
            )
            .await;
        }
        assert_eq!(
            std::fs::read(root.join("registrations.log")).unwrap(),
            registration
        );
    }
    put(path, original);
    Host::start(root, "exact.json", "corrupt-binding")
        .await
        .stop()
        .await;
}
