use anyhow::Result;
use std::ffi::{c_char, CString};
use std::path::PathBuf;

pub const POPOVER_SIZE: (f64, f64) = (390.0, 844.0);
pub const WINDOW_SIZE: (f64, f64) = (1080.0, 760.0);

unsafe extern "C" {
    fn pom_macos_status_run(url: *const c_char, icon_path: *const c_char);
    fn pom_macos_status_stop();
}

fn chat_url(port: u16) -> String {
    crate::gateway_url(port)
}

fn icon_path() -> PathBuf {
    let bundled = std::env::current_exe().ok().and_then(|exe| {
        let contents = exe.parent()?.parent()?;
        let candidate = contents.join("Resources/pom-status.png");
        candidate.exists().then_some(candidate)
    });
    bundled.unwrap_or_else(|| crate::config::project_root().join("assets/brand/pom-mark.png"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_surfaces_use_phone_and_desktop_dimensions() {
        assert_eq!(POPOVER_SIZE, (390.0, 844.0));
        assert_eq!(WINDOW_SIZE, (1080.0, 760.0));
    }

    #[test]
    fn native_chat_url_uses_the_port_selected_by_rust() {
        assert_eq!(chat_url(9123), "http://127.0.0.1:9123/admin-ui/chat");
    }
}

pub fn run<F>(port: u16, service: F) -> Result<()>
where
    F: FnOnce() -> Result<()> + Send + 'static,
{
    let service = std::thread::spawn(move || {
        let result = service();
        // One-shot CLI commands such as --list-models must not leave the
        // AppKit run loop alive after the command has completed.
        unsafe { pom_macos_status_stop() };
        result
    });
    let url = CString::new(chat_url(port))?;
    let icon = CString::new(icon_path().to_string_lossy().as_bytes())?;
    unsafe { pom_macos_status_run(url.as_ptr(), icon.as_ptr()) };

    if service.is_finished() {
        service
            .join()
            .map_err(|_| anyhow::anyhow!("thread do serviço macOS terminou em pânico"))??;
    }
    Ok(())
}
