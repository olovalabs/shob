//! The tool-result cache: read dedupe, search dedupe, and the rules that keep
//! both from ever serving stale data.
//!
//! # The contract
//!
//! A hit requires all of:
//!
//! 1. the same request identity (path + range, or the search parameters);
//! 2. a content stamp recomputed **from the bytes on disk right now** that
//!    equals the stamp recorded when the result was produced;
//! 3. the delivered body still plausibly in context: within
//!    [`crate::token_efficiency::config::TokenEfficiencySettings::dedupe_window_turns`]
//!    turns, and no compaction since.
//!
//! Anything else is a miss that returns full content. `mtime` alone never
//! promotes an entry to a hit unless `trust_mtime` was explicitly enabled, and
//! the tests in this module hold that line.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::config::TokenEfficiencySettings;
use super::fsprobe::{FileProbe, FileSnapshot};
use super::hash::{Digest, key_digest};

/// Content identity of one file version. The digest is what decides; `len` and
/// `lines` are carried so a hypothetical digest collision would still have to
/// match on two more fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContentStamp {
    pub digest: Digest,
    pub len: u64,
    pub lines: usize,
}

impl ContentStamp {
    pub fn of_bytes(bytes: &[u8]) -> Self {
        Self {
            digest: Digest::from_bytes(bytes),
            len: bytes.len() as u64,
            lines: count_lines(bytes),
        }
    }
}

/// Newline count without allocating, matching `read`'s
/// `content.split('\n').collect()` semantics (a trailing newline yields a final
/// empty element).
pub fn count_lines(bytes: &[u8]) -> usize {
    bytes.iter().filter(|byte| **byte == b'\n').count() + 1
}

/// The range of a `read`, in file lines.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct ReadRange {
    /// 0-based first line.
    pub start: usize,
    /// Exclusive end line; `None` means "to end of file".
    pub end: Option<usize>,
}

/// Identity of a read request. `fresh` is part of the key on purpose: a forced
/// re-read must never resolve to the stub it is trying to escape.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ReadKey {
    pub path: PathBuf,
    pub range: (usize, Option<usize>),
    pub fresh: bool,
}

impl ReadKey {
    pub fn new(path: impl Into<PathBuf>, range: ReadRange, fresh: bool) -> Self {
        Self {
            path: path.into(),
            range: (range.start, range.end),
            fresh,
        }
    }

    /// The digest used for the request-key map.
    ///
    /// Range is folded in so two ranges cannot collide. `fresh` is deliberately
    /// *not*: a forced read asks the same question (same file, same range) and
    /// must find the entry so it can report `Forced` and then overwrite it with
    /// the body it just delivered. Keying on freshness would make a forced read
    /// an unrelated entry, and the stub it is meant to escape would be rebuilt
    /// from a stale turn number.
    fn digest(&self) -> String {
        key_digest(&[
            "read",
            &self.path.to_string_lossy(),
            &self.range.0.to_string(),
            &self.range.1.map(|end| end.to_string()).unwrap_or_default(),
        ])
    }
}

/// What the cache remembers about one delivered read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeliveredRead {
    pub stamp: ContentStamp,
    pub meta: super::fsprobe::FileMeta,
    /// Response byte length, for the savings metric.
    pub delivered_bytes: usize,
    /// Turns taken so far when this body was sent. `0` = before the first turn.
    pub turn: u64,
}

/// Why a lookup produced what it did. Every variant is reported so the metrics
/// file can distinguish "we saved tokens" from "we had nothing to say".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lookup {
    /// Same request, same bytes, body still in context: send the stub.
    Hit,
    /// Never seen, or the range changed.
    Miss,
    /// Seen, but the file's bytes differ.
    Changed,
    /// Seen and unchanged, but too long ago to trust.
    Expired,
    /// Seen and unchanged, but the model explicitly asked for a fresh read.
    Forced,
    /// Seen and unchanged, but the file is gone.
    Deleted,
}

impl Lookup {
    pub fn is_hit(self) -> bool {
        matches!(self, Lookup::Hit)
    }
}

