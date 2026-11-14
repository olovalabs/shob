use crate::prebuilt_extensions::{body_limited, check_hash};
use percent_encoding::percent_decode_str;
use reqwest::Url;

const CDN_ENV: &str = "RPI_CDN_BASE_URL";
const CLI_REPOSITORY: &str = "https://github.com/bigfish1913/pi-rust";
const PACKAGE_REPOSITORY: &str = "https://github.com/pi-rust/rpi-package";

/// Resolve and validate the optional CDN base. The CDN is only a transport;
/// manifests and all release identity fields still come from GitHub.
///
/// Precedence is explicit CLI (`--cdn`), environment (`RPI_CDN_BASE_URL`), then
/// global `settings.json` (`cdnBaseUrl`). Settings are a convenience default;
/// they never override an explicit invocation or environment configuration.
pub(crate) fn resolve_base(cli_value: Option<&str>) -> Result<Option<Url>, String> {
    let configured_setting = crate::settings::load_settings()
        .ok()
        .and_then(|settings| settings.cdn_base_url);
    let value = cli_value
        .map(str::to_owned)
        .or_else(|| std::env::var(CDN_ENV).ok())
        .or(configured_setting)
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    let Some(value) = value else { return Ok(None) };
    let mut url = Url::parse(&value).map_err(|_| "invalid CDN base URL".to_string())?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("CDN base URL must be HTTPS without credentials, query, or fragment".into());
    }
    // Rebuild the base path from safe, non-empty segments. This normalizes
    // repeated/trailing slashes while preventing a base path from escaping the
    // fixed CDN namespace below.
    let segments = url
        .path()
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(|segment| {
            percent_decode_str(segment)
                .decode_utf8()
                .map(|value| value.into_owned())
                .map_err(|_| "invalid encoded CDN base path".to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    if segments.iter().any(|segment| {
        segment == "."
            || segment == ".."
            || segment.contains(['/', '\\'])
            || segment.chars().any(|ch| ch.is_control())
    }) {
        return Err("CDN base URL contains an unsafe path".into());
    }
    {
        let mut path = url
            .path_segments_mut()
            .map_err(|_| "invalid CDN base URL")?;
        path.clear();
        for segment in &segments {
            path.push(segment);
        }
    }
    Ok(Some(url))
}

fn repository_prefix(repository: &str) -> Option<&'static str> {
    match repository {
        CLI_REPOSITORY => Some("rpi"),
        PACKAGE_REPOSITORY => Some("rpi-package"),
        _ => None,
    }
}

fn safe_path_segment(value: &str, label: &str) -> Result<String, String> {
    let decoded = percent_decode_str(value)
        .decode_utf8()
        .map_err(|_| format!("invalid encoded {label}"))?;
    if decoded.is_empty()
        || decoded == "."
        || decoded == ".."
        || decoded.contains(['/', '\\'])
        || decoded.chars().any(|ch| ch.is_control())
        || !decoded
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'+'))
    {
        return Err(format!("{label} is not a safe URL path segment"));
    }
    Ok(decoded.into_owned())
}

fn safe_archive_filename(value: &str) -> Result<String, String> {
    let url = Url::parse(value).map_err(|_| "invalid archive URL")?;
    let filename = url
        .path_segments()
        .ok_or("archive URL has no path")?
        .last()
        .ok_or("archive URL has no filename")?;
    safe_path_segment(filename, "archive filename")
}

/// Construct the CDN URL only for the two official repositories and only from
/// the already validated GitHub release tag and archive URL filename.
pub(crate) fn archive_url(
    base: &Url,
    repository: &str,
    tag: &str,
    github_archive_url: &str,
) -> Result<Option<Url>, String> {
    let Some(prefix) = repository_prefix(repository) else {
        return Ok(None);
    };
    let tag = safe_path_segment(tag, "release tag")?;
    let filename = safe_archive_filename(github_archive_url)?;
    let mut url = base.clone();
    {
        let mut path = url
            .path_segments_mut()
            .map_err(|_| "invalid CDN base URL")?;
        let already_scoped = base
            .path_segments()
            .and_then(|segments| segments.filter(|segment| !segment.is_empty()).next_back())
            == Some(prefix);
        if !already_scoped {
            path.push(prefix);
        }
        path.push("releases").push(&tag).push(&filename);
    }
    Ok(Some(url))
}

