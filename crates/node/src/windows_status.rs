//! Native Windows shell: WebView2 window and notification area icon.
//!
//! The process is still the same Rust binary that serves the gateway. Tao
//! provides the Win32 window, Wry uses the WebView2 installed by Windows and
//! `tray-icon` registers the tray menu. There is no console and no Electron: the
//! native child process is watched by a lightweight supervisor in the same
//! executable. `main.rs` declares `windows_subsystem = "windows"` on the
//! Windows target.

pub const TRAY_SHOW_ID: &str = "pom.show";
pub const TRAY_QUIT_ID: &str = "pom.quit";
pub const WINDOW_SIZE: (f64, f64) = (1120.0, 880.0);
#[cfg(windows)]
pub const SUPERVISED_CHILD_ENV: &str = "POM_WINDOWS_SUPERVISED_CHILD";
#[cfg(windows)]
pub const CRASH_RELAUNCH_DELAY_SECS: u64 = 1;

fn wait_for_gateway_ready(receiver: &std::sync::mpsc::Receiver<u16>) -> Result<u16, String> {
    receiver
        .recv()
        .map_err(|_| "the Windows service ended before the gateway became ready".to_string())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WindowAction {
    ShowWindow,
    HideToTray,
    Quit,
}

pub const fn close_action() -> WindowAction {
    WindowAction::HideToTray
}

pub fn menu_action(id: &str) -> Option<WindowAction> {
    match id {
        TRAY_SHOW_ID => Some(WindowAction::ShowWindow),
        TRAY_QUIT_ID => Some(WindowAction::Quit),
        _ => None,
    }
}

/// The child's exit when it failed before the gateway was ready. The person
/// already saw the error in a dialog, and starting again would fail the same
/// way: relaunching it every second was an invisible loop with no window.
pub const STARTUP_FAILED_EXIT_CODE: i32 = 3;

/// Whether this process got as far as a ready gateway. A failure after that is
/// a crash of a running node, which the supervisor relaunches.
#[cfg_attr(not(windows), allow(dead_code))]
pub static GATEWAY_READY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Only a successful exit represents `Quit` in the supervisor layer, and a
/// failed startup is not retried. `None` covers termination by exception or
/// signal, which must be relaunched.
pub fn should_relaunch(exit_code: Option<i32>) -> bool {
    !matches!(exit_code, Some(0) | Some(STARTUP_FAILED_EXIT_CODE))
}

#[cfg(windows)]
mod native {
    use super::{
        close_action, menu_action, wait_for_gateway_ready, WindowAction, TRAY_QUIT_ID,
        TRAY_SHOW_ID, WINDOW_SIZE,
    };
    use anyhow::{anyhow, Context, Result};
    use image::load_from_memory;
    use std::process::Command as ProcessCommand;
    use std::thread;
    use std::time::Duration;
    use tao::dpi::LogicalSize;
    use tao::event::{Event, WindowEvent};
    use tao::event_loop::{ControlFlow, EventLoopBuilder};
    use tao::platform::run_return::EventLoopExtRunReturn;
    use tao::window::WindowBuilder;
    use tray_icon::menu::{Menu, MenuEvent, MenuItem};
    use tray_icon::{Icon, TrayIconBuilder};
    use tracing::warn;
    use webview2_com::Microsoft::Web::WebView2::Win32::{
        ICoreWebView2_14, COREWEBVIEW2_SERVER_CERTIFICATE_ERROR_ACTION_ALWAYS_ALLOW,
    };
    use webview2_com::{take_pwstr, ServerCertificateErrorDetectedEventHandler};
    use windows::core::{Interface, PWSTR};
    use wry::{WebViewBuilder, WebViewExtWindows};

    /// Lets the view open this machine's own interface over `https://` when
    /// Windows does not trust the local CA. WebView2 reports the certificate it
    /// could not verify; only the one of the persisted web key, on a loopback
    /// address, is allowed, and WebView2 remembers that for the session. Any
    /// other certificate keeps WebView2's error page.
    fn accept_local_certificate(webview: &wry::WebView) -> Result<()> {
        let core: ICoreWebView2_14 = webview
            .webview()
            .cast()
            .context("the WebView2 runtime is too old for the certificate event")?;
        let handler = ServerCertificateErrorDetectedEventHandler::create(Box::new(|_, args| {
            let Some(args) = args else {
                return Ok(());
            };
            let uri = {
                let mut uri = PWSTR::null();
                unsafe { args.RequestUri(&mut uri)? };
                take_pwstr(uri)
            };
            if crate::web_ca::local_host_of(&uri).is_none() {
                return Ok(());
            }
            let pem = {
                let certificate = unsafe { args.ServerCertificate()? };
                let mut pem = PWSTR::null();
                unsafe { certificate.ToPemEncoding(&mut pem)? };
                take_pwstr(pem)
            };
            if crate::web_ca::is_local_web_certificate_pem(&pem) {
                unsafe { args.SetAction(COREWEBVIEW2_SERVER_CERTIFICATE_ERROR_ACTION_ALWAYS_ALLOW)? };
            }
            Ok(())
        }));
        let mut token = 0i64;
        unsafe { core.add_ServerCertificateErrorDetected(&handler, &mut token) }
            .context("could not listen for WebView2 certificate errors")?;
        Ok(())
    }

    enum UserEvent {
        Menu(MenuEvent),
        ServiceStopped,
    }

    fn tray_icon() -> Result<Icon> {
        let image = load_from_memory(include_bytes!("../../../assets/brand/pom-icon-32.png"))
            .context("could not read the Windows tray icon")?
            .to_rgba8();
        Icon::from_rgba(image.clone().into_raw(), image.width(), image.height())
            .map_err(|error| anyhow!("could not build the tray icon: {error}"))
    }

    fn restore_window(window: &tao::window::Window) {
        window.set_minimized(false);
        window.set_visible(true);
        window.set_focus();
    }

    pub fn run<F>(service: F, gateway_ready: std::sync::mpsc::Receiver<u16>) -> Result<()>
    where
        F: FnOnce() -> Result<()> + Send + 'static,
    {
        if std::env::var_os(super::SUPERVISED_CHILD_ENV).is_none() {
            drop(service);
            return run_supervisor();
        }
        run_child(service, gateway_ready)
    }

    fn run_supervisor() -> Result<()> {
        let executable = std::env::current_exe().context("could not locate pom.exe")?;
        let args = std::env::args_os().skip(1).collect::<Vec<_>>();
        loop {
            let status = ProcessCommand::new(&executable)
                .args(&args)
                .env(super::SUPERVISED_CHILD_ENV, "1")
                .status()
                .with_context(|| format!("could not start {}", executable.display()))?;
            if !super::should_relaunch(status.code()) {
                return Ok(());
            }
            tracing::error!(
                exit_code = ?status.code(),
                "POM ended without a user exit; relaunching the Windows process"
            );
            thread::sleep(Duration::from_secs(super::CRASH_RELAUNCH_DELAY_SECS));
        }
    }

    fn run_child<F>(service: F, gateway_ready: std::sync::mpsc::Receiver<u16>) -> Result<()>
    where
        F: FnOnce() -> Result<()> + Send + 'static,
    {
        let mut event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
        let service_proxy = event_loop.create_proxy();
        let service = thread::spawn(move || {
            let result = service();
            let _ = service_proxy.send_event(UserEvent::ServiceStopped);
            result
        });

        // The service binds the gateway before sending this signal. Creating
        // WebView2 before that point turns a slow debug startup into a local
        // ERR_CONNECTION_REFUSED page, which is especially confusing because
        // the server is still healthy and about to listen.
        // The window loads the port the gateway bound, so a port moved at
        // startup still opens the interface.
        let Ok(port) = wait_for_gateway_ready(&gateway_ready) else {
            return service
                .join()
                .map_err(|_| anyhow!("Windows service thread ended in a panic"))?;
        };
        super::GATEWAY_READY.store(true, std::sync::atomic::Ordering::Relaxed);

        let window = WindowBuilder::new()
            .with_title("POM")
            .with_inner_size(LogicalSize::new(WINDOW_SIZE.0, WINDOW_SIZE.1))
            .build(&event_loop)
            .map_err(|error| anyhow!("could not open the Windows window: {error}"))?;
        // No URL yet: the first navigation waits for the certificate handler.
        // Started by `build`, it met the local certificate before the handler
        // existed, and without the CA trusted the window never showed the UI.
        let webview = WebViewBuilder::new()
            // A link that leaves the interface (a purchase, the site) opens in the
            // system browser, and so does any new window, so POM stays where it was.
            .with_navigation_handler(|url| {
                if crate::external_links::is_external(&url) {
                    crate::external_links::open_in_browser(&url);
                    return false;
                }
                true
            })
            .with_new_window_req_handler(|url, _features| {
                crate::external_links::open_in_browser(&url);
                wry::NewWindowResponse::Deny
            })
            .build(&window)
            .context("could not start WebView2; confirm that the runtime is installed")?;
        if let Err(error) = accept_local_certificate(&webview) {
            warn!("WebView2 cannot pin the interface certificate: {error:#}");
        }
        webview
            .load_url(&crate::gateway_url(port))
            .context("could not open the interface in WebView2")?;
        // The system asks the person once; a refusal is remembered.
        thread::spawn(crate::trust_store::auto_install);

        let menu = Menu::new();
        let show_item = MenuItem::with_id(TRAY_SHOW_ID, "Open POM", true, None);
        let quit_item = MenuItem::with_id(TRAY_QUIT_ID, "Quit POM", true, None);
        menu.append(&show_item)
            .context("could not add Restore to the tray menu")?;
        menu.append(&quit_item)
            .context("could not add Quit to the tray menu")?;
        let tray = TrayIconBuilder::new()
            .with_tooltip("POM")
            .with_menu(Box::new(menu))
            .with_icon(tray_icon()?)
            .build()
            .context("could not install the icon in the Windows tray")?;

        let proxy = event_loop.create_proxy();
        MenuEvent::set_event_handler(Some(move |event| {
            let _ = proxy.send_event(UserEvent::Menu(event));
        }));

        let mut user_quit = false;
        event_loop.run_return(|event, _, control_flow| {
            *control_flow = ControlFlow::Wait;
            match event {
                Event::WindowEvent {
                    event: WindowEvent::CloseRequested,
                    ..
                } => match close_action() {
                    WindowAction::HideToTray => window.set_visible(false),
                    WindowAction::Quit => {
                        user_quit = true;
                        *control_flow = ControlFlow::Exit;
                    }
                    WindowAction::ShowWindow => restore_window(&window),
                },
                Event::UserEvent(UserEvent::Menu(event)) => {
                    match menu_action(event.id().as_ref()) {
                        Some(WindowAction::ShowWindow) => restore_window(&window),
                        Some(WindowAction::Quit) => {
                            user_quit = true;
                            *control_flow = ControlFlow::Exit;
                        }
                        Some(WindowAction::HideToTray) | None => {}
                    }
                }
                Event::UserEvent(UserEvent::ServiceStopped) => {
                    *control_flow = ControlFlow::Exit;
                }
                _ => {}
            }
        });

        // Unregister the callback before destroying the tray and release the
        // objects in the reverse order of creation. `process::exit` without this
        // block left the WebView2 Win32 class alive and produced ERROR 1412 on
        // teardown, masking the real cause of a service crash.
        MenuEvent::set_event_handler::<fn(MenuEvent)>(None);
        drop(tray);
        drop(webview);
        drop(window);

        if user_quit {
            // As with AppKit's Quit, we do not wait for a Tokio server with no
            // cooperative channel: ending the process stops the service and
            // removes the tray icon deterministically.
            std::process::exit(0);
        }

        service
            .join()
            .map_err(|_| anyhow!("Windows service thread ended in a panic"))?
    }
}

/// What a failed start does on Windows, where the GUI binary has no console:
/// logs the error, shows it in a dialog with the log's path, and ends with
/// [`STARTUP_FAILED_EXIT_CODE`] so the supervisor does not retry it. A failure
/// after the gateway was ready returns, and the supervisor relaunches it. The
/// elevated firewall helper reports to its parent instead.
#[cfg(windows)]
pub fn report_startup_failure(error: &anyhow::Error) {
    use windows::core::PCWSTR;
    use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};

    if GATEWAY_READY.load(std::sync::atomic::Ordering::Relaxed)
        || std::env::args().any(|arg| arg.starts_with("--windows-firewall-helper"))
    {
        return;
    }
    tracing::error!("POM could not start: {error:#}");
    let log = crate::config::project_root().join(crate::windows_diagnostic_log_filename());
    let text = format!(
        "{} could not start:\n\n{error:#}\n\nDetails in {}",
        crate::brand::name(),
        log.display()
    );
    let wide = |value: &str| value.encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>();
    let (text, caption) = (wide(&text), wide(crate::brand::name()));
    unsafe {
        MessageBoxW(None, PCWSTR(text.as_ptr()), PCWSTR(caption.as_ptr()), MB_OK | MB_ICONERROR);
    }
    std::process::exit(STARTUP_FAILED_EXIT_CODE);
}

