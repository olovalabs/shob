//! Best-effort alert sounds.
//!
//! No audio dependency: ask the OS for one of its stock sounds and ignore the
//! result. `KATHAMO_NO_SOUND=1` disables it (tests and headless runs).

use crate::attention::AlertKind;

pub fn play(kind: AlertKind) {
    if cfg!(test)
        || std::env::var_os("KATHAMO_NO_SOUND").is_some()
        || std::env::var_os("MARKUP_NO_SOUND").is_some()
    {
        return;
    }
    let _ = spawn(kind);
}

#[cfg(target_os = "macos")]
fn spawn(kind: AlertKind) -> std::io::Result<()> {
    use std::process::Command;
    let sound = match kind {
        AlertKind::Blocked => "Basso",
        AlertKind::Done => "Glass",
    };
    Command::new("afplay")
        .arg(format!("/System/Library/Sounds/{sound}.aiff"))
        .spawn()
        .map(|_| ())
}

#[cfg(target_os = "linux")]
fn spawn(kind: AlertKind) -> std::io::Result<()> {
    use std::process::Command;
    // Preferred: the freedesktop event-sound theme.
    let event = match kind {
        AlertKind::Blocked => "dialog-error",
        AlertKind::Done => "dialog-information",
    };
    if Command::new("canberra-gtk-play")
        .args(["-i", event])
        .spawn()
        .is_ok()
    {
        return Ok(());
    }
    // Fall back to a stock file on common themes.
    let files: &[&str] = match kind {
        AlertKind::Blocked => &[
            "/usr/share/sounds/freedesktop/stereo/dialog-error.oga",
            "/usr/share/sounds/freedesktop/stereo/bell.oga",
        ],
        AlertKind::Done => &[
            "/usr/share/sounds/freedesktop/stereo/complete.oga",
            "/usr/share/sounds/freedesktop/stereo/bell.oga",
        ],
    };
    for file in files {
        if std::path::Path::new(file).is_file() {
            if Command::new("paplay").arg(file).spawn().is_ok() {
                return Ok(());
            }
            if Command::new("aplay").arg(file).spawn().is_ok() {
                return Ok(());
            }
        }
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn spawn(kind: AlertKind) -> std::io::Result<()> {
    use std::process::Command;
    let sound = match kind {
        AlertKind::Blocked => "Windows Exclamation.wav",
        AlertKind::Done => "Windows Notify System Generic.wav",
    };
    Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &format!("(New-Object Media.SoundPlayer 'C:\\Windows\\Media\\{sound}').PlaySync()"),
        ])
        .spawn()
        .map(|_| ())
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn spawn(_kind: AlertKind) -> std::io::Result<()> {
    Ok(())
}
