//! Standalone parser specifications. Parent must wire the module before running.
use super::*;
use base64::{engine::general_purpose::STANDARD, Engine};

fn png_bytes() -> Vec<u8> {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, 1, 1);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder
            .write_header()
            .unwrap()
            .write_image_data(&[21, 42, 84, 255])
            .unwrap();
    }
    bytes
}

fn image_with_size(size: Option<usize>) -> Value {
    let mut bytes = png_bytes();
    if let Some(size) = size {
        assert!(size >= bytes.len());
        bytes.resize(size, 0);
    }
    json!({"type":"image", "mimeType":"image/png", "data": STANDARD.encode(bytes)})
}

fn command(blocks: Vec<Value>) -> Value {
    json!({"type":"prompt", "id":"test", "contentBlocks": blocks})
}

#[test]
fn legacy_aliases_keep_dispatcher_precedence_and_no_new_limits() {
    for field in ["message", "content", "prompt"] {
        assert_eq!(
            parse_prompt_content(&json!({field: "legacy"})).unwrap(),
            ParsedPrompt::Legacy("legacy".into())
        );
    }
    assert_eq!(
        parse_prompt_content(&json!({"message":"first", "content":"second", "prompt":"third"}))
            .unwrap(),
        ParsedPrompt::Legacy("first".into())
    );
    assert_eq!(
        parse_prompt_content(&json!({"content":"second", "prompt":"third"})).unwrap(),
        ParsedPrompt::Legacy("second".into())
    );
    for invalid in [Value::Null, json!(3), json!({}), json!([])] {
        assert!(
            parse_prompt_content(&json!({"message":invalid, "content":"not a fallback"})).is_err()
        );
    }
    let long = "x".repeat(MAX_PROMPT_BYTES + 1);
    assert_eq!(
        parse_prompt_content(&json!({"message":long})).unwrap(),
        ParsedPrompt::Legacy(long)
    );
    assert_eq!(
        parse_prompt_content(&json!({"message":""})).unwrap(),
        ParsedPrompt::Legacy(String::new())
    );
    assert!(parse_prompt_content(&json!({})).is_err());
}

#[test]
fn mutually_exclusive_presence_and_reserved_images_never_fall_back() {
    for field in ["message", "content", "prompt"] {
        for value in [Value::Null, json!("text"), json!(2)] {
            let mut input = command(vec![json!({"type":"text", "text":"new"})]);
            input[field] = value;
            assert!(parse_prompt_content(&input)
                .unwrap_err()
                .contains("cannot be combined"));
        }
    }
    for images in [Value::Null, json!([]), json!([{"data":"secret-payload"}])] {
        for mut input in [
            json!({"message":"legacy"}),
            command(vec![image_with_size(None)]),
        ] {
            input["images"] = images.clone();
            let error = parse_prompt_content(&input).unwrap_err();
            assert!(error.contains("unsupported"));
            assert!(!error.contains("secret-payload"));
        }
    }
    for invalid in [Value::Null, json!([]), json!("bad"), json!({})] {
        assert!(parse_prompt_content(&json!({"contentBlocks":invalid})).is_err());
    }
}

#[test]
fn image_only_and_interleaving_keep_order_and_exact_data() {
    let image = image_with_size(None);
    let input = command(vec![
        json!({"type":"text", "text":"before"}),
        image.clone(),
        json!({"type":"text", "text":"after"}),
        image.clone(),
    ]);
    let parsed = parse_prompt_content(&input).unwrap();
    assert!(parsed.has_images());
    let ParsedPrompt::Blocks(blocks) = parsed else {
        panic!("expected blocks")
    };
    assert_eq!(
        serde_json::to_value(blocks).unwrap(),
        input["contentBlocks"]
    );
    let parsed = parse_prompt_content(&command(vec![image])).unwrap();
    assert!(parsed.has_images());
    assert!(
        !parse_prompt_content(&command(vec![json!({"type":"text", "text":""})]))
            .unwrap()
            .has_images()
    );
    assert!(!ParsedPrompt::Legacy("legacy".into()).has_images());
}

#[test]
fn only_plain_text_and_inline_image_fields_are_accepted() {
    let mut provider_image = image_with_size(None);
    provider_image["uri"] = json!("https://inert.invalid/secret-payload");
    let invalid = vec![
        json!(null),
        json!(1),
        json!([]),
        json!("text"),
        json!({}),
        json!({"type":"unknown-secret-payload"}),
        json!({"type":"thinking", "thinking":"secret-payload"}),
        json!({"type":"toolCall", "id":"x", "name":"x", "arguments":{}}),
        json!({"type":"text", "text":null}),
        json!({"type":"text"}),
        json!({"type":"text", "text":2}),
        json!({"type":"text", "text":"ok", "textSignature":"secret-payload"}),
        json!({"type":"text", "text":"ok", "arbitrary":null}),
        json!({"type":"image", "data":"secret-payload"}),
        json!({"type":"image", "data":null, "mimeType":"image/png"}),
        json!({"type":"image", "data":"AAAA", "mimeType":null}),
        provider_image,
    ];
    for block in invalid {
        let error = parse_prompt_content(&command(vec![block])).unwrap_err();
        assert!(error.starts_with("contentBlocks[0]:"));
        assert!(!error.contains("secret-payload"));
    }
}

