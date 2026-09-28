//! Item da barra de status do macOS: o chat do nó a um clique da barra de menus.
//!
//! O que é nativo aqui é só a moldura - o item de barra, o popover e a janela.
//! O conteúdo é a mesma interface web que o nó já serve, carregada num
//! `WKWebView` a partir do gateway local. Não há segunda linguagem no caminho
//! de inferência, segundo processo, nem runtime de navegador empacotado:
//! AppKit e WebKit são frameworks do sistema.
//!
//! `macos_status.m` tem a implementação e a ABI C que este módulo chama. As
//! dimensões e o endereço saem daqui, de propósito: o lado Objective-C não
//! conhece a configuração do nó, e duplicar os números nos dois lados era
//! convite a desencontro.

use anyhow::Result;
use std::ffi::{c_char, c_int, CString};
use std::path::PathBuf;

/// Medida do painel, em pontos: exatamente a tela lógica de um iPhone 14/15
/// (390 x 844). "Tamanho de smartphone" aqui é um telemóvel concreto, não uma
/// aproximação, e é nesta largura que o chat foi conferido.
///
/// A altura é o alvo, não uma promessa: num ecrã que não a comporte, o lado
/// nativo encolhe o painel para caber. Um MacBook Pro de 13" antigo tem 800
/// pontos de altura, e um painel de 844 sairia pela borda de baixo.
pub const POPOVER_SIZE: (f64, f64) = (390.0, 844.0);

/// A janela maior, para quando 390 pontos de largura apertam. Maior que o
/// painel nos dois eixos - uma "janela maior" mais baixa que o painel que a
/// abriu seria uma troca ruim - e redimensionável a partir daí.
pub const WINDOW_SIZE: (f64, f64) = (1120.0, 880.0);

unsafe extern "C" {
    fn pom_macos_status_run(
        url: *const c_char,
        icon_path: *const c_char,
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

/// Caminho da arte 1x do item de barra; o lado nativo procura `@2x` e `@3x` ao
/// lado dela.
///
/// Dentro do `POM.app` a arte vem de `Contents/Resources`, posta lá por
/// `scripts/make-macos-app.sh`. Fora do bundle - `cargo run` durante o
/// desenvolvimento - ela vem da árvore do projeto, para o item aparecer sem
/// exigir que se empacote o app a cada alteração.
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

/// Entrega a thread principal ao AppKit e corre o serviço do nó ao lado.
///
/// A troca é obrigatória, não uma preferência: `NSApplication` só funciona a
/// partir da thread principal do processo. O serviço recebe uma thread própria
/// e, quando termina, derruba o laço de eventos - é isso que faz um arranque
/// que falha cedo, como um `pom.json` inválido, sair sozinho em vez de ficar
/// pendurado num item de barra que ninguém pediu.
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
    let user_quit = unsafe {
        pom_macos_status_run(
            url.as_ptr(),
            icon.as_ptr(),
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

    // O laço de eventos só devolve o controlo quando o serviço já pediu a
    // paragem, então a thread termina sem espera. Propagar o erro dela é o que
    // mantém o código de saída do CLI honesto.
    service
        .join()
        .map_err(|_| anyhow::anyhow!("thread do serviço macOS terminou em pânico"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn o_popover_tem_medida_de_telemovel() {
        // Retrato, e na largura de um iPhone padrão: é o que "tamanho de
        // smartphone" quer dizer, e a largura em que o chat foi conferido.
        let (width, height) = POPOVER_SIZE;
        assert_eq!(width, 390.0);
        assert!(height > width, "o painel da barra é retrato, não paisagem");
        assert!(
            WINDOW_SIZE.0 > width && WINDOW_SIZE.1 > height,
            "a janela maior tem de ser maior que o painel nos dois eixos"
        );
    }

    #[test]
    fn o_endereco_usa_a_porta_escolhida_pelo_rust() {
        assert_eq!(chat_url(9123), "http://127.0.0.1:9123/admin-ui/chat");
        assert_eq!(chat_url(8080), "http://127.0.0.1:8080/admin-ui/chat");
    }

    #[test]
    fn a_arte_do_item_de_barra_esta_versionada_nas_tres_resolucoes() {
        // O caminho sai do manifesto, e não de `project_root()`: durante os
        // testes o cwd é o do crate, não a raiz do repositório.
        //
        // Se o derivador em `scripts/derive-macos-status-icon.py` deixar de
        // ser corrido depois de mexer na marca, o item de barra cai para um
        // rótulo de texto - em silêncio. Este teste é o aviso.
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
