use super::*;
#[test]
fn strict_request_duplicates_utf8_size_and_output_budget() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("request.json");
    let valid = json!({"version":1,"sessionId":"id","sessionRoot":root.path(),"sessionPath":root.path().join("id.jsonl"),"limit":1,"customTypes":[],"cursor":null});
    std::fs::write(&path, valid.to_string()).unwrap();
    assert!(request(&path).is_ok());
    let duplicate = format!("{{\"version\":1,{}", &valid.to_string()[1..]);
    std::fs::write(&path, duplicate).unwrap();
    assert!(request(&path).is_err());
    std::fs::write(&path, [255, 254]).unwrap();
    assert!(request(&path).is_err());
    std::fs::File::create(&path)
        .unwrap()
        .set_len((MAX_REQUEST + 1) as u64)
        .unwrap();
    assert_eq!(request(&path).unwrap_err().code, "limit_exceeded");
    let mut output = Output(Vec::new());
    assert!(output.write(&vec![0; MAX_RESPONSE]).is_err());
    assert!(output.0.is_empty());
    let mut size = SizeLimit(2);
    assert!(size.write(b"abc").is_err());
    assert!(
        serde_json::to_vec(&crate::resource_selection::capabilities())
            .unwrap()
            .len()
            < crate::resource_selection::MAX_CAPABILITIES_BYTES
    );
}
#[tokio::test]
async fn total_source_budget_and_range_limits_are_enforced_below_allocation() {
    use rpi_tools::FileSystem;
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("history.jsonl");
    std::fs::File::create(&path)
        .unwrap()
        .set_len(32 * 1024 * 1024)
        .unwrap();
    let path = std::fs::canonicalize(path).unwrap();
    let progress = PathBuf::from(format!("{}.progress.jsonl", path.display()));
    std::fs::File::create(&progress)
        .unwrap()
        .set_len(33 * 1024 * 1024)
        .unwrap();
    let error = readonly_fs::ReadonlyFs::new(&path, Instant::now() + Duration::from_secs(10))
        .err()
        .expect("combined source budget must fail before allocation");
    assert!(error.contains("byte limit"), "{error}");
    std::fs::remove_file(&progress).unwrap();
    let fs = readonly_fs::ReadonlyFs::new(&path, Instant::now() + Duration::from_secs(10)).unwrap();
    let reader = fs
        .open_read_file(path.to_str().unwrap())
        .await
        .unwrap()
        .unwrap();
    assert!(reader.read_range(0, usize::MAX).await.is_err());
    assert_eq!(
        fs.read_binary_file(path.to_str().unwrap(), None)
            .await
            .unwrap()
            .len(),
        32 * 1024 * 1024
    );
    assert!(fs
        .read_binary_file(progress.to_str().unwrap(), None)
        .await
        .is_err());
}
#[tokio::test]
async fn expired_reader_deadline_refuses_before_read_all_or_allocation() {
    use rpi_tools::FileSystem;
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("history.jsonl");
    std::fs::write(&path, b"unread").unwrap();
    let path = std::fs::canonicalize(path).unwrap();
    assert!(
        readonly_fs::ReadonlyFs::new(&path, Instant::now() - Duration::from_millis(1))
            .err()
            .expect("expired deadline must refuse before reading input")
            .contains("deadline")
    );
    let fs = readonly_fs::ReadonlyFs::new(&path, Instant::now() + Duration::from_secs(10)).unwrap();
    assert!(fs
        .write_file(
            path.to_str().unwrap(),
            rpi_tools::FileContent::Text("no writes".into()),
            None
        )
        .await
        .is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"unread");
}
