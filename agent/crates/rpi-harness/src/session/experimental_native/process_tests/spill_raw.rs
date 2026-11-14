//! Test-only raw evidence for a guarded synthetic fixture.
//!
//! The caller must have verified the matching marker and that the child remains
//! paused, and must keep it paused throughout `evidence`. Paths must come from
//! the caller's canonical, guarded fixture, not from an arbitrary child payload.
//! All raw handles close before returning (and before caller kill/recovery or
//! release). These helpers never open SQLite, copy a live DB, flush, or write.
//! Header/space checks do not parse journal records or verify their checksums.

#![cfg(test)]

use std::fs::{File, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};

const CAP: u64 = 32 * 1024 * 1024;

fn open_readonly(path: &Path) -> File {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE; access is READ only.
        options.share_mode(1 | 2 | 4);
    }
    options
        .open(path)
        .unwrap_or_else(|error| panic!("spill raw open: {error}"))
}

fn bounded_len(file: &File) -> u64 {
    let metadata = file
        .metadata()
        .unwrap_or_else(|error| panic!("spill raw metadata: {error}"));
    assert!(metadata.is_file(), "spill raw requires a regular file");
    let len = metadata.len();
    assert!(len <= CAP, "spill raw file exceeds cap: {len}");
    len
}

/// Read a closed baseline, or a main file while the marker-proven child is paused.
/// Metadata is checked before reading; `take` also bounds a growing file.
pub(super) fn main_bytes(path: &Path) -> Vec<u8> {
    let file = open_readonly(path);
    let expected_len = bounded_len(&file);
    let mut bytes = Vec::new();
    file.take(CAP + 1)
        .read_to_end(&mut bytes)
        .unwrap_or_else(|error| panic!("spill raw main read: {error}"));
    assert!(bytes.len() as u64 <= CAP, "spill raw main read exceeds cap");
    assert!(
        bytes.len() as u64 == expected_len,
        "spill raw main length changed during read"
    );
    bytes
}

/// Append to the complete OS path, retaining non-UTF-8 and Unicode path contents.
pub(super) fn journal_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push("-journal");
    PathBuf::from(name)
}

/// Inspect only the first journal header and bounded main bytes while paused.
/// A valid first segment need not account for the entire journal file: later
/// synced segments may have their own headers. This is not checksum validation,
/// hot-journal recovery, or evidence of power-loss durability.
pub(super) fn evidence(path: &Path, baseline: &[u8]) -> serde_json::Value {
    assert!(
        baseline.len() as u64 <= CAP,
        "spill raw baseline exceeds cap"
    );
    assert!(baseline.len() >= 18, "spill raw baseline header is short");
    assert!(
        &baseline[..16] == b"SQLite format 3\0",
        "spill raw baseline DB magic mismatch"
    );
    let encoded_page_size = u16::from_be_bytes([baseline[16], baseline[17]]);
    let baseline_page_size = if encoded_page_size == 1 {
        65536_u32
    } else {
        u32::from(encoded_page_size)
    };
    assert!(
        (512..=65536).contains(&baseline_page_size) && baseline_page_size.is_power_of_two(),
        "spill raw baseline page size invalid"
    );
    assert!(
        baseline
            .chunks_exact(baseline_page_size as usize)
            .remainder()
            .is_empty(),
        "spill raw baseline has partial page"
    );
    let baseline_pages = baseline.len() as u64 / u64::from(baseline_page_size);

    // This scope closes the journal handle before reading main, even on failure.
    let (header, journal_bytes) = {
        let mut file = open_readonly(&journal_path(path));
        let len = bounded_len(&file);
        assert!(len >= 28, "spill raw journal header is short: {len}");
        let mut header = [0_u8; 28];
        file.read_exact(&mut header)
            .unwrap_or_else(|error| panic!("spill raw journal header read: {error}"));
        (header, len)
    };
    assert!(
        header[..8] == [0xd9, 0xd5, 0x05, 0xf9, 0x20, 0xa1, 0x63, 0xd7],
        "spill raw journal magic mismatch"
    );
    let be_u32 = |offset: usize| {
        u32::from_be_bytes([
            header[offset],
            header[offset + 1],
            header[offset + 2],
            header[offset + 3],
        ])
    };
    let records = be_u32(8);
    // Offset 12 is an arbitrary checksum seed, intentionally not validated.
    let original_pages = be_u32(16);
    let sector_size = be_u32(20);
    let page_size = be_u32(24);
    assert!(
        records != 0 && records != u32::MAX,
        "spill raw journal record count is zero or sentinel"
    );
    assert!(
        u64::from(original_pages) == baseline_pages,
        "spill raw journal original page count mismatch"
    );
    assert!(
        (32..=65536).contains(&sector_size) && sector_size.is_power_of_two(),
        "spill raw journal sector size invalid"
    );
    assert!(
        (512..=65536).contains(&page_size) && page_size.is_power_of_two(),
        "spill raw journal page size invalid"
    );
    assert!(
        page_size == baseline_page_size,
        "spill raw journal page size mismatch"
    );
    let first_segment_end = u64::from(page_size)
        .checked_add(8)
        .and_then(|record_size| u64::from(records).checked_mul(record_size))
        .and_then(|record_bytes| u64::from(sector_size).checked_add(record_bytes))
        .expect("spill raw journal extent overflow");
    assert!(
        first_segment_end <= journal_bytes,
        "spill raw journal first segment exceeds file length"
    );

    let staged = main_bytes(path);
    assert!(
        staged.len() >= baseline.len(),
        "spill raw main became shorter"
    );
    let mut changed_original_bytes = 0_usize;
    let mut first_changed_offset = None;
    for (offset, (original, current)) in baseline.iter().zip(staged.iter()).enumerate() {
        if original != current {
            changed_original_bytes += 1;
            first_changed_offset.get_or_insert(offset);
        }
    }
    let first_changed_offset = first_changed_offset
        .expect("spill raw main has no changed byte in original baseline range");

    serde_json::json!({
        "original_pages": original_pages,
        "page_size": page_size,
        "sector_size": sector_size,
        "records": records,
        "journal_bytes": journal_bytes,
        "baseline_bytes": baseline.len(),
        "staged_bytes": staged.len(),
        "changed_original_bytes": changed_original_bytes,
        "first_changed_offset": first_changed_offset,
    })
}
