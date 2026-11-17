//! `rpi --connect`: connect a [`RemoteDriver`] and run the shared TUI shell.
//!
//! This module is the remote **adapter** only — the shell it drives lives in
//! [`crate::tui_shell`], and the transport lives in
//! [`crate::session_driver::RemoteDriver`].

use crate::app::EXIT_RUNTIME;
use crate::session_driver::RemoteDriver;
use crate::tui_shell::run_driver_tui;

/// Connect to a server and run the interactive client until the user exits.
pub async fn run(addr: &str, token: Option<&str>) -> i32 {
    let driver = match RemoteDriver::connect(addr, token).await {
        Ok(driver) => driver,
        Err(error) => {
            eprintln!("error: {error}");
            return EXIT_RUNTIME;
        }
    };
    let code = match run_driver_tui(&driver, addr).await {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("error: {error}");
            EXIT_RUNTIME
        }
    };
    driver.shutdown().await;
    code
}
