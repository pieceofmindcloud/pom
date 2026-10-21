//! Starting a console program without a console window.
//!
//! `pom.exe` is a GUI-subsystem process (`main.rs`, `windows_subsystem`), so
//! Windows gives every console program it starts (`hostname.exe`,
//! `netstat.exe`, `taskkill.exe`, `tar.exe`) a window of its own unless the
//! child is created with `CREATE_NO_WINDOW`. Elsewhere these are no-ops.

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// `command`, set to start with no console window on Windows.
pub fn hidden(command: &mut std::process::Command) -> &mut std::process::Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

/// [`hidden`] for a Tokio command.
pub fn hidden_async(command: &mut tokio::process::Command) -> &mut tokio::process::Command {
    #[cfg(windows)]
    command.creation_flags(CREATE_NO_WINDOW);
    command
}
