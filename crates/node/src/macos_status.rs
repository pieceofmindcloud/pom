//! macOS status bar item: the node's chat one click away from the menu bar.
//!
//! What is native here is only the frame - the bar item, the popover and the
//! window. The content is the same web interface the node already serves,
//! loaded in a `WKWebView` from the local gateway. There is no second language
//! in the inference path, no second process, and no bundled browser runtime:
//! AppKit and WebKit are system frameworks.
//!
//! `macos_status.m` has the implementation and the C ABI this module calls. The
//! dimensions and the address come from here, on purpose: the Objective-C side
//! does not know the node's configuration, and duplicating the numbers on both
//! sides was an invitation to a mismatch.

use anyhow::Result;
use std::ffi::{c_char, c_int, CString};
use std::path::PathBuf;

/// Panel size, in points: exactly the logical screen of an iPhone 14/15
/// (390 x 844). "Smartphone size" here is a concrete phone, not an
/// approximation, and this is the width at which the chat was checked.
///
/// The height is the target, not a promise: on a screen that cannot hold it,
/// the native side shrinks the panel to fit. An old 13" MacBook Pro is 800
/// points tall, and an 844 panel would run off the bottom edge.
pub const POPOVER_SIZE: (f64, f64) = (390.0, 844.0);

/// The larger window, for when 390 points of width are tight. Larger than the
/// panel on both axes - a "larger window" shorter than the panel that opened
/// it would be a bad trade - and resizable from there.
pub const WINDOW_SIZE: (f64, f64) = (1120.0, 880.0);

unsafe extern "C" {
    fn pom_macos_status_run(
        url: *const c_char,
        icon_path: *const c_char,
        app_name: *const c_char,
        popover_width: f64,
        popover_height: f64,
        window_width: f64,
        window_height: f64,
    ) -> c_int;
    fn pom_macos_status_stop();
}

fn chat_url(port: u16) -> String {
    crate::gateway_url(port)
}

/// Path of the bar item's 1x artwork; the native side looks for `@2x` and
/// `@3x` next to it.
///
/// Inside `POM.app` the artwork comes from `Contents/Resources`, put there by
/// `scripts/make-macos-app.sh`. Outside the bundle - `cargo run` during
/// development - it comes from the project tree, so the item shows up without
/// requiring the app to be packaged on every change.
fn icon_path() -> PathBuf {
    const FILE: &str = "pom-status-template.png";
    let bundled = std::env::current_exe().ok().and_then(|exe| {
        let contents = exe.parent()?.parent()?;
        let candidate = contents.join("Resources").join(FILE);
        candidate.exists().then_some(candidate)
    });
    bundled.unwrap_or_else(|| {
        crate::config::project_root()
            .join("assets/brand/macos")
            .join(FILE)
    })
}

/// Hands the main thread to AppKit and runs the node service beside it.
///
/// The swap is mandatory, not a preference: `NSApplication` only works from
/// the process's main thread. The service gets a thread of its own and, when
/// it ends, brings down the event loop - that is what makes a startup that
/// fails early, such as an invalid `pom.json`, exit by itself instead of
/// hanging on a bar item nobody asked for.
pub fn run<F>(port: u16, service: F) -> Result<()>
where
    F: FnOnce() -> Result<()> + Send + 'static,
{
    let service = std::thread::spawn(move || {
        let result = service();
        unsafe { pom_macos_status_stop() };
        result
    });
    let url = CString::new(chat_url(port))?;
    let icon = CString::new(icon_path().to_string_lossy().as_bytes())?;
    let app_name = CString::new(crate::brand::name())?;
    let user_quit = unsafe {
        pom_macos_status_run(
            url.as_ptr(),
            icon.as_ptr(),
            app_name.as_ptr(),
            POPOVER_SIZE.0,
            POPOVER_SIZE.1,
            WINDOW_SIZE.0,
            WINDOW_SIZE.1,
        )
    };

    if user_quit != 0 {
        // AppKit has completed the standard Quit action. The node service is
        // deliberately not joined here because it owns a long-lived server
        // loop without a cooperative shutdown signal; ending main terminates
        // both the UI and that service thread as one process.
        std::process::exit(0);
    }

    // The event loop only returns control once the service has already asked
    // to stop, so the thread ends without waiting. Propagating its error is
    // what keeps the CLI's exit code honest.
    service
        .join()
        .map_err(|_| anyhow::anyhow!("macOS service thread ended in a panic"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_popover_is_phone_sized() {
        // Portrait, and at the width of a standard iPhone: that is what
        // "smartphone size" means, and the width at which the chat was checked.
        let (width, height) = POPOVER_SIZE;
        assert_eq!(width, 390.0);
        assert!(
            height > width,
            "the status bar panel is portrait, not landscape"
        );
        assert!(
            WINDOW_SIZE.0 > width && WINDOW_SIZE.1 > height,
            "the larger window must be larger than the panel on both axes"
        );
    }

    #[test]
    fn the_address_uses_the_port_chosen_by_rust() {
        assert_eq!(chat_url(9123), "http://127.0.0.1:9123/admin-ui/chat");
        assert_eq!(chat_url(8080), "http://127.0.0.1:8080/admin-ui/chat");
    }

    #[test]
    fn the_status_bar_item_art_is_versioned_in_the_three_resolutions() {
        // The path comes from the manifest, not from `project_root()`: during
        // the tests the cwd is the crate's, not the repository root.
        //
        // If the deriver in `scripts/derive-macos-status-icon.py` stops being
        // run after the brand is touched, the bar item falls back to a text
        // label - silently. This test is the warning.
        let brand =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/brand/macos");
        for name in [
            "pom-status-template.png",
            "pom-status-template@2x.png",
            "pom-status-template@3x.png",
        ] {
            let art = brand.join(name);
            assert!(art.exists(), "falta {}", art.display());
        }
    }
}
