//! Changelog fetching and parsing.
//!
//! Port of the upstream changelog utility.
//! Bundled release notes power startup notices and `/changelog` without a
//! source checkout or network request. Optional remote fetching remains
//! available to consumers, with the same versioned-entry parser.
//!
//! The remote URL is overridable with `RPI_CHANGELOG_URL` so a fork can point at
//! its own changelog without a rebuild.

use serde::{Deserialize, Serialize};

const BUNDLED_CHANGELOG: &str = include_str!("../embedded-docs/changelog.md");

/// One released version's changelog section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangelogEntry {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    /// The section body (everything under the heading, trimmed).
    pub content: String,
}

/// Default remote changelog URL. Overridable via `RPI_CHANGELOG_URL`.
pub const DEFAULT_CHANGELOG_URL: &str =
    "https://raw.githubusercontent.com/bigfish1913/pi-rust/main/CHANGELOG.md";

/// The remote changelog URL to use, honoring `RPI_CHANGELOG_URL`.
pub fn changelog_url() -> String {
    std::env::var("RPI_CHANGELOG_URL").unwrap_or_else(|_| DEFAULT_CHANGELOG_URL.to_string())
}

/// Parse a semver-ish heading like `## 1.2.3` / `# 1.2.3` / `## v1.2.3`.
fn parse_heading(line: &str) -> Option<(u64, u64, u64)> {
    let trimmed = line.trim_start_matches('#').trim();
    let version = trimmed.split_whitespace().next()?;
    // release-plz emits linked headings: `[0.5.1](https://.../compare/...)`.
    // Extract the label before parsing so the URL cannot turn the patch into 0.
    let version = if let Some(label) = version.strip_prefix('[') {
        label.split_once(']')?.0
    } else {
        version
    };
    let version = version.strip_prefix('v').unwrap_or(version);
    let mut parts = version.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().unwrap_or(0);
    let patch = parts
        .next()
        .map(|p| p.split(['-', '+']).next().unwrap_or(p))
        .unwrap_or("0")
        .parse()
        .unwrap_or(0);
    Some((major, minor, patch))
}

/// Split a changelog into versioned entries (native `parseChangelog`).
pub fn parse_changelog(text: &str) -> Vec<ChangelogEntry> {
    let mut entries: Vec<ChangelogEntry> = Vec::new();
    let mut current: Option<ChangelogEntry> = None;

    for line in text.lines() {
        let is_heading = line.trim_start().starts_with('#');
        if is_heading {
            if let Some((major, minor, patch)) = parse_heading(line) {
                if let Some(entry) = current.take() {
                    entries.push(finalize(entry));
                }
                current = Some(ChangelogEntry {
                    major,
                    minor,
                    patch,
                    content: String::new(),
                });
                continue;
            }
        }
        if let Some(entry) = current.as_mut() {
            entry.content.push_str(line);
            entry.content.push('\n');
        }
    }
    if let Some(entry) = current.take() {
        entries.push(finalize(entry));
    }
    entries
}

fn finalize(mut entry: ChangelogEntry) -> ChangelogEntry {
    entry.content = entry.content.trim().to_string();
    entry
}

/// Release notes shipped with this binary, usable without network or checkout.
pub fn bundled_entries() -> Vec<ChangelogEntry> {
    let mut entries = parse_changelog(BUNDLED_CHANGELOG);
    entries.sort_by_key(|entry| std::cmp::Reverse((entry.major, entry.minor, entry.patch)));
    entries
}