/// One search result set, stamped for validation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CachedSearch {
    /// `(path, mtime_ms)` for every file the search actually read. Validation
    /// compares these against the filesystem; content is never re-read.
    pub scanned: Vec<(PathBuf, i64)>,
    /// Digest over the result lines in order, so the answer is what is cached,
    /// not just the question.
    pub result_digest: Digest,
    pub result_bytes: usize,
    pub match_count: usize,
    pub turn: u64,
}

/// A search key: the tool plus every argument that changes the answer.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct SearchKey {
    pub tool: String,
    pub pattern: String,
    pub root: PathBuf,
    pub glob: Option<String>,
    pub ignore_case: bool,
    pub literal: bool,
    pub limit: Option<u32>,
}

impl SearchKey {
    fn digest(&self) -> String {
        key_digest(&[
            &self.tool,
            &self.pattern,
            &self.root.to_string_lossy(),
            self.glob.as_deref().unwrap_or(""),
            if self.ignore_case { "i" } else { "s" },
            if self.literal { "l" } else { "r" },
            &self.limit.map(|l| l.to_string()).unwrap_or_default(),
        ])
    }
}

/// An anchor tag shown to the model for `path` at a known version.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AnchorLedger {
    pub tag: [u8; 4],
    pub stamp: ContentStamp,
    pub delivered_turn: u64,
}

/// Cumulative counters. Cheap to clone, cheap to read, and the only thing the
/// metrics layer needs from the cache.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CacheCounters {
    pub read_hits: u64,
    pub read_misses: u64,
    pub read_changed: u64,
    pub read_expired: u64,
    pub search_hits: u64,
    pub search_misses: u64,
    pub search_invalidated: u64,
    pub anchor_expansions: u64,
    pub anchor_stale: u64,
    pub bytes_saved: u64,
    pub invalidations: u64,
}

/// The cache. All state is behind one lock held only for map work: hashing and
/// file reads happen outside it.
pub struct ToolCache {
    settings: TokenEfficiencySettings,
    reads: BTreeMap<String, (ReadKey, DeliveredRead)>,
    searches: BTreeMap<String, (SearchKey, CachedSearch)>,
    anchors: BTreeMap<PathBuf, AnchorLedger>,
    turn: u64,
    counters: CacheCounters,
}

impl ToolCache {
    pub fn new(settings: TokenEfficiencySettings) -> Self {
        Self {
            settings,
            reads: BTreeMap::new(),
            searches: BTreeMap::new(),
            anchors: BTreeMap::new(),
            turn: 0,
            counters: CacheCounters::default(),
        }
    }

    pub fn settings(&self) -> &TokenEfficiencySettings {
        &self.settings
    }

    pub fn counters(&self) -> CacheCounters {
        self.counters
    }

    pub fn turn(&self) -> u64 {
        self.turn
    }

    /// Advance the conversation clock. Called once per provider turn.
    pub fn begin_turn(&mut self, turn: u64) {
        self.turn = turn;
        // Entries older than twice the window can never be reused and have
        // already had a chance to report Expired; dropping them here keeps the
        // maps bounded by session length rather than by file count.
        let window = u64::from(self.settings.dedupe_window_turns);
        let prune_window = window.saturating_mul(2).max(window + 1);
        let turn = self.turn;
        self.reads
            .retain(|_, (_, delivered)| turn.saturating_sub(delivered.turn) <= prune_window);
        self.searches
            .retain(|_, (_, cached)| turn.saturating_sub(cached.turn) <= prune_window);
    }

    // ---- read dedupe -----------------------------------------------------

