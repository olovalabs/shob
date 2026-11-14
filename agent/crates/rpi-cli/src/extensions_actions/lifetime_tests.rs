use super::*;
use rpi_ai::providers::faux::{FauxProvider, FauxScript};
use rpi_harness::types::AgentHarnessOptions;
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expired_reverse_bindings_fail_closed_but_still_reclaim_their_children() {
    let (host, cell) = HarnessActionHost::new_empty(
        vec![],
        std::env::temp_dir(),
        Handle::current(),
        BTreeMap::new(),
        rpi_extensions::ToolCallContext::default(),
    );
    assert!(host
        .get_execution_context(json!({}))
        .await
        .unwrap_err()
        .contains("before harness"));
    let provider = FauxProvider::new(
        FauxScript::new()
            .with_text("slow")
            .with_tokens_per_second(0.1),
    );
    let parent = AgentHarness::create(AgentHarnessOptions {
        model: provider.default_model().clone(),
        models: vec![provider],
        ..Default::default()
    })
    .await
    .unwrap();
    HarnessActionHost::set_harness(&cell, Arc::new(parent.clone()));
    assert!(host.get_execution_context(json!({})).await.is_ok());
    let child = host
        .child_agent(7, json!({"op":"start","task":"report"}))
        .await
        .unwrap();
    let id = &child["childId"];
    drop(parent);
    assert!(host
        .get_execution_context(json!({}))
        .await
        .unwrap_err()
        .contains("after harness"));
    assert!(host
        .child_agent(7, json!({"op":"start","task":"stale"}))
        .await
        .unwrap_err()
        .contains("after harness"));
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let value = host
                .child_agent(7, json!({"op":"poll","childId":id}))
                .await
                .unwrap();
            if value["status"] == "failed" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
    assert!(host
        .child_agent(8, json!({"op":"dispose","childId":id}))
        .await
        .is_err());
    host.child_agent(7, json!({"op":"dispose","childId":id}))
        .await
        .unwrap();
    assert!(host
        .child_agent(7, json!({"op":"poll","childId":id}))
        .await
        .is_err());
}

/// `sendUserMessage` 必须认 `deliverAs`。
///
/// 以前它一律 `prompt_text(..)`：跑着时 lane 忙 → 报错 → 而这个动作是阻塞到
/// 整轮跑完的，桥只看 `dispatched`，错误没人看 → 消息静默丢失；
/// steer / followUp / nextTurn 也都变成「开新一轮」。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_user_message_queues_while_the_lane_is_busy() {
    let (host, cell) = HarnessActionHost::new_empty(
        vec![],
        std::env::temp_dir(),
        Handle::current(),
        BTreeMap::new(),
        rpi_extensions::ToolCallContext::default(),
    );
    // 慢流：跑着的时候才有窗口把消息排进去。
    let provider = FauxProvider::new(
        FauxScript::new()
            .with_text("slow")
            .with_tokens_per_second(0.5),
    );
    let harness = AgentHarness::create(AgentHarnessOptions {
        model: provider.default_model().clone(),
        models: vec![provider],
        ..Default::default()
    })
    .await
    .unwrap();
    HarnessActionHost::set_harness(&cell, Arc::new(harness.clone()));

    let lane = harness.lane("main");
    let running = tokio::spawn({
        let lane = lane.clone();
        async move { lane.prompt_text("go", Vec::new()).await }
    });
    // 等 run 真的被接纳，再发消息。
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if harness
                .snapshot()
                .await
                .map(|s| s.active.is_some())
                .unwrap_or(false)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("the run must become active");

    let value = host
        .send_user_message(json!({"text": "插队的消息", "deliverAs": "steer"}))
        .await
        .expect("发消息不能因为 lane 忙就失败");
    assert_eq!(
        value["status"], "queued",
        "跑着时发消息应当进队列，而不是开新一轮或报错：{value}"
    );

    // 队列里真的有它（不是被吞掉）。
    let queued = lane.clear_queue().await.unwrap();
    assert_eq!(
        queued
            .steering
            .iter()
            .map(|m| m.text.as_str())
            .collect::<Vec<_>>(),
        vec!["插队的消息"]
    );

    let _ = lane.abort().await;
    let _ = running.await;
}

