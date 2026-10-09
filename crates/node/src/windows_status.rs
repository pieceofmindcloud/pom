//! Shell nativo do Windows: janela WebView2 e ícone na área de notificação.
//!
//! O processo continua sendo o mesmo binário Rust que serve o gateway. Tao
//! entrega a janela Win32, Wry usa o WebView2 instalado pelo Windows e
//! `tray-icon` registra o menu da bandeja. Não há console nem Electron: o
//! processo filho nativo é acompanhado por um supervisor leve no mesmo
//! executável. `main.rs` declara `windows_subsystem = "windows"` no alvo
//! Windows.

pub const TRAY_SHOW_ID: &str = "pom.show";
pub const TRAY_QUIT_ID: &str = "pom.quit";
pub const WINDOW_SIZE: (f64, f64) = (1120.0, 880.0);
#[cfg(windows)]
pub const SUPERVISED_CHILD_ENV: &str = "POM_WINDOWS_SUPERVISED_CHILD";
#[cfg(windows)]
pub const CRASH_RELAUNCH_DELAY_SECS: u64 = 1;

fn wait_for_gateway_ready(receiver: &std::sync::mpsc::Receiver<()>) -> Result<(), String> {
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

/// Só uma saída bem-sucedida representa `Sair` na camada supervisora.
/// `None` cobre encerramento por exceção ou sinal, que deve ser relançado.
pub fn should_relaunch(exit_code: Option<i32>) -> bool {
    exit_code != Some(0)
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
    use wry::WebViewBuilder;

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

    pub fn run<F>(port: u16, service: F, gateway_ready: std::sync::mpsc::Receiver<()>) -> Result<()>
    where
        F: FnOnce() -> Result<()> + Send + 'static,
    {
        if std::env::var_os(super::SUPERVISED_CHILD_ENV).is_none() {
            drop(service);
            return run_supervisor();
        }
        run_child(port, service, gateway_ready)
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

    fn run_child<F>(
        port: u16,
        service: F,
        gateway_ready: std::sync::mpsc::Receiver<()>,
    ) -> Result<()>
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
        if wait_for_gateway_ready(&gateway_ready).is_err() {
            return service
                .join()
                .map_err(|_| anyhow!("Windows service thread ended in a panic"))?;
        }

        let window = WindowBuilder::new()
            .with_title("POM")
            .with_inner_size(LogicalSize::new(WINDOW_SIZE.0, WINDOW_SIZE.1))
            .build(&event_loop)
            .map_err(|error| anyhow!("could not open the Windows window: {error}"))?;
        let webview = WebViewBuilder::new()
            .with_url(crate::gateway_url(port))
            .build(&window)
            .context("could not start WebView2; confirm that the runtime is installed")?;

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

        // Desregistre o callback antes de destruir a bandeja e libere os
        // objetos na ordem inversa da criação. `process::exit` sem esse trecho
        // deixava a classe Win32 do WebView2 viva e produzia ERROR 1412 no
        // teardown, mascarando a causa real de uma queda do serviço.
        MenuEvent::set_event_handler::<fn(MenuEvent)>(None);
        drop(tray);
        drop(webview);
        drop(window);

        if user_quit {
            // Assim como o Sair do AppKit, não aguardamos um servidor Tokio
            // sem canal cooperativo: terminar o processo encerra o serviço e
            // remove o ícone da bandeja de forma determinística.
            std::process::exit(0);
        }

        service
            .join()
            .map_err(|_| anyhow!("Windows service thread ended in a panic"))?
    }
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
    }

    #[test]
    fn the_shell_waits_for_the_gateway_ready_signal_before_the_webview() {
        let (ready_sender, ready_receiver) = std::sync::mpsc::sync_channel(0);
        let (started_sender, started_receiver) = std::sync::mpsc::sync_channel(0);
        let (finished_sender, finished_receiver) = std::sync::mpsc::sync_channel(0);
        let waiter = std::thread::spawn(move || {
            started_sender.send(()).unwrap();
            wait_for_gateway_ready(&ready_receiver).unwrap();
            finished_sender.send(()).unwrap();
        });

        started_receiver.recv().unwrap();
        assert!(matches!(
            finished_receiver.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        ready_sender.send(()).unwrap();
        finished_receiver.recv().unwrap();
        waiter.join().unwrap();
    }
}