#[cfg(windows)]
pub use native::run;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closing_the_window_hides_to_the_tray() {
        assert_eq!(close_action(), WindowAction::HideToTray);
    }

    #[test]
    fn the_tray_menu_separates_restore_from_quit() {
        assert_eq!(menu_action(TRAY_SHOW_ID), Some(WindowAction::ShowWindow));
        assert_eq!(menu_action(TRAY_QUIT_ID), Some(WindowAction::Quit));
        assert_eq!(menu_action("unknown"), None);
    }

    #[test]
    fn the_shell_window_is_desktop_sized() {
        assert!(WINDOW_SIZE.0 > 1000.0);
        assert!(WINDOW_SIZE.1 > 800.0);
    }

    #[test]
    fn the_supervisor_only_relaunches_an_abnormal_exit() {
        assert!(!should_relaunch(Some(0)));
        assert!(should_relaunch(Some(1)));
        assert!(should_relaunch(None));
        assert!(!should_relaunch(Some(STARTUP_FAILED_EXIT_CODE)));
    }

    #[test]
    fn the_shell_waits_for_the_gateway_ready_signal_before_the_webview() {
        let (ready_sender, ready_receiver) = std::sync::mpsc::sync_channel(0);
        let (started_sender, started_receiver) = std::sync::mpsc::sync_channel(0);
        let (finished_sender, finished_receiver) = std::sync::mpsc::sync_channel(0);
        let waiter = std::thread::spawn(move || {
            started_sender.send(()).unwrap();
            let port = wait_for_gateway_ready(&ready_receiver).unwrap();
            finished_sender.send(port).unwrap();
        });

        started_receiver.recv().unwrap();
        assert!(matches!(
            finished_receiver.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        ready_sender.send(8081).unwrap();
        assert_eq!(finished_receiver.recv().unwrap(), 8081);
        waiter.join().unwrap();
    }
}