/// 中止必须把排队消息**交回调用方**，且每条带着自己的图片。
///
/// 队列 `[仅文字][文字+红图][文字+蓝图]`：队首本来没图，扁平方案必然把后面
/// 那条的图贴到队首上 —— 用户看到的图文对就错了，发出去模型看到的也错。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_queued_message_keeps_its_own_image() {
    let (host, cell) = HarnessActionHost::new_empty(
        vec![],
        std::env::temp_dir(),
        Handle::current(),
        BTreeMap::new(),
        rpi_extensions::ToolCallContext::default(),
    );
    let provider = FauxProvider::new(FauxScript::new().with_text("ok"));
    let harness = AgentHarness::create(AgentHarnessOptions {
        model: provider.default_model().clone(),
        models: vec![provider],
        ..Default::default()
    })
    .await
    .unwrap();
    HarnessActionHost::set_harness(&cell, Arc::new(harness.clone()));

    // 两张**不同**的合法 1x1 PNG（红 / 蓝）。必须是真的 PNG：`steer` 会走
    // `normalize_user_images`，解不出来的会被换成占位文字并丢掉图片。
    let red = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC";
    let blue = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGNgYPgPAAEDAQAIicLsAAAAAElFTkSuQmCC";
    let image = |data: &str| rpi_ai::types::ImageContent {
        kind: rpi_ai::types::ImageContentType,
        data: data.into(),
        mime_type: "image/png".into(),
    };
    let text_block = |text: &str| {
        rpi_ai::types::Content::Text(rpi_ai::types::TextContent {
            kind: rpi_ai::types::TextContentType,
            text: text.into(),
            text_signature: None,
        })
    };
    let with_image = |text: &str, data: &str| {
        rpi_agent::AgentMessage::User(rpi_ai::types::UserMessage::new(
            rpi_ai::types::UserContent::Blocks(vec![
                text_block(text),
                rpi_ai::types::Content::Image(image(data)),
            ]),
            0,
        ))
    };
    let text_only = |text: &str| {
        rpi_agent::AgentMessage::User(rpi_ai::types::UserMessage::new(
            rpi_ai::types::UserContent::Text(text.into()),
            0,
        ))
    };

    let lane = harness.lane("main");
    lane.steer(text_only("只有文字")).await.unwrap();
    lane.steer(with_image("红色的那句", red)).await.unwrap();
    lane.follow_up(with_image("蓝色的那句", blue))
        .await
        .unwrap();

    let value = host.abort(json!({})).await.unwrap();
    let steer = value["steer"].as_array().expect("steer is an array");
    assert_eq!(steer.len(), 2, "{value}");
    assert_eq!(steer[0]["text"], json!("只有文字"), "{value}");
    assert_eq!(
        steer[0]["images"],
        json!([]),
        "队首本来没图，不能被别的消息的图污染：{value}"
    );
    assert_eq!(steer[1]["text"], json!("红色的那句"), "{value}");
    assert_eq!(
        steer[1]["images"][0]["data"],
        json!(red),
        "图必须跟着它自己的文字：{value}"
    );

    let follow = value["followUp"].as_array().expect("followUp is an array");
    assert_eq!(follow.len(), 1, "{value}");
    assert_eq!(follow[0]["text"], json!("蓝色的那句"), "{value}");
    assert_eq!(
        follow[0]["images"][0]["data"],
        json!(blue),
        "follow-up 的图不能和 steer 的图混淆：{value}"
    );
}

/// 空闲时中止是幂等 no-op，队列照样交回（且分类不变）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn abort_reclaims_on_an_idle_lane_and_keeps_queue_kinds() {
    let (host, cell) = HarnessActionHost::new_empty(
        vec![],
        std::env::temp_dir(),
        Handle::current(),
        BTreeMap::new(),
        rpi_extensions::ToolCallContext::default(),
    );
    let provider = FauxProvider::new(FauxScript::new().with_text("ok"));
    let harness = AgentHarness::create(AgentHarnessOptions {
        model: provider.default_model().clone(),
        models: vec![provider],
        ..Default::default()
    })
    .await
    .unwrap();
    HarnessActionHost::set_harness(&cell, Arc::new(harness.clone()));

    let lane = harness.lane("main");
    let msg = |text: &str| {
        rpi_agent::AgentMessage::User(rpi_ai::types::UserMessage::new(
            rpi_ai::types::UserContent::Text(text.into()),
            0,
        ))
    };
    lane.steer(msg("插队的")).await.unwrap();
    lane.follow_up(msg("排队的")).await.unwrap();

    let value = host.abort(json!({})).await.unwrap();
    assert_eq!(value["aborted"], json!(false), "空闲中止是 no-op，不是错误");
    assert_eq!(
        value["steer"],
        json!([{ "text": "插队的", "images": [] }]),
        "{value}"
    );
    assert_eq!(
        value["followUp"],
        json!([{ "text": "排队的", "images": [] }]),
        "follow-up 被当成 steer 恢复的话，会在回合中途投递：{value}"
    );
    // 移交而不是复制：再来一次就空了。
    let again = host.abort(json!({})).await.unwrap();
    assert_eq!(again["steer"], json!([]));
    assert_eq!(again["followUp"], json!([]));
}