#[test]
fn canonical_base64_mime_signature_and_body_errors_are_explicit() {
    for data in [
        "",
        "Zh==",
        "Zg",
        "Zg==\n",
        "_w==",
        "data:image/png;base64,AAAA",
        "secret-payload!",
    ] {
        let input = command(vec![
            json!({"type":"image", "mimeType":"image/png", "data":data}),
        ]);
        let error = parse_prompt_content(&input).unwrap_err();
        assert!(!error.contains("secret-payload"));
    }
    for mime in [
        "image/jpeg",
        "image/jpg",
        "image/png; charset=utf-8",
        "IMAGE/PNG",
        "image/gif",
    ] {
        let mut image = image_with_size(None);
        image["mimeType"] = json!(mime);
        assert!(parse_prompt_content(&command(vec![image])).is_err());
    }
    // A valid signature alone does not qualify as a decoded image.
    for data in [
        STANDARD.encode(b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR"),
        STANDARD.encode(b"\xff\xd8\xff\xe0"),
    ] {
        for mime in ["image/png", "image/jpeg"] {
            assert!(parse_prompt_content(&command(vec![
                json!({"type":"image", "mimeType":mime, "data":data})
            ]))
            .is_err());
        }
    }
    let mut bad = png_bytes();
    bad.truncate(40);
    assert!(parse_prompt_content(&command(vec![
        json!({"type":"image", "mimeType":"image/png", "data":STANDARD.encode(bad)})
    ]))
    .is_err());
}

#[test]
fn block_and_image_counts_accept_exactly_the_limit() {
    let text = json!({"type":"text", "text":""});
    assert!(parse_prompt_content(&command(vec![text.clone(); MAX_CONTENT_BLOCKS])).is_ok());
    assert!(parse_prompt_content(&command(vec![text; MAX_CONTENT_BLOCKS + 1])).is_err());
    let image = image_with_size(None);
    assert!(parse_prompt_content(&command(vec![image.clone(); MAX_IMAGES])).is_ok());
    assert!(parse_prompt_content(&command(vec![image; MAX_IMAGES + 1]))
        .unwrap_err()
        .contains("4 images"));
}

#[test]
fn utf8_text_budget_is_aggregate_bytes_not_characters() {
    let mut input = command(vec![
        json!({"type":"text", "text":"é".repeat(MAX_TEXT_BYTES / 2 - 1)}),
        json!({"type":"text", "text":"é"}),
    ]);
    assert!(parse_prompt_content(&input).is_ok());
    input["contentBlocks"][1]["text"] = json!("éx");
    assert!(parse_prompt_content(&input).unwrap_err().contains("32768"));
    assert!(parse_prompt_content(&command(vec![
        json!({"type":"text", "text":"\0".repeat(MAX_TEXT_BYTES)})
    ]))
    .is_ok());
}

#[test]
fn exact_decoded_image_budgets_not_base64_estimates() {
    let max = rpi_tools::image_processing::RPC_IMAGE_MAX_BYTES;
    assert!(parse_prompt_content(&command(vec![
        image_with_size(Some(max)),
        image_with_size(Some(max))
    ]))
    .is_ok());
    assert!(parse_prompt_content(&command(vec![image_with_size(Some(max + 1))])).is_err());
    let input = command(vec![
        image_with_size(Some(max)),
        image_with_size(Some(max - 100)),
        image_with_size(Some(101)),
    ]);
    assert!(parse_prompt_content(&input)
        .unwrap_err()
        .contains("2097152"));
    assert!(parse_prompt_content(&command(vec![
        image_with_size(Some(max)),
        image_with_size(Some(max / 2)),
        image_with_size(Some(max / 2))
    ]))
    .is_ok());
}

#[test]
fn exact_serialized_command_boundary_includes_id_extensions_escaping_and_lf() {
    let mut input = command(vec![json!({"type":"text", "text":"\0\n\"\\é"})]);
    // Non-string IDs are allowed by the existing envelope. Account for every
    // JSON escape and container byte rather than sizing id.as_str().
    input["id"] = json!({"nested":["\0\n\"\\é", 123]});
    input["extensionPadding"] = json!("");
    let base = serde_json::to_vec(&input).unwrap().len() + 1;
    input["extensionPadding"] = json!("x".repeat(MAX_PROMPT_BYTES - base));
    assert_eq!(
        serde_json::to_vec(&input).unwrap().len() + 1,
        MAX_PROMPT_BYTES
    );
    parse_prompt_content(&input).unwrap();
    let padding = format!("{}x", input["extensionPadding"].as_str().unwrap());
    input["extensionPadding"] = json!(padding);
    assert_eq!(
        serde_json::to_vec(&input).unwrap().len() + 1,
        MAX_PROMPT_BYTES + 1
    );
    assert!(parse_prompt_content(&input)
        .unwrap_err()
        .contains("3145728"));
}

#[test]
fn capability_profile_contains_all_bounds_for_later_integration() {
    assert_eq!(
        prompt_capabilities(),
        json!({
            "contentBlocksVersion":1, "image":true,
            "imageMimeTypes":["image/png", "image/jpeg"],
            "maxImages":4, "maxImageBytes":1048576, "maxTotalImageBytes":2097152,
            "maxImageBase64Bytes":1398104, "maxPromptBytes":3145728,
            "maxTextBytes":32768, "maxContentBlocks":64, "maxImageSide":2000,
            "maxImagePixels":4000000, "imageDecodeAllocBudgetBytes":67108864,
            "frameBytes":4194304
        })
    );
}
