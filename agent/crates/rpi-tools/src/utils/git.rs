//! Git URL parsing and manipulation utilities.
//!
//! Provides functionality to parse, validate, and manipulate Git repository URLs
//! in various formats (HTTPS, SSH, git://, etc.).
//!
//! # Examples
//!
//! ```rust
//! use rpi_tools::utils::git::GitUrl;
//!
//! let url = GitUrl::parse("https://github.com/user/repo.git").unwrap();
//! assert_eq!(url.host(), "github.com");
//! assert_eq!(url.owner(), "user");
//! assert_eq!(url.repo(), "repo");
//! ```

use std::fmt;

/// Represents a parsed Git URL with its components.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitUrl {
    /// The protocol/scheme (https, ssh, git, etc.)
    scheme: String,
    /// The host (e.g., github.com)
    host: String,
    /// Optional port number
    port: Option<u16>,
    /// The repository owner/namespace
    owner: String,
    /// The repository name
    repo: String,
    /// Optional branch or tag
    reference: Option<String>,
    /// Optional subdirectory path
    path: Option<String>,
}

impl GitUrl {
    /// Parse a Git URL string into a `GitUrl` structure.
    ///
    /// Supports various formats:
    /// - HTTPS: `https://github.com/user/repo.git`
    /// - SSH: `git@github.com:user/repo.git`
    /// - Git protocol: `git://github.com/user/repo.git`
    /// - With branch: `https://github.com/user/repo.git#branch`
    /// - With subdirectory: `https://github.com/user/repo.git#path=subdir`
    ///
    /// # Errors
    ///
    /// Returns an error if the URL cannot be parsed or is missing required components.
    pub fn parse(url: &str) -> Result<Self, GitUrlError> {
        let url = url.trim();
        if url.is_empty() {
            return Err(GitUrlError::EmptyUrl);
        }

        // Check for reference (branch/tag) suffix
        let (url, reference) = if let Some(hash_pos) = url.find('#') {
            let reference = url[hash_pos + 1..].to_string();
            (&url[..hash_pos], Some(reference))
        } else {
            (url, None)
        };

        // Parse SSH format: git@host:owner/repo
        if url.starts_with("git@") {
            return Self::parse_ssh(url, reference);
        }

        // Parse URL format: scheme://host/owner/repo
        let scheme_end = url.find("://").ok_or(GitUrlError::InvalidFormat)?;
        let scheme = url[..scheme_end].to_string();
        let rest = &url[scheme_end + 3..];

        // Split host and path
        let slash_pos = rest.find('/').ok_or(GitUrlError::MissingPath)?;
        let host_part = &rest[..slash_pos];
        let path_part = &rest[slash_pos + 1..];

        // Parse host and optional port
        let (host, port) = if let Some(colon_pos) = host_part.find(':') {
            let host = host_part[..colon_pos].to_string();
            let port_str = &host_part[colon_pos + 1..];
            let port = port_str
                .parse::<u16>()
                .map_err(|_| GitUrlError::InvalidPort)?;
            (host, Some(port))
        } else {
            (host_part.to_string(), None)
        };

        // Remove .git suffix if present
        let path_part = if path_part.ends_with(".git") {
            &path_part[..path_part.len() - 4]
        } else {
            path_part
        };

        // Split owner and repo
        let parts: Vec<&str> = path_part.split('/').collect();
        if parts.len() < 2 {
            return Err(GitUrlError::MissingRepo);
        }

        let owner = parts[0].to_string();
        let repo = parts[1].to_string();

        // Check for subdirectory path
        let path = if parts.len() > 2 {
            Some(parts[2..].join("/"))
        } else {
            None
        };

        Ok(GitUrl {
            scheme,
            host,
            port,
            owner,
            repo,
            reference,
            path,
        })
    }

    /// Parse SSH format Git URL.
    fn parse_ssh(url: &str, reference: Option<String>) -> Result<Self, GitUrlError> {
        // Format: git@host:owner/repo
        let rest = &url[4..]; // Skip "git@"
        let colon_pos = rest.find(':').ok_or(GitUrlError::InvalidFormat)?;
        let host = rest[..colon_pos].to_string();
        let path_part = &rest[colon_pos + 1..];

        // Remove .git suffix if present
        let path_part = if path_part.ends_with(".git") {
            &path_part[..path_part.len() - 4]
        } else {
            path_part
        };

        // Split owner and repo
        let parts: Vec<&str> = path_part.split('/').collect();
        if parts.len() < 2 {
            return Err(GitUrlError::MissingRepo);
        }

        let owner = parts[0].to_string();
        let repo = parts[1].to_string();

        // Check for subdirectory path
        let path = if parts.len() > 2 {
            Some(parts[2..].join("/"))
        } else {
            None
        };

        Ok(GitUrl {
            scheme: "ssh".to_string(),
            host,
            port: None,
            owner,
            repo,
            reference,
            path,
        })
    }

    /// Get the protocol scheme (https, ssh, git, etc.).
    pub fn scheme(&self) -> &str {
        &self.scheme
    }

    /// Get the host (e.g., github.com).
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Get the optional port number.
    pub fn port(&self) -> Option<u16> {
        self.port
    }