    /// Decide whether a read can be answered with a stub, and record the
    /// delivery when it cannot.
    ///
    /// `delivered_bytes` is the length of the body the tool just produced; it
    /// is the number the savings metric credits. This method reads the file to
    /// stamp it but never trusts the caller for content.
    pub fn observe_read<P: FileProbe + ?Sized>(
        &mut self,
        probe: &P,
        raw_path: &Path,
        key: ReadKey,
        delivered_bytes: usize,
    ) -> (Lookup, Option<DeliveredRead>) {
        if !self.settings.read_dedupe {
            return (Lookup::Miss, None);
        }
        let path = probe.canonicalize(raw_path);
        let snapshot = probe.snapshot(&path);

        let stamp = snapshot
            .as_ref()
            .map(|snapshot: &FileSnapshot| ContentStamp::of_bytes(&snapshot.bytes));
        let meta = snapshot.as_ref().map(|snapshot| snapshot.meta);

        // The key records the canonical path so `./a.rs` and `a.rs` share one
        // entry; `delivered` records what was actually sent.
        let canonical_key = ReadKey::new(
            &path,
            ReadRange {
                start: key.range.0,
                end: key.range.1,
            },
            key.fresh,
        );
        let map_key = canonical_key.digest();

        let previous = self.reads.get(&map_key).map(|(_, delivered)| *delivered);

        let lookup = match previous {
            None => Lookup::Miss,
            Some(_) if key.fresh => Lookup::Forced,
            Some(_) if stamp.is_none() => {
                // The file is gone. Evict so a later re-creation is a real miss.
                self.reads.remove(&map_key);
                self.anchors.remove(&path);
                Lookup::Deleted
            }
            Some(previous) => {
                let unchanged = match (stamp, meta) {
                    (Some(stamp), Some(meta)) => {
                        if self.settings.trust_mtime {
                            // Opt-in fast path: same length and same mtime means
                            // "assume unchanged". Documented as unsafe on
                            // same-millisecond same-length rewrites.
                            stamp.len == previous.stamp.len
                                && meta.mtime_ms == previous.meta.mtime_ms
                        } else {
                            stamp == previous.stamp
                        }
                    }
                    _ => false,
                };
                if !unchanged {
                    Lookup::Changed
                } else if self.turn.saturating_sub(previous.turn)
                    > u64::from(self.settings.dedupe_window_turns)
                {
                    Lookup::Expired
                } else {
                    Lookup::Hit
                }
            }
        };

        match lookup {
            Lookup::Hit => {
                self.counters.read_hits += 1;
                self.counters.bytes_saved += delivered_bytes as u64;
            }
            Lookup::Changed => self.counters.read_changed += 1,
            Lookup::Expired => self.counters.read_expired += 1,
            _ => self.counters.read_misses += 1,
        }

        // On a hit the recorded delivery stays as it is; its turn is not
        // refreshed, so a body cannot be kept alive forever by re-reading it.
        if !lookup.is_hit()
            && let (Some(stamp), Some(meta)) = (stamp, meta)
        {
            self.reads.insert(
                map_key,
                (
                    canonical_key,
                    DeliveredRead {
                        stamp,
                        meta,
                        delivered_bytes,
                        turn: self.turn,
                    },
                ),
            );
            self.set_anchor(path, stamp);
        }

        (lookup, previous)
    }

    fn set_anchor(&mut self, path: PathBuf, stamp: ContentStamp) {
        let tag = stamp.digest.as_bytes()[..4]
            .try_into()
            .expect("SHA-256 is 32 bytes");
        self.anchors.insert(
            path,
            AnchorLedger {
                tag,
                stamp,
                delivered_turn: self.turn,
            },
        );
    }

    /// The anchor tag for `path` as of the last delivery, if any.
    pub fn anchor(&self, path: &Path) -> Option<AnchorLedger> {
        self.anchors.get(path).copied()
    }

    pub fn record_anchor_expansion(&mut self) {
        self.counters.anchor_expansions += 1;
    }

    pub fn record_anchor_stale(&mut self) {
        self.counters.anchor_stale += 1;
    }

    // ---- search dedupe ---------------------------------------------------

    /// Validate a previous search against the filesystem. A hit requires that
    /// every scanned file still exists at the same `mtime`, that no new file
    /// appeared in any scanned directory, and that the scanned set is small
    /// enough to be worth validating.
    pub fn observe_search<P: FileProbe + ?Sized>(
        &mut self,
        probe: &P,
        key: &SearchKey,
    ) -> Option<CachedSearch> {
        if !self.settings.search_cache {
            self.counters.search_misses += 1;
            return None;
        }
        let map_key = key.digest();
        let cached = self
            .searches
            .get(&map_key)
            .map(|(_, cached)| cached.clone())?;

        if cached.scanned.len() > self.settings.max_validate_files {
            // Too big to validate cheaply: a miss, which is always correct.
            self.counters.search_misses += 1;
            return None;
        }
        if self.turn.saturating_sub(cached.turn) > u64::from(self.settings.dedupe_window_turns) {
            self.searches.remove(&map_key);
            self.counters.search_misses += 1;
            return None;
        }
        for (path, mtime) in &cached.scanned {
            match probe.metadata(path) {
                Some(meta) if meta.mtime_ms == *mtime => {}
                // A new file in a scanned directory changes a glob's answer, so
                // directory mtimes are part of `scanned` and this catches it.
                _ => {
                    self.searches.remove(&map_key);
                    self.counters.search_invalidated += 1;
                    return None;
                }
            }
        }

        self.counters.search_hits += 1;
        self.counters.bytes_saved += cached.result_bytes as u64;
        Some(cached)
    }

