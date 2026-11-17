//! Linux clipboard-persistence regression: a copy that reports success must
//! leave the text retrievable afterwards, without depending on a desktop
//! clipboard manager being installed.
//!
//! Reproduces the LXQt/Openbox failure mode where `copy_to_clipboard` created
//! an `arboard::Clipboard`, wrote, and dropped it on return: on X11 the
//! clipboard is served by the owning process, so destroying the instance
//! destroys the data whenever no `CLIPBOARD_MANAGER` took it over within
//! arboard's 100ms handoff window. Windows and macOS keep OS-owned clipboards,
//! so the same transient pattern passed there.
//!
//! Proof boundary: on a headless machine (no X11/Wayland server) the copy
//! itself fails and this probe skips — the persistence contract needs a live
//! display server. Where a clipboard manager IS installed, the old code could
//! survive via the manager handoff, so this probe must be run (and was) on a
//! manager-less X11 session.
use super::*;

#[test]
fn copied_text_survives_copy_to_clipboard_returning() {
    let text = "CLIPBOARD_PERSISTENCE_PROBE";
    if !copy_to_clipboard(text) {
        // Headless environment: there is no clipboard to be persistent in.
        return;
    }
    // A reader that knows nothing about the copy must still find the text
    // after copy_to_clipboard has returned and dropped its own handle.
    let read_back = arboard::Clipboard::new()
        .ok()
        .and_then(|mut reader| reader.get_text().ok());
    assert_eq!(
        read_back.as_deref(),
        Some(text),
        "clipboard content must survive copy_to_clipboard returning"
    );
}

#[test]
// Manual live-desktop harness, not a CI probe: with RPI_CLIP_LIVE_TEST=1 it
// copies and then serves the selection for 20s so an OUT-OF-PROCESS reader
// (e.g. `xclip -o -selection clipboard`) can verify persistence on a real
// X11/Wayland session. Skipped everywhere else; the in-process read-back in
// `copied_text_survives_copy_to_clipboard_returning` is the automated check.
fn live_cross_process_serving_manual() {
    if std::env::var("RPI_CLIP_LIVE_TEST").is_err() {
        return;
    }
    assert!(copy_to_clipboard("LIVE_X11_PROBE_OK"));
    eprintln!("copied; serving for 20s");
    std::thread::sleep(std::time::Duration::from_secs(20));
}