    /// Get the repository owner/namespace.
    pub fn owner(&self) -> &str {
        &self.owner
    }

    /// Get the repository name.
    pub fn repo(&self) -> &str {
        &self.repo
    }

    /// Get the optional branch or tag reference.
    pub fn reference(&self) -> Option<&str> {
        self.reference.as_deref()
    }

    /// Get the optional subdirectory path.
    pub fn path(&self) -> Option<&str> {
        self.path.as_deref()
    }

    /// Convert to HTTPS URL format.
    pub fn to_https_url(&self) -> String {
        let mut url = format!("https://{}/{}", self.host, self.owner);
        url.push('/');
        url.push_str(&self.repo);

        if let Some(port) = self.port {
            url = format!(
                "https://{}:{}/{}/{}",
                self.host, port, self.owner, self.repo
            );
        }

        if let Some(ref reference) = self.reference {
            url.push('#');
            url.push_str(reference);
        }

        url
    }

    /// Convert to SSH URL format.
    pub fn to_ssh_url(&self) -> String {
        let mut url = format!("git@{}:{}/{}", self.host, self.owner, self.repo);

        if let Some(ref reference) = self.reference {
            url.push('#');
            url.push_str(reference);
        }

        url
    }

    /// Convert to git:// protocol URL format.
    pub fn to_git_url(&self) -> String {
        let mut url = format!("git://{}/{}/{}", self.host, self.owner, self.repo);

        if let Some(ref reference) = self.reference {
            url.push('#');
            url.push_str(reference);
        }

        url
    }

    /// Check if this is a GitHub repository.
    pub fn is_github(&self) -> bool {
        self.host.contains("github.com")
    }

    /// Check if this is a GitLab repository.
    pub fn is_gitlab(&self) -> bool {
        self.host.contains("gitlab.com") || self.host.contains("gitlab")
    }

    /// Check if this is a Bitbucket repository.
    pub fn is_bitbucket(&self) -> bool {
        self.host.contains("bitbucket.org")
    }
}

impl fmt::Display for GitUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.scheme.as_str() {
            "ssh" => write!(f, "{}", self.to_ssh_url()),
            "git" => write!(f, "{}", self.to_git_url()),
            _ => write!(f, "{}", self.to_https_url()),
        }
    }
}

/// Errors that can occur when parsing a Git URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitUrlError {
    /// The URL string is empty.
    EmptyUrl,
    /// The URL format is invalid.
    InvalidFormat,
    /// The URL is missing the repository path.
    MissingPath,
    /// The URL is missing the repository name.
    MissingRepo,
    /// The port number is invalid.
    InvalidPort,
}

impl fmt::Display for GitUrlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GitUrlError::EmptyUrl => write!(f, "URL is empty"),
            GitUrlError::InvalidFormat => write!(f, "Invalid URL format"),
            GitUrlError::MissingPath => write!(f, "Missing repository path"),
            GitUrlError::MissingRepo => write!(f, "Missing repository name"),
            GitUrlError::InvalidPort => write!(f, "Invalid port number"),
        }
    }
}

impl std::error::Error for GitUrlError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_https_url() {
        let url = GitUrl::parse("https://github.com/user/repo.git").unwrap();
        assert_eq!(url.scheme(), "https");
        assert_eq!(url.host(), "github.com");
        assert_eq!(url.owner(), "user");
        assert_eq!(url.repo(), "repo");
        assert_eq!(url.port(), None);
        assert_eq!(url.reference(), None);
    }

    #[test]
    fn test_parse_ssh_url() {
        let url = GitUrl::parse("git@github.com:user/repo.git").unwrap();
        assert_eq!(url.scheme(), "ssh");
        assert_eq!(url.host(), "github.com");
        assert_eq!(url.owner(), "user");
        assert_eq!(url.repo(), "repo");
    }

    #[test]
    fn test_parse_with_branch() {
        let url = GitUrl::parse("https://github.com/user/repo.git#main").unwrap();
        assert_eq!(url.reference(), Some("main"));
    }

    #[test]
    fn test_parse_with_port() {
        let url = GitUrl::parse("https://gitlab.example.com:8443/user/repo.git").unwrap();
        assert_eq!(url.host(), "gitlab.example.com");
        assert_eq!(url.port(), Some(8443));
    }

    #[test]
    fn test_to_https_url() {
        let url = GitUrl::parse("git@github.com:user/repo.git").unwrap();
        assert_eq!(url.to_https_url(), "https://github.com/user/repo");
    }

    #[test]
    fn test_to_ssh_url() {
        let url = GitUrl::parse("https://github.com/user/repo.git").unwrap();
        assert_eq!(url.to_ssh_url(), "git@github.com:user/repo");
    }

    #[test]
    fn test_is_github() {
        let url = GitUrl::parse("https://github.com/user/repo.git").unwrap();
        assert!(url.is_github());
        assert!(!url.is_gitlab());
    }

    #[test]
    fn test_invalid_url() {
        assert_eq!(GitUrl::parse(""), Err(GitUrlError::EmptyUrl));
        assert_eq!(GitUrl::parse("not-a-url"), Err(GitUrlError::InvalidFormat));
    }
}