    /// Whether `answer` is byte-for-byte the result that was cached. Callers use
    /// this when the cached answer is more than a summary — an `ls` listing, for
    /// instance, where handing back a stale-but-similar list is worse than
    /// re-running the tool.
    ///
    /// `CachedSearch`'s path+mtime validation answers "has anything the search
    /// read changed"; this answers "is the output I would replay exactly what I
    /// recorded". Both are cheap and neither trusts `mtime` alone.
    pub fn answer_matches(&self, cached: &CachedSearch, answer: &[u8]) -> bool {
        Digest::from_bytes(answer) == cached.result_digest
    }

    /// Record a search result so a later identical request can be validated.
    pub fn record_search(
        &mut self,
        key: SearchKey,
        scanned: Vec<(PathBuf, i64)>,
        answer: &[u8],
        matches: usize,
    ) {
        if !self.settings.search_cache {
            return;
        }
        let entry = CachedSearch {
            scanned,
            result_digest: Digest::from_bytes(answer),
            result_bytes: answer.len(),
            match_count: matches,
            turn: self.turn,
        };
        self.searches.insert(key.digest(), (key, entry));
    }

    // ---- invalidation ----------------------------------------------------

    /// A file was written through a tool. Drop every entry that described the
    /// old bytes, and forget the anchor so a stale tag is rejected.
    pub fn invalidate_path(&mut self, path: &Path) {
        self.anchors.remove(path);
        self.reads.retain(|_, (key, _)| key.path != path);
        // A search result over any directory that could contain this path is no
        // longer trustworthy; drop the ones whose scanned set mentions it.
        self.searches
            .retain(|_, (_, cached)| !cached.scanned.iter().any(|(scanned, _)| scanned == path));
        self.counters.invalidations += 1;
    }

    /// History was rewritten (compaction) or a session was resumed: the earlier
    /// bodies are no longer in the model's context, so every "see above" stub
    /// this cache could produce would be a lie.
    pub fn invalidate_all(&mut self) {
        self.reads.clear();
        self.searches.clear();
        self.anchors.clear();
        self.counters.invalidations += 1;
    }

    /// Number of live read entries. Test and diagnostics only.
    pub fn read_entries(&self) -> usize {
        self.reads.len()
    }