async fn download_one(
    client: &reqwest::Client,
    url: &str,
    limit: u64,
    expected_hash: &str,
    label: &str,
) -> Result<Vec<u8>, String> {
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|error| format!("{label} request failed: {error}"))?;
    if !response.status().is_success() {
        return Err(format!("{label} request returned {}", response.status()));
    }
    let bytes = body_limited(response, limit).await?;
    if bytes.len() as u64 != limit {
        return Err(format!(
            "{label} size {} does not match expected size {limit}",
            bytes.len()
        ));
    }
    check_hash(&bytes, expected_hash)?;
    Ok(bytes)
}

/// Try the untrusted CDN first, then always fall back to the canonical GitHub
/// release asset on any CDN transport, status, size, or hash failure.
pub(crate) async fn download_with_fallback(
    client: &reqwest::Client,
    cdn_url: Option<&Url>,
    github_url: &str,
    limit: u64,
    expected_hash: &str,
    label: &str,
) -> Result<Vec<u8>, String> {
    if let Some(cdn_url) = cdn_url {
        if let Ok(bytes) = download_one(
            client,
            cdn_url.as_str(),
            limit,
            expected_hash,
            "CDN archive",
        )
        .await
        {
            return Ok(bytes);
        }
    }
    download_one(client, github_url, limit, expected_hash, label).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_and_normalizes_base_and_builds_official_paths() {
        let base = resolve_base(Some("https://assets.example.test///mirror//"))
            .unwrap()
            .unwrap();
        assert_eq!(base.as_str(), "https://assets.example.test/mirror");
        let url = archive_url(
            &base,
            CLI_REPOSITORY,
            "v1.2.3",
            "https://github.com/bigfish1913/pi-rust/releases/download/v1.2.3/rpi.zip",
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            url.as_str(),
            "https://assets.example.test/mirror/rpi/releases/v1.2.3/rpi.zip"
        );
        let scoped = resolve_base(Some("https://assets.example.test/rpi"))
            .unwrap()
            .unwrap();
        let scoped_url = archive_url(
            &scoped,
            CLI_REPOSITORY,
            "v1.2.3",
            "https://github.com/bigfish1913/pi-rust/releases/download/v1.2.3/rpi.zip",
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            scoped_url.as_str(),
            "https://assets.example.test/rpi/releases/v1.2.3/rpi.zip"
        );
    }

    #[tokio::test]
    async fn invalid_cdn_archive_falls_back_to_github() {
        use crate::prebuilt_extensions::hash;
        use std::io::{Read, Write};
        let good = b"canonical archive".to_vec();
        let good_len = good.len() as u64;
        let bad = b"stale CDN archive".to_vec();
        let expected = hash(&good);
        let server_good = good.clone();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            for body in [&bad, &server_good] {
                let (mut socket, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                let mut byte = [0u8; 1];
                while !request.ends_with(b"\r\n\r\n") {
                    socket.read_exact(&mut byte).unwrap();
                    request.push(byte[0]);
                }
                write!(
                    socket,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .unwrap();
                socket.write_all(body).unwrap();
            }
        });
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let cdn = Url::parse(&format!("http://{address}/cdn")).unwrap();
        let github = format!("http://{address}/github");
        let result =
            download_with_fallback(&client, Some(&cdn), &github, good_len, &expected, "archive")
                .await
                .unwrap();
        assert_eq!(result, good);
        server.join().unwrap();
    }

    #[test]
    fn rejects_unsafe_bases_and_filenames_and_ignores_other_repositories() {
        for value in [
            "http://assets.example.test",
            "https://user:pass@assets.example.test",
            "https://assets.example.test/x?y=1",
            "https://assets.example.test/x%2Fy",
        ] {
            assert!(resolve_base(Some(value)).is_err(), "{value}");
        }
        let base = resolve_base(Some("https://assets.example.test"))
            .unwrap()
            .unwrap();
        assert!(archive_url(
            &base,
            CLI_REPOSITORY,
            "v1",
            "https://github.com/a/b/releases/download/v1/a%2Fb.zip"
        )
        .is_err());
        assert!(archive_url(
            &base,
            "https://github.com/other/repo",
            "v1",
            "https://github.com/other/repo/releases/download/v1/a.zip"
        )
        .unwrap()
        .is_none());
    }
}