pub fn format_entries(entries: &[ChangelogEntry]) -> String {
    entries
        .iter()
        .map(|entry| {
            format!(
                "## {}.{}.{}\n\n{}",
                entry.major, entry.minor, entry.patch, entry.content
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Default command view: the latest shipped release at or below this binary's version.
pub fn latest_release(entries: &[ChangelogEntry], current_version: &str) -> Vec<ChangelogEntry> {
    let Ok(current) = semver::Version::parse(current_version) else {
        return Vec::new();
    };
    entries
        .iter()
        .filter(|entry| {
            (entry.major, entry.minor, entry.patch) <= (current.major, current.minor, current.patch)
        })
        .max_by_key(|entry| (entry.major, entry.minor, entry.patch))
        .cloned()
        .into_iter()
        .collect()
}

/// Migrate only legacy markers that name a release in rpi's bundled history.
/// A foreign version (for example Pi 0.85.1) must not suppress rpi updates.
pub fn rpi_baseline<'a>(
    entries: &[ChangelogEntry],
    dedicated: Option<&'a str>,
    legacy: Option<&'a str>,
) -> Option<&'a str> {
    dedicated.or_else(|| {
        legacy.filter(|version| {
            semver::Version::parse(version).is_ok_and(|version| {
                entries.iter().any(|entry| {
                    (entry.major, entry.minor, entry.patch)
                        == (version.major, version.minor, version.patch)
                })
            })
        })
    })
}

#[derive(Debug, Default)]
pub struct StartupChangelog {
    pub markdown: Option<String>,
    pub version_to_record: Option<String>,
}

/// Pi parity: first install records a baseline; upgrades show unseen releases;
/// existing transcripts and downgrades do not consume or regress that baseline.
pub fn startup_changelog(
    entries: &[ChangelogEntry],
    last_version: Option<&str>,
    current_version: &str,
    has_messages: bool,
) -> StartupChangelog {
    startup_changelog_for_profile(entries, last_version, current_version, has_messages, false)
}

/// Existing profiles without an rpi baseline see the current release once;
/// genuinely fresh installs record a baseline without replaying old history.
pub fn startup_changelog_for_profile(
    entries: &[ChangelogEntry],
    last_version: Option<&str>,
    current_version: &str,
    has_messages: bool,
    existing_profile: bool,
) -> StartupChangelog {
    if has_messages {
        return StartupChangelog::default();
    }
    let Ok(current) = semver::Version::parse(current_version) else {
        return StartupChangelog::default();
    };
    let Some(last) = last_version.and_then(|version| semver::Version::parse(version).ok()) else {
        return StartupChangelog {
            markdown: existing_profile
                .then(|| {
                    let current_notes: Vec<_> = entries
                        .iter()
                        .filter(|entry| {
                            (entry.major, entry.minor, entry.patch)
                                == (current.major, current.minor, current.patch)
                        })
                        .cloned()
                        .collect();
                    format_entries(&current_notes)
                })
                .filter(|markdown| !markdown.is_empty()),
            version_to_record: Some(current_version.into()),
        };
    };
    if last >= current {
        return StartupChangelog::default();
    }
    let last = (last.major, last.minor, last.patch);
    let current = (current.major, current.minor, current.patch);
    let mut unseen: Vec<_> = entries
        .iter()
        .filter(|entry| {
            let version = (entry.major, entry.minor, entry.patch);
            version > last && version <= current
        })
        .cloned()
        .collect();
    unseen.sort_by_key(|entry| std::cmp::Reverse((entry.major, entry.minor, entry.patch)));
    StartupChangelog {
        markdown: (!unseen.is_empty()).then(|| format_entries(&unseen)),
        version_to_record: Some(current_version.into()),
    }
}

/// Fetch the changelog body. `allow_network == false` returns `Ok(None)`.
pub async fn fetch_changelog(allow_network: bool) -> Result<Option<String>, String> {
    if !allow_network {
        return Ok(None);
    }
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .user_agent(format!("rpi/{}", crate::VERSION))
        .build()
        .map_err(|e| e.to_string())?;
    let response = client
        .get(changelog_url())
        .header("accept", "text/plain")
        .send()
        .await
        .map_err(|e| format!("changelog request failed: {e}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "changelog request failed: {}",
            response.status().as_u16()
        ));
    }
    let text = response.text().await.map_err(|e| e.to_string())?;
    Ok(Some(text))
}

/// Fetch and parse the changelog, newest first. Best-effort: a network failure
/// yields an empty list rather than an error.
pub async fn load_entries(allow_network: bool) -> Vec<ChangelogEntry> {
    match fetch_changelog(allow_network).await {
        Ok(Some(text)) => parse_changelog(&text),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linked_release_headings_preserve_the_patch_version() {
        let entries = parse_changelog(
            "## [Unreleased]\n\n## [0.5.1](https://example.test/compare/v0.5.0...v0.5.1) - 2026-10-18\n\n- fixed\n\n## [v0.5.0](https://example.test/old)\n\n- previous",
        );
        assert_eq!(entries.len(), 2);
        assert_eq!(
            (entries[0].major, entries[0].minor, entries[0].patch),
            (0, 5, 1)
        );
        assert_eq!(
            (entries[1].major, entries[1].minor, entries[1].patch),
            (0, 5, 0)
        );
        let notice = startup_changelog_for_profile(&entries, None, "0.5.1", false, true);
        let markdown = notice.markdown.unwrap();
        assert!(markdown.contains("## 0.5.1"));
        assert!(!markdown.contains("## 0.5.0"));
    }

    #[test]
    fn parses_versions() {
        let text = "\
# Changelog

## 1.2.3
- fixed a thing

## 0.9.0
- initial

## v2.0.0-beta.1
- future
";
        let entries = parse_changelog(text);
        assert_eq!(entries.len(), 3);
        assert_eq!(
            (entries[0].major, entries[0].minor, entries[0].patch),
            (1, 2, 3)
        );
        assert!(entries[0].content.contains("fixed a thing"));
        assert_eq!(
            (entries[2].major, entries[2].minor, entries[2].patch),
            (2, 0, 0)
        );
    }

    #[test]
    fn ignores_non_version_headings() {
        let text = "# Changelog\n\n## Unreleased\n- wip\n\n## 1.0.0\n- out\n";
        let entries = parse_changelog(text);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].content, "- out");
    }

    #[test]
    fn parses_keep_a_changelog_headings_and_bundled_notes() {
        let entries = parse_changelog("## [Unreleased]\n- next\n\n## [0.3.17] - 2026-10-08\n### Fixed\n- reply spacing\n\n## [0.3.16] - 2026-10-06\n- previous");
        assert_eq!(entries.len(), 2);
        assert_eq!(
            (entries[0].major, entries[0].minor, entries[0].patch),
            (0, 3, 17)
        );
        assert!(entries[0].content.contains("reply spacing"));
        assert!(!entries[0].content.contains("previous"));
        assert!(!bundled_entries().is_empty());
    }

    #[test]
    fn startup_notes_show_once_only_for_new_sessions_after_upgrade() {
        let entries = parse_changelog(
            "## [0.3.18]\n- future\n## [0.3.17]\n- current\n## [0.3.16]\n- previous",
        );
        let fresh = startup_changelog(&entries, None, "0.3.17", false);
        assert!(fresh.markdown.is_none());
        assert_eq!(fresh.version_to_record.as_deref(), Some("0.3.17"));
        let upgrade = startup_changelog(&entries, Some("0.3.15"), "0.3.17", false);
        let markdown = upgrade.markdown.unwrap();
        assert!(markdown.contains("current"));
        assert!(markdown.contains("previous"));
        assert!(!markdown.contains("future"));
        let again = startup_changelog(
            &entries,
            upgrade.version_to_record.as_deref(),
            "0.3.17",
            false,
        );
        assert!(again.markdown.is_none());
        assert!(again.version_to_record.is_none());
        let resumed = startup_changelog(&entries, Some("0.3.16"), "0.3.17", true);
        assert!(resumed.markdown.is_none());
        assert!(resumed.version_to_record.is_none());
        assert!(startup_changelog(&entries, Some("0.3.16"), "0.3.17", false)
            .markdown
            .is_some());
        let downgraded = startup_changelog(&entries, Some("0.3.18"), "0.3.17", false);
        assert!(downgraded.markdown.is_none());
        assert!(downgraded.version_to_record.is_none());
    }

    #[test]
    fn foreign_baseline_does_not_hide_rpi_notes_and_the_new_marker_is_independent() {
        let entries = parse_changelog("## [0.3.19]\n- current\n## [0.3.18]\n- previous");
        assert_eq!(rpi_baseline(&entries, None, Some("0.85.1")), None);
        let notice = startup_changelog_for_profile(
            &entries,
            rpi_baseline(&entries, None, Some("0.85.1")),
            "0.3.19",
            false,
            true,
        );
        assert_eq!(notice.version_to_record.as_deref(), Some("0.3.19"));
        let markdown = notice.markdown.unwrap();
        assert!(markdown.contains("current"));
        assert!(!markdown.contains("previous"));
        let baseline = rpi_baseline(&entries, Some("0.3.19"), Some("0.86.0"));
        let again = startup_changelog_for_profile(&entries, baseline, "0.3.19", false, true);
        assert!(again.markdown.is_none());
        assert!(again.version_to_record.is_none());
        assert_eq!(rpi_baseline(&entries, None, Some("0.3.18")), Some("0.3.18"));
        assert_eq!(
            rpi_baseline(&entries, Some("0.3.20"), Some("0.85.1")),
            Some("0.3.20")
        );
        assert!(
            startup_changelog_for_profile(&entries, Some("0.3.20"), "0.3.19", false, true)
                .version_to_record
                .is_none()
        );
    }

    #[test]
    fn existing_profile_without_marker_defers_on_resume_and_shows_only_current_notes_once() {
        let entries = parse_changelog("## [0.3.19]\n- current\n## [0.3.18]\n- previous");
        let resumed = startup_changelog_for_profile(&entries, None, "0.3.19", true, true);
        assert!(resumed.markdown.is_none());
        assert!(resumed.version_to_record.is_none());
        let notice = startup_changelog_for_profile(&entries, None, "0.3.19", false, true);
        assert!(notice.markdown.unwrap().contains("current"));
        let baseline = notice.version_to_record;
        assert!(startup_changelog_for_profile(
            &entries,
            baseline.as_deref(),
            "0.3.19",
            false,
            true
        )
        .markdown
        .is_none());
        let fresh = startup_changelog_for_profile(&entries, None, "0.3.19", false, false);
        assert!(fresh.markdown.is_none());
        assert_eq!(fresh.version_to_record.as_deref(), Some("0.3.19"));
    }

    #[test]
    fn command_selects_one_latest_release_and_never_future_notes() {
        let entries = parse_changelog(
            "## [0.3.20]\n- future\n## [0.3.19]\n- current\n## [0.3.18]\n- previous",
        );
        let latest = latest_release(&entries, "0.3.19");
        assert_eq!(latest.len(), 1);
        assert_eq!(latest[0].content, "- current");
        assert_eq!(latest_release(&entries, "0.3.21")[0].content, "- future");
    }

    #[test]
    fn offline_is_empty() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let entries = rt.block_on(load_entries(false));
        assert!(entries.is_empty());
    }
}
