//! Open URL in the default browser — mirrors `packages/coding-agent/src/utils/open-browser.ts`.
//!
//! Cross-platform utility to open a URL in the user's default browser.
//! Intentionally avoids invoking a shell to prevent command injection from
//! attacker-controlled URLs.
//!
//! # Platform Support
//!
//! - **macOS**: Uses `open` command
//! - **Windows**: Uses `rundll32 url.dll,FileProtocolHandler`
//! - **Linux/Unix**: Uses `xdg-open`
//!
//! # Security
//!
//! This implementation deliberately does NOT use a shell. On Windows, using
//! `cmd /c start` would allow attacker-controlled URLs to inject metacharacters
//! (&, |, ^, etc.) that cmd.exe re-parses before `start` runs.
//!
//! # Example
//!
//! This launches a real browser, so the example is compiled but not run by doctests.
//!
//! ```no_run
//! use rpi_tools::utils::open_browser::open_browser;
//!
//! // Open a URL in the default browser
//! if let Err(e) = open_browser("https://example.com") {
//!     eprintln!("Failed to open browser: {}", e);
//! }
//! ```

use std::process::Command;

/// Open a URL in the default browser. Returns `Ok(())` if the browser launch
/// command was successfully invoked (not if the browser actually opened the URL).
///
/// # Arguments
///
/// * `url` - The URL to open. Should be a valid URL string.
///
/// # Returns
///
/// * `Ok(())` - The browser launch command was invoked successfully.
/// * `Err(String)` - Failed to invoke the browser launch command.
///
/// # Platform-specific behavior
///
/// - **macOS**: Runs `open <url>`
/// - **Windows**: Runs `rundll32 url.dll,FileProtocolHandler <url>`
/// - **Linux/Unix**: Runs `xdg-open <url>`
///
/// # Security notes
///
/// This function intentionally does NOT invoke a shell. The URL is passed
/// directly as an argument to the platform-specific launcher, preventing
/// command injection from malicious URLs.
/// Pure platform mapping used by [`open_browser`] — resolves which launcher
/// command to run, but spawns nothing.
///
/// Split out from [`open_browser`] so the platform selection can be unit-tested
/// without actually launching the user's browser.
fn browser_launch_command(url: &str) -> Result<(&'static str, Vec<&str>), String> {
    if cfg!(target_os = "macos") {
        Ok(("open", vec![url]))
    } else if cfg!(target_os = "windows") {
        Ok(("rundll32", vec!["url.dll,FileProtocolHandler", url]))
    } else if cfg!(target_os = "linux")
        || cfg!(target_os = "freebsd")
        || cfg!(target_os = "openbsd")
        || cfg!(target_os = "netbsd")
    {
        Ok(("xdg-open", vec![url]))
    } else {
        Err("Unsupported platform for opening browser".to_string())
    }
}

pub fn open_browser(url: &str) -> Result<(), String> {
    let (cmd, args) = browser_launch_command(url)?;

    // Spawn the command without waiting for it to complete.
    // Browser launch is best-effort: we don't want to block if the browser
    // takes time to start, and we don't want a launcher failure to crash
    // the application.
    match Command::new(cmd).args(&args).spawn() {
        Ok(_child) => {
            // Successfully spawned. We don't wait for it to complete.
            Ok(())
        }
        Err(e) => {
            // Failed to spawn. This could be because the command doesn't exist
            // (e.g., xdg-open not installed on a minimal Linux system).
            Err(format!("Failed to open browser: {}", e))
        }
    }
}

/// Open a file with the default application. Returns `Ok(())` if the launch
/// command was successfully invoked.
///
/// # Arguments
///
/// * `path` - The file path to open.
///
/// # Returns
///
/// * `Ok(())` - The launch command was invoked successfully.
/// * `Err(String)` - Failed to invoke the launch command.
pub fn open_file(path: &str) -> Result<(), String> {
    let (cmd, args): (&str, Vec<&str>) = if cfg!(target_os = "macos") {
        ("open", vec![path])
    } else if cfg!(target_os = "windows") {
        // On Windows, we can use `start` but need to be careful about shells.
        // Using rundll32 with FileProtocolHandler works for files too.
        ("rundll32", vec!["url.dll,FileProtocolHandler", path])
    } else if cfg!(target_os = "linux")
        || cfg!(target_os = "freebsd")
        || cfg!(target_os = "openbsd")
        || cfg!(target_os = "netbsd")
    {
        ("xdg-open", vec![path])
    } else {
        return Err(format!("Unsupported platform for opening file"));
    };

    match Command::new(cmd).args(&args).spawn() {
        Ok(_) => Ok(()),
        Err(e) => Err(format!("Failed to open file: {}", e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Commented out on purpose: this calls `open_browser`, which really spawns
    // the platform launcher (on Windows `rundll32 url.dll,FileProtocolHandler`),
    // so merely running the suite pops a browser window open. The side-effect-free
    // equivalent below asserts the same platform mapping without launching
    // anything, so there is nothing left for this to cover.
    //
    // #[test]
    // fn test_open_browser() {
    //     let result = open_browser("https://example.com");
    //     assert!(result.is_ok());
    // }

    #[test]
    fn test_platform_detection() {
        // Must stay side-effect free: this asserts the platform mapping only.
        // Calling `open_browser` here would really launch the user's browser
        // (on Windows: rundll32 url.dll,FileProtocolHandler <url>).
        let (cmd, args) =
            browser_launch_command("https://example.com").expect("supported platform");
        if cfg!(target_os = "windows") {
            assert_eq!(cmd, "rundll32");
            assert_eq!(
                args,
                vec!["url.dll,FileProtocolHandler", "https://example.com"]
            );
        } else if cfg!(target_os = "macos") {
            assert_eq!(cmd, "open");
            assert_eq!(args, vec!["https://example.com"]);
        } else {
            assert_eq!(cmd, "xdg-open");
            assert_eq!(args, vec!["https://example.com"]);
        }
    }
}
