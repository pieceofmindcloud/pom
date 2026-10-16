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
use tao::window::WindowBuilder;
use wry::WebViewBuilder;

/// The same size as the macOS and Windows windows.
pub const WINDOW_SIZE: (f64, f64) = (1120.0, 880.0);

enum UserEvent {
    ServiceStopped,
}

/// Runs the service on a background thread, opens the window once the gateway answers, and
/// returns when the window is closed or the service stops.
pub fn run<F>(url: String, service: F, gateway_ready: Receiver<()>) -> Result<()>
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
    if gateway_ready.recv().is_err() {
        return service
            .join()
            .map_err(|_| anyhow!("the service thread ended in a panic"))?;
    }

    let window = WindowBuilder::new()
        .with_title("POM")
        .with_inner_size(LogicalSize::new(WINDOW_SIZE.0, WINDOW_SIZE.1))
        .build(&event_loop)
        .map_err(|error| anyhow!("could not open the window: {error}"))?;
    let webview = WebViewBuilder::new()
        .with_url(&url)
        .build(&window)
        .context("could not start the WebKitGTK view; install the webkit2gtk-4.1 runtime")?;

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
