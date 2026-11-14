//! Isolated synthetic JSONL load probe. Never use a real session directory.
//! `generate <new-directory> <entries> <body-bytes>` then `load <directory>`.
//! Optional single-use `exercise <directory> <append-count> <body-bytes>`.
use std::{
    fs::OpenOptions,
    io::{BufWriter, Write},
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use rpi_harness::session::{
    jsonl::{encode_header, encode_mutation, HeaderKind, JsonlSessionStorage, JsonlV4Header},
    types::{
        BranchEntryPageQuery, Entry, EntryBase, EntryQuery, MessageEntry, ProvisionedEntry,
        ProvisionedKind, SessionMutation, SessionStorage,
    },
    CounterIdGenerator, FakeClock,
};
use rpi_tools::{FileSystem, OsExecutionEnv};

const MARKER: &str = "synthetic-jsonl-history-probe-v1";

fn generate(
    root: &Path,
    count: usize,
    body_bytes: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    // Refuse reuse, so this command cannot overwrite an existing session root.
    std::fs::create_dir(root)?;
    std::fs::write(root.join("PROBE_ONLY"), MARKER)?;
    let file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(root.join("history.jsonl"))?;
    let mut writer = BufWriter::new(file);
    writer.write_all(
        encode_header(&JsonlV4Header {
            kind: HeaderKind,
            version: 4,
            id: "synthetic-history-probe".into(),
            created_at: 1,
            cwd: root.to_string_lossy().into_owned(),
            progress_format: None,
            parent_session_id: None,
            legacy_parent_session_path: None,
            metadata: None,
        })
        .as_bytes(),
    )?;
    let body = "x".repeat(body_bytes);
    for index in 0..count {
        let seq = index as u64 + 1;
        let entry = Entry::Message(MessageEntry {
            base: EntryBase {
                entry_type: "message".into(),
                id: format!("entry-{index}"),
                seq,
                parent_id: index
                    .checked_sub(1)
                    .map(|previous| format!("entry-{previous}")),
                timestamp: 1,
            },
            message: rpi_ai::UserMessage::new(body.clone(), 1).into(),
            terminate: None,
        });
        writer.write_all(
            encode_mutation(&SessionMutation::Entry {
                seq,
                timestamp: 1,
                lane: Some("main".into()),
                entry,
            })
            .as_bytes(),
        )?;
    }
    writer.flush()?;
    println!(
        "{}",
        serde_json::json!({"phase":"generated", "entries":count,
        "bodyBytes":body_bytes,"fileBytes":root.join("history.jsonl").metadata()?.len()})
    );
    Ok(())
}

async fn phase(
    value: serde_json::Value,
    final_phase: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("{value}");
    std::io::stdout().flush()?;
    // Allow the independent runner to sample current RSS and the native peak.
    // Deliberately do not trim heaps or retain any page/returned entry here.
    tokio::time::sleep(if final_phase {
        Duration::from_secs(2)
    } else {
        Duration::from_millis(500)
    })
    .await;
    Ok(())
}

async fn exercise(
    storage: &JsonlSessionStorage,
    append_count: usize,
    body_bytes: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    let defaults = BranchEntryPageQuery::default();
    let started = Instant::now();
    let mut older_pages = 0;
    let mut newer_pages = 0;
    let mut latest_pages = 0;
    // Repeatedly walk the branch both ways, keeping only small cursor strings.
    for _ in 0..3 {
        let mut query = BranchEntryPageQuery::default();
        let mut after_id;
        loop {
            let page = storage.branch_entry_page("main", &query).await?;
            older_pages += 1;
            let before_id = page.next_before_id.clone();
            after_id = page.last_id.clone();
            query.leaf_id = page.leaf_id.clone();
            drop(page);
            match before_id {
                Some(id) => query.before_id = Some(id),
                None => break,
            }
        }
        query.before_id = None;
        query.after_id = after_id;
        if query.after_id.is_some() {
            loop {
                let page = storage.branch_entry_page("main", &query).await?;
                newer_pages += 1;
                let after_id = page.next_after_id.clone();
                drop(page);
                match after_id {
                    Some(id) => query.after_id = Some(id),
                    None => break,
                }
            }
        }
        drop(storage.branch_entry_page("main", &defaults).await?);
        latest_pages += 1;
    }
    let page_ns = started.elapsed().as_nanos();
    phase(
        serde_json::json!({"phase":"paged", "pid":std::process::id(),
        "pageCount":older_pages + newer_pages + latest_pages,
        "olderPages":older_pages,"newerPages":newer_pages,"latestPages":latest_pages,
        "pageLimit":defaults.limit,"pageMaxBytes":defaults.max_bytes,"pageNs":page_ns}),
        false,
    )
    .await?;

    let initial_messages = storage.get_stats().await?.message_count;
    let mut appended = 0;
    let mut append_elapsed = Duration::ZERO;
    // Report bounded batches, not just the end, to expose the memory trajectory.
    loop {
        let batch_count = (append_count - appended).min(64);
        let started = Instant::now();
        for index in appended..appended + batch_count {
            // Explicit IDs cannot collide with fixture entry-* or CounterIdGenerator id-*.
            // This fixture is single-use; no body clone survives an append.
            drop(
                storage
                    .append_entry(
                        ProvisionedEntry {
                            id: format!("exercise-append-{index}"),
                            kind: ProvisionedKind::Message {
                                message: rpi_ai::UserMessage::new("a".repeat(body_bytes), 1).into(),
                                terminate: None,
                            },
                        },
                        "main",
                    )
                    .await?,
            );
        }
        let batch_ns = started.elapsed();
        append_elapsed += batch_ns;
        appended += batch_count;
        let total_messages = storage.get_stats().await?.message_count;
        let finished = appended == append_count;
        phase(serde_json::json!({"phase":"appended", "pid":std::process::id(),
            "appendCount":appended,"requestedAppendCount":append_count,"batchCount":batch_count,
            "bodyBytes":body_bytes,"initialMessages":initial_messages,"totalMessages":total_messages,
            "batchAppendNs":batch_ns.as_nanos(),"appendNs":append_elapsed.as_nanos(),
            "finished":finished}), finished).await?;
        if finished {
            break;
        }
    }
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    let mode = args.get(1).ok_or("expected generate/load/exercise")?;
    let root = Path::new(
        args.get(2)
            .ok_or("expected dedicated synthetic directory")?,
    );
    if mode == "generate" {
        return generate(
            root,
            args.get(3).ok_or("expected entry count")?.parse()?,
            args.get(4).ok_or("expected body bytes")?.parse()?,
        );
    }
    if (mode != "load" && mode != "exercise")
        || std::fs::read_to_string(root.join("PROBE_ONLY"))? != MARKER
    {
        return Err("load/exercise requires a directory created by this synthetic probe".into());
    }
    let exercise_args = if mode == "exercise" {
        let count = args
            .get(3)
            .ok_or("expected append count")?
            .parse::<usize>()?;
        let body_bytes = args.get(4).ok_or("expected body bytes")?.parse::<usize>()?;
        // Never append twice into the same synthetic fixture, even after a failed run.
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(root.join("EXERCISE_STARTED"))?;
        Some((count, body_bytes))
    } else {
        None
    };
    let root = std::fs::canonicalize(root)?;
    let path = root.join("history.jsonl");
    let fs: Arc<dyn FileSystem> = Arc::new(OsExecutionEnv::with_cwd(root.clone()));
    let started = Instant::now();
    let storage = JsonlSessionStorage::load(
        fs,
        &path.to_string_lossy(),
        Arc::new(FakeClock::new()),
        Arc::new(CounterIdGenerator::new()),
    )
    .await?;
    let elapsed = started.elapsed();
    if let Some((append_count, body_bytes)) = exercise_args {
        phase(serde_json::json!({"phase":"loaded","loadNs":elapsed.as_nanos(),
            "fileBytes":path.metadata()?.len(),"messageCount":storage.get_stats().await?.message_count,
            "pid":std::process::id()}), false).await?;
        exercise(&storage, append_count, body_bytes).await?;
        std::hint::black_box(&storage);
        return Ok(());
    }
    // A bounded read must not turn into full-history materialization.
    let page = storage
        .find_entries(&EntryQuery {
            limit: Some(5),
            ..Default::default()
        })
        .await?;
    println!(
        "{}",
        serde_json::json!({"phase":"ready", "loadNs":elapsed.as_nanos(),
        "fileBytes":path.metadata()?.len(),"messageCount":storage.get_stats().await?.message_count,
        "pageEntries":page.len(),"pid":std::process::id()})
    );
    std::io::stdout().flush()?;
    // The runner samples ready/peak memory while this owned state stays alive.
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    std::hint::black_box((&storage, &page));
    Ok(())
}
