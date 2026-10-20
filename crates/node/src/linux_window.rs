//! Linux window: the node's web interface in a native WebKitGTK view.
//!
//! The process is the same binary that serves the gateway. Tao provides the GTK window and
//! Wry the WebKitGTK view, which loads the local gateway. Unlike the Windows shell there is
//! no tray icon: closing the window ends POM, and the service stops with it.
//!
//! Only built with the `desktop` feature. On Linux the node is a server by default, and a
//! server should not depend on GTK.

use anyhow::{anyhow, Context, Result};
use std::sync::mpsc::Receiver;
use std::thread;
use tao::dpi::LogicalSize;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::platform::run_return::EventLoopExtRunReturn;
use tao::platform::unix::WindowExtUnix;
use tao::window::WindowBuilder;
use webkit2gtk::gio::prelude::TlsCertificateExt;
use webkit2gtk::{WebContextExt, WebViewExt};
use wry::{WebViewBuilder, WebViewBuilderExtUnix, WebViewExtUnix};

/// The same size as the macOS and Windows windows.
pub const WINDOW_SIZE: (f64, f64) = (1120.0, 880.0);

enum UserEvent {
    ServiceStopped,
}

/// Runs the service on a background thread, opens the window once the gateway answers, and
/// returns when the window is closed or the service stops.
///
/// The window loads the port the gateway reports having bound, not one computed before
/// startup, so a port moved at startup still opens the interface.
pub fn run<F>(service: F, gateway_ready: Receiver<u16>) -> Result<()>
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

    // The service signals this only after it binds the gateway. A view opened earlier shows
    // "connection refused" while the node is still starting.
    let Ok(port) = gateway_ready.recv() else {
        return service
            .join()
            .map_err(|_| anyhow!("the service thread ended in a panic"))?;
    };

    let window = WindowBuilder::new()
        .with_title("POM")
        .with_inner_size(LogicalSize::new(WINDOW_SIZE.0, WINDOW_SIZE.1))
        .build(&event_loop)
        .map_err(|error| anyhow!("could not open the window: {error}"))?;
    // The view goes into the window's GTK box. `build(&window)` makes an X11 child window,
    // which stays blank under Wayland - the default session on current Ubuntu.
    let container = window
        .default_vbox()
        .ok_or_else(|| anyhow!("the window has no GTK container for the view"))?;
    let webview = WebViewBuilder::new()
        .with_url(crate::gateway_url(port))
        .build_gtk(container)
        .context("could not start the WebKitGTK view; install the webkit2gtk-4.1 runtime")?;
    accept_local_certificate(&webview.webview());
    // The system asks the person once; a refusal is remembered.
    thread::spawn(crate::trust_store::auto_install);

    let mut closed_by_user = false;
    event_loop.run_return(|event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => {
                closed_by_user = true;
                *control_flow = ControlFlow::Exit;
            }
            Event::UserEvent(UserEvent::ServiceStopped) => *control_flow = ControlFlow::Exit,
            _ => {}
        }
    });

    // The view and the window are released before the process ends, in the reverse order of
    // creation, as the Windows shell does.
    drop(webview);
    drop(window);

    if closed_by_user {
        // Ending the process stops the service: a Tokio server has no cooperative channel to
        // stop it from here.
        std::process::exit(0);
    }
    service
        .join()
        .map_err(|_| anyhow!("the service thread ended in a panic"))?
}

/// Lets the view open this machine's own interface over `https://` when the
/// system does not trust the local CA: WebKitGTK reports the failed load with
/// the certificate, and only the certificate of the persisted web key, on a
/// loopback address, is allowed for that host before loading again. Any other
/// certificate keeps the error page.
fn accept_local_certificate(view: &webkit2gtk::WebView) {
    view.connect_load_failed_with_tls_errors(|view, uri, certificate, _errors| {
        let Some(host) = crate::web_ca::local_host_of(uri) else {
            return false;
        };
        let Some(der) = certificate.certificate() else {
            return false;
        };
        if !crate::web_ca::is_local_web_certificate(&der) {
            return false;
        }
        let Some(context) = view.context() else {
            return false;
        };
        context.allow_tls_certificate_for_host(certificate, &host);
        view.load_uri(uri);
        true
    });
}

/// Opens a window on a POM that is already running from the same project root, instead of
/// starting a second node with the same identity. Closing the window leaves that POM running.
pub fn show_running(port: u16) -> Result<()> {
    let (ready, gateway_ready) = std::sync::mpsc::sync_channel(1);
    let _ = ready.send(port);
    run(
        || loop {
            thread::park();
        },
        gateway_ready,
    )
}
