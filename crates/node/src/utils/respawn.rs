//! Reiniciar este processo como ele subiu.
//!
//! O respawn ingênuo - `Command::new(current_exe())` e nada mais - perde os
//! argumentos de linha de comando. Um nó subido com `--advertise-addr` volta
//! sem ele, e os peers passam a guardar um endereço temporário que expira.
//!
//! O ambiente não precisa de cuidado: `Command` o herda por padrão, e é isso
//! que preserva `POM_CONFIG` e `POM_NODE_KEY` de cada nó de uma
//! máquina com vários.

use std::sync::OnceLock;

static ARGV: OnceLock<Vec<String>> = OnceLock::new();

/// Guarda o `argv` do arranque. Chamada uma vez, no início de `main`.
pub fn remember_argv(args: Vec<String>) {
    let _ = ARGV.set(argv_rest(args));
}

/// Os argumentos sem o `argv[0]`. Separada de `remember_argv` porque o
/// `OnceLock` só aceita um valor por processo e um teste não pode desfazê-lo.
fn argv_rest(args: Vec<String>) -> Vec<String> {
    args.into_iter().skip(1).collect()
}

/// Os argumentos do arranque, sem o `argv[0]`.
pub fn original_argv() -> Vec<String> {
    ARGV.get().cloned().unwrap_or_default()
}

/// Sobe uma cópia deste processo e sai.
///
/// Só retorna quando o spawn falha - aí o chamador segue vivo e reporta, em vez
/// de derrubar um nó que não conseguiu se substituir.
pub fn respawn_self() -> std::io::Error {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => return e,
    };
    match std::process::Command::new(exe)
        .args(original_argv())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(_) => std::process::exit(0),
        Err(e) => e,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// O respawn precisa devolver o processo do jeito que ele subiu. O caminho
    /// antigo repassava só --gateway-port, então um nó com --advertise-addr
    /// voltava sem ele e os peers passavam a guardar um endereço temporário.
    #[test]
    fn argv_lembrado_volta_igual_sem_o_programa() {
        remember_argv(vec![
            "./target/release/pom".into(),
            "--refresh".into(),
            "3600".into(),
            "--advertise-addr".into(),
            "[2804:d55::1]:42777".into(),
        ]);
        assert_eq!(
            original_argv(),
            vec![
                "--refresh".to_string(),
                "3600".to_string(),
                "--advertise-addr".to_string(),
                "[2804:d55::1]:42777".to_string(),
            ],
            "o argv0 é o executável e volta por current_exe, não pela lista"
        );
    }

    /// Um nó subido sem argumento nenhum volta sem argumento nenhum - e a
    /// lista vazia não pode virar um argumento vazio na linha de comando.
    #[test]
    fn argv_so_com_o_programa_nao_gera_argumento() {
        assert!(argv_rest(vec!["./target/release/pom".to_string()]).is_empty());
        assert_eq!(
            argv_rest(vec!["pom".into(), "--no-discovery".into()]),
            vec!["--no-discovery".to_string()]
        );
    }
}