    pub fn search_entries(&self) -> usize {
        self.searches.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::token_efficiency::config::TokenEfficiencySettings;
    use crate::token_efficiency::fsprobe::testing::FakeFs;

    fn cache() -> ToolCache {
        ToolCache::new(TokenEfficiencySettings::default())
    }

    fn key(path: &str) -> ReadKey {
        ReadKey::new(
            path,
            ReadRange {
                start: 0,
                end: None,
            },
            false,
        )
    }

    fn read(cache: &mut ToolCache, fs: &FakeFs, path: &str, bytes: &[u8]) -> Lookup {
        let (lookup, _) = cache.observe_read(fs, Path::new(path), key(path), bytes.len());
        lookup
    }

    #[test]
    fn first_read_misses_and_second_identical_read_hits() {
        let fs = FakeFs::new();
        fs.write("/a.rs", "fn main() {}\n");
        let mut cache = cache();

        assert_eq!(
            read(&mut cache, &fs, "/a.rs", b"fn main() {}\n"),
            Lookup::Miss
        );
        assert_eq!(
            read(&mut cache, &fs, "/a.rs", b"fn main() {}\n"),
            Lookup::Hit
        );
        assert_eq!(cache.counters().read_hits, 1);
        assert_eq!(cache.counters().read_misses, 1);
    }

    #[test]
    fn hit_reports_the_savings_it_actually_made() {
        let fs = FakeFs::new();
        fs.write("/a.rs", &"x".repeat(4000));
        let mut cache = cache();
        let body = vec![b'x'; 4000];

        read(&mut cache, &fs, "/a.rs", &body);
        assert_eq!(cache.counters().bytes_saved, 0, "a miss saves nothing");
        read(&mut cache, &fs, "/a.rs", &body);
        read(&mut cache, &fs, "/a.rs", &body);
        assert_eq!(cache.counters().bytes_saved, 8000);
    }

    #[test]
    fn changed_content_is_never_a_hit() {
        let fs = FakeFs::new();
        fs.write("/a.rs", "one\n");
        let mut cache = cache();

        assert_eq!(read(&mut cache, &fs, "/a.rs", b"one\n"), Lookup::Miss);
        fs.write("/a.rs", "two\n");
        assert_eq!(read(&mut cache, &fs, "/a.rs", b"two\n"), Lookup::Changed);
        assert_eq!(cache.counters().read_changed, 1);
        // The new content is now the recorded version.
        assert_eq!(read(&mut cache, &fs, "/a.rs", b"two\n"), Lookup::Hit);
    }

    #[test]
    fn same_length_same_mtime_rewrite_is_still_detected() {
        // The adversarial case the mtime fast path would miss: identical length,
        // identical mtime, different bytes.
        let fs = FakeFs::new();
        fs.write("/a.rs", "aaaa");
        let mut cache = cache();
        read(&mut cache, &fs, "/a.rs", b"aaaa");

        fs.write_same_mtime("/a.rs", "bbbb");
        assert_eq!(read(&mut cache, &fs, "/a.rs", b"bbbb"), Lookup::Changed);
    }

    #[test]
    fn touch_without_content_change_stays_a_hit() {
        let fs = FakeFs::new();
        fs.write("/a.rs", "stable\n");
        let mut cache = cache();
        read(&mut cache, &fs, "/a.rs", b"stable\n");

        fs.touch("/a.rs");
        assert_eq!(read(&mut cache, &fs, "/a.rs", b"stable\n"), Lookup::Hit);
    }

    #[test]
    fn deleting_the_file_is_not_a_hit() {
        let fs = FakeFs::new();
        fs.write("/a.rs", "one\n");
        let mut cache = cache();
        read(&mut cache, &fs, "/a.rs", b"one\n");

        fs.remove("/a.rs");
        assert_eq!(read(&mut cache, &fs, "/a.rs", b"one\n"), Lookup::Deleted);
        // And the entry is evicted, so re-creating the file is a fresh miss.
        assert_eq!(read(&mut cache, &fs, "/a.rs", b"one\n"), Lookup::Miss);
    }

    #[test]
    fn expiry_after_the_window_forces_a_full_resend() {
        let fs = FakeFs::new();
        fs.write("/a.rs", "body\n");
        let mut cache = ToolCache::new(TokenEfficiencySettings {
            dedupe_window_turns: 2,
            ..TokenEfficiencySettings::default()
        });

        cache.begin_turn(1);
        read(&mut cache, &fs, "/a.rs", b"body\n");
        cache.begin_turn(3);
        assert_eq!(
            read(&mut cache, &fs, "/a.rs", b"body\n"),
            Lookup::Hit,
            "turns 1→3 is a distance of 2"
        );
        cache.begin_turn(4);
        assert_eq!(
            read(&mut cache, &fs, "/a.rs", b"body\n"),
            Lookup::Expired,
            "distance 3 exceeds the window of 2"
        );
    }

    #[test]
    fn a_hit_does_not_extend_the_window() {
        // Repeatedly reading the same file must not refresh the delivery turn,
        // or a body dropped by compaction could be vouched for indefinitely.
        let fs = FakeFs::new();
        fs.write("/a.rs", "body\n");
        let mut cache = ToolCache::new(TokenEfficiencySettings {
            dedupe_window_turns: 2,
            ..TokenEfficiencySettings::default()
        });

        cache.begin_turn(1);
        read(&mut cache, &fs, "/a.rs", b"body\n");
        cache.begin_turn(2);
        assert!(read(&mut cache, &fs, "/a.rs", b"body\n").is_hit());
        cache.begin_turn(3);
        assert!(
            read(&mut cache, &fs, "/a.rs", b"body\n").is_hit(),
            "still inside 1→3"
        );
        cache.begin_turn(4);
        assert_eq!(
            read(&mut cache, &fs, "/a.rs", b"body\n"),
            Lookup::Expired,
            "measured from turn 1, not turn 3"
        );
    }

    #[test]
    fn fresh_flag_always_misses() {
        let fs = FakeFs::new();
        fs.write("/a.rs", "body\n");
        let mut cache = cache();
        read(&mut cache, &fs, "/a.rs", b"body\n");

        let forced = ReadKey::new(
            "/a.rs",
            ReadRange {
                start: 0,
                end: None,
            },
            true,
        );
        let (lookup, _) = cache.observe_read(&fs, Path::new("/a.rs"), forced, 6);
        assert_eq!(lookup, Lookup::Forced);
    }

    #[test]
    fn a_forced_read_overwrites_the_entry_with_the_fresh_delivery() {
        // The forced read must not leave the old turn number behind, or the next
        // ordinary read would be stubbed with a turn the model no longer has.
        let fs = FakeFs::new();
        fs.write("/a.rs", "body\n");
        let mut cache = ToolCache::new(TokenEfficiencySettings {
            dedupe_window_turns: 8,
            ..TokenEfficiencySettings::default()
        });
        cache.begin_turn(1);
        read(&mut cache, &fs, "/a.rs", b"body\n");

        cache.begin_turn(5);
        let forced = ReadKey::new("/a.rs", ReadRange::default(), true);
        let (lookup, _) = cache.observe_read(&fs, Path::new("/a.rs"), forced, 5);
        assert_eq!(lookup, Lookup::Forced);

        // The delivery is now stamped at turn 5, so a subsequent forced read
        // reports the new turn rather than reviving turn 1.
        let (_, delivered) = cache.observe_read(
            &fs,
            Path::new("/a.rs"),
            ReadKey::new("/a.rs", ReadRange::default(), true),
            5,
        );
        assert_eq!(delivered.map(|d| d.turn), Some(5));
        assert_eq!(cache.read_entries(), 1, "the same range keeps one entry");
    }

    #[test]
    fn a_different_range_is_a_different_entry() {
        let fs = FakeFs::new();
        fs.write("/a.rs", "line1\nline2\n");
        let mut cache = cache();
        read(&mut cache, &fs, "/a.rs", b"line1\nline2\n");

        let ranged = ReadKey::new(
            "/a.rs",
            ReadRange {
                start: 1,
                end: None,
            },
            false,
        );
        let (lookup, _) = cache.observe_read(&fs, Path::new("/a.rs"), ranged, 6);
        assert_eq!(lookup, Lookup::Miss);
        assert_eq!(cache.read_entries(), 2);
    }

    #[test]
    fn aliased_paths_share_one_entry() {
        let fs = FakeFs::new();
        fs.write("/real/a.rs", "body\n");
        fs.alias("/link/a.rs", "/real/a.rs");
        let mut cache = cache();

        let first = ReadKey::new("/link/a.rs", ReadRange::default(), false);
        let (lookup, _) = cache.observe_read(&fs, Path::new("/link/a.rs"), first, 5);
        assert_eq!(lookup, Lookup::Miss);
        // Same file through the canonical path is a hit.
        assert_eq!(read(&mut cache, &fs, "/real/a.rs", b"body\n"), Lookup::Hit);
    }

    #[test]
    fn write_invalidation_forces_a_miss() {
        let fs = FakeFs::new();
        fs.write("/a.rs", "one\n");
        let mut cache = cache();
        read(&mut cache, &fs, "/a.rs", b"one\n");
        assert_eq!(read(&mut cache, &fs, "/a.rs", b"one\n"), Lookup::Hit);

        cache.invalidate_path(Path::new("/a.rs"));
        fs.write("/a.rs", "one\n");
        assert_eq!(read(&mut cache, &fs, "/a.rs", b"one\n"), Lookup::Miss);
        assert_eq!(cache.read_entries(), 1);
    }

    #[test]
    fn invalidate_all_clears_every_ledger() {
        let fs = FakeFs::new();
        fs.write("/a.rs", "one\n");
        fs.write("/b.rs", "two\n");
        let mut cache = cache();
        read(&mut cache, &fs, "/a.rs", b"one\n");
        read(&mut cache, &fs, "/b.rs", b"two\n");
        assert_eq!(cache.read_entries(), 2);

        cache.invalidate_all();
        assert_eq!(cache.read_entries(), 0);
        assert_eq!(cache.search_entries(), 0);
        assert!(cache.anchor(Path::new("/a.rs")).is_none());
        assert_eq!(read(&mut cache, &fs, "/a.rs", b"one\n"), Lookup::Miss);
    }

    #[test]
    fn disabling_read_dedupe_makes_every_read_a_miss() {
        let fs = FakeFs::new();
        fs.write("/a.rs", "body\n");
        let mut cache = ToolCache::new(TokenEfficiencySettings {
            read_dedupe: false,
            ..TokenEfficiencySettings::default()
        });
        assert_eq!(read(&mut cache, &fs, "/a.rs", b"body\n"), Lookup::Miss);
        assert_eq!(read(&mut cache, &fs, "/a.rs", b"body\n"), Lookup::Miss);
        assert_eq!(cache.read_entries(), 0, "nothing is recorded while off");
    }

    #[test]
    fn trust_mtime_trades_exactness_for_a_stat_when_enabled() {
        let fs = FakeFs::new();
        fs.write("/a.rs", "aaaa");
        let mut cache = ToolCache::new(TokenEfficiencySettings {
            trust_mtime: true,
            ..TokenEfficiencySettings::default()
        });
        read(&mut cache, &fs, "/a.rs", b"aaaa");

        // Same length, same mtime: the opt-in path reports a hit even though the
        // bytes differ. This is the documented cost of the switch.
        fs.write_same_mtime("/a.rs", "bbbb");
        assert_eq!(read(&mut cache, &fs, "/a.rs", b"bbbb"), Lookup::Hit);

        // A length change is still caught, because `len` is compared.
        fs.write_same_mtime("/a.rs", "bbbbb");
        assert_eq!(read(&mut cache, &fs, "/a.rs", b"bbbbb"), Lookup::Changed);
    }

    #[test]
    fn anchor_tag_tracks_the_delivered_version() {
        let fs = FakeFs::new();
        fs.write("/a.rs", "one\n");
        let mut cache = cache();
        read(&mut cache, &fs, "/a.rs", b"one\n");
        let first = cache.anchor(Path::new("/a.rs")).expect("anchor recorded");

        fs.write("/a.rs", "two\n");
        read(&mut cache, &fs, "/a.rs", b"two\n");
        let second = cache.anchor(Path::new("/a.rs")).expect("anchor updated");
        assert_ne!(
            first.stamp, second.stamp,
            "a new version must not keep the old tag"
        );
    }

    // ---- search ----------------------------------------------------------

    fn search_key(pattern: &str) -> SearchKey {
        SearchKey {
            tool: "grep".into(),
            pattern: pattern.into(),
            root: PathBuf::from("/work"),
            glob: Some("*.rs".into()),
            ignore_case: false,
            literal: false,
            limit: Some(100),
        }
    }

    #[test]
    fn search_hits_only_while_every_scanned_file_is_untouched() {
        let fs = FakeFs::new();
        fs.write("/work/a.rs", "one");
        fs.write("/work/b.rs", "two");
        let mut cache = cache();
        let scanned = vec![
            (PathBuf::from("/work/a.rs"), fs.mtime_of("/work/a.rs")),
            (PathBuf::from("/work/b.rs"), fs.mtime_of("/work/b.rs")),
        ];
        cache.record_search(search_key("fn "), scanned, b"a.rs:1:fn one", 1);

        assert!(cache.observe_search(&fs, &search_key("fn ")).is_some());

        fs.write("/work/b.rs", "two modified");
        assert!(cache.observe_search(&fs, &search_key("fn ")).is_none());
        assert_eq!(cache.counters().search_invalidated, 1);
    }

    #[test]
    fn a_new_file_in_a_scanned_directory_invalidates_a_glob_search() {
        // Directory mtimes stand in for "the file set changed".
        let fs = FakeFs::new();
        fs.write("/work/a.rs", "one");
        let dir_mtime = fs.mtime_of("/work/a.rs");
        let mut cache = cache();
        cache.record_search(
            search_key("one"),
            vec![(PathBuf::from("/work"), dir_mtime)],
            b"a.rs:1:one",
            1,
        );
        assert!(cache.observe_search(&fs, &search_key("one")).is_some());

        // A new file makes the directory's mtime move.
        fs.write("/work/c.rs", "three");
        assert!(cache.observe_search(&fs, &search_key("one")).is_none());
    }

    #[test]
    fn different_search_arguments_do_not_share_an_entry() {
        let fs = FakeFs::new();
        fs.write("/work/a.rs", "one");
        let mut cache = cache();
        let scanned = vec![(PathBuf::from("/work/a.rs"), fs.mtime_of("/work/a.rs"))];
        cache.record_search(search_key("one"), scanned.clone(), b"hit", 1);

        assert!(cache.observe_search(&fs, &search_key("two")).is_none());
        let mut insensitive = search_key("one");
        insensitive.ignore_case = true;
        assert!(cache.observe_search(&fs, &insensitive).is_none());
        let mut literal = search_key("one");
        literal.literal = true;
        assert!(cache.observe_search(&fs, &literal).is_none());

        // The original still hits, so the misses above were key mismatch, not
        // collateral invalidation.
        assert!(cache.observe_search(&fs, &search_key("one")).is_some());
    }

    #[test]
    fn oversized_scanned_sets_are_not_validated() {
        let fs = FakeFs::new();
        fs.write("/work/a.rs", "one");
        let mut cache = ToolCache::new(TokenEfficiencySettings {
            max_validate_files: 1,
            ..TokenEfficiencySettings::default()
        });
        let scanned = vec![
            (PathBuf::from("/work/a.rs"), fs.mtime_of("/work/a.rs")),
            (PathBuf::from("/work/b.rs"), fs.mtime_of("/work/b.rs")),
        ];
        cache.record_search(search_key("one"), scanned, b"hit", 1);
        assert!(
            cache.observe_search(&fs, &search_key("one")).is_none(),
            "validating more files than the cap is not worth the I/O"
        );
    }

    #[test]
    fn write_invalidation_drops_searches_that_scanned_the_path() {
        let fs = FakeFs::new();
        fs.write("/work/a.rs", "one");
        let mut cache = cache();
        let scanned = vec![(PathBuf::from("/work/a.rs"), fs.mtime_of("/work/a.rs"))];
        cache.record_search(search_key("one"), scanned, b"hit", 1);

        cache.invalidate_path(Path::new("/work/a.rs"));
        assert!(cache.observe_search(&fs, &search_key("one")).is_none());
    }

    #[test]
    fn search_cache_off_never_hits() {
        let fs = FakeFs::new();
        fs.write("/work/a.rs", "one");
        let mut cache = ToolCache::new(TokenEfficiencySettings {
            search_cache: false,
            ..TokenEfficiencySettings::default()
        });
        cache.record_search(
            search_key("one"),
            vec![(PathBuf::from("/work/a.rs"), 1)],
            b"hit",
            1,
        );
        assert!(cache.observe_search(&fs, &search_key("one")).is_none());
        assert_eq!(cache.search_entries(), 0);
    }

    #[test]
    fn expired_searches_are_dropped() {
        let fs = FakeFs::new();
        fs.write("/work/a.rs", "one");
        let mut cache = ToolCache::new(TokenEfficiencySettings {
            dedupe_window_turns: 1,
            ..TokenEfficiencySettings::default()
        });
        let scanned = vec![(PathBuf::from("/work/a.rs"), fs.mtime_of("/work/a.rs"))];
        cache.begin_turn(1);
        cache.record_search(search_key("one"), scanned, b"hit", 1);
        cache.begin_turn(5);
        assert!(cache.observe_search(&fs, &search_key("one")).is_none());
    }

    #[test]
    fn content_stamp_counts_trailing_newline_like_read_does() {
        assert_eq!(ContentStamp::of_bytes(b"").lines, 1);
        assert_eq!(ContentStamp::of_bytes(b"a").lines, 1);
        assert_eq!(ContentStamp::of_bytes(b"a\n").lines, 2);
        assert_eq!(ContentStamp::of_bytes(b"a\nb").lines, 2);
    }
}
