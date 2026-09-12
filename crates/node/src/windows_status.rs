//! Shell nativo do Windows: janela WebView2 e ícone na área de notificação.
//!
//! O processo continua sendo o mesmo binário Rust que serve o gateway. Tao
//! entrega a janela Win32, Wry usa o WebView2 instalado pelo Windows e
//! `tray-icon` registra o menu da bandeja. Não há console, Electron ou processo
//! auxiliar: `main.rs` declara `windows_subsystem = "windows"` no alvo Windows.

pub const TRAY_SHOW_ID: &str = "pom.show";
pub const TRAY_QUIT_ID: &str = "pom.quit";
pub const WINDOW_SIZE: (f64, f64) = (1120.0, 880.0);

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

#[cfg(windows)]
mod native {
    use super::{close_action, menu_action, WindowAction, TRAY_QUIT_ID, TRAY_SHOW_ID, WINDOW_SIZE};
    use anyhow::{anyhow, Context, Result};
    use image::load_from_memory;
    use std::thread;
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
            .context("não deu para ler o ícone da bandeja do Windows")?
            .to_rgba8();
        Icon::from_rgba(image.clone().into_raw(), image.width(), image.height())
            .map_err(|error| anyhow!("não deu para montar o ícone da bandeja: {error}"))
    }

    fn restore_window(window: &tao::window::Window) {
        window.set_minimized(false);
        window.set_visible(true);
        window.set_focus();
    }

    pub fn run<F>(port: u16, service: F) -> Result<()>
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

        let window = WindowBuilder::new()
            .with_title("POM")
            .with_inner_size(LogicalSize::new(WINDOW_SIZE.0, WINDOW_SIZE.1))
            .build(&event_loop)
            .map_err(|error| anyhow!("não deu para abrir a janela Windows: {error}"))?;
        let _webview = WebViewBuilder::new()
            .with_url(crate::gateway_url(port))
            .build(&window)
            .context("não deu para iniciar o WebView2; confirme que o runtime está instalado")?;

        let menu = Menu::new();
        let show_item = MenuItem::with_id(TRAY_SHOW_ID, "Abrir POM", true, None);
        let quit_item = MenuItem::with_id(TRAY_QUIT_ID, "Sair do POM", true, None);
        menu.append(&show_item)
            .context("não deu para adicionar Restaurar ao menu da bandeja")?;
        menu.append(&quit_item)
            .context("não deu para adicionar Sair ao menu da bandeja")?;
        let _tray = TrayIconBuilder::new()
            .with_tooltip("POM")
            .with_menu(Box::new(menu))
            .with_icon(tray_icon()?)
            .build()
            .context("não deu para instalar o ícone na bandeja do Windows")?;

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

        if user_quit {
            // Assim como o Sair do AppKit, não aguardamos um servidor Tokio
            // sem canal cooperativo: terminar o processo encerra o serviço e
            // remove o ícone da bandeja de forma determinística.
            std::process::exit(0);
        }

        service
            .join()
            .map_err(|_| anyhow!("thread do serviço Windows terminou em pânico"))?
    }
}

#[cfg(windows)]
pub use native::run;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fechar_a_janela_esconde_na_bandeja() {
        assert_eq!(close_action(), WindowAction::HideToTray);
    }

    #[test]
    fn menu_da_bandeja_separa_restaurar_de_sair() {
        assert_eq!(menu_action(TRAY_SHOW_ID), Some(WindowAction::ShowWindow));
        assert_eq!(menu_action(TRAY_QUIT_ID), Some(WindowAction::Quit));
        assert_eq!(menu_action("unknown"), None);
    }

    #[test]
    fn janela_do_shell_tem_tamanho_de_desktop() {
        assert!(WINDOW_SIZE.0 > 1000.0);
        assert!(WINDOW_SIZE.1 > 800.0);
    }
}
