//! Reiniciar este processo como ele subiu.
//!
//! O respawn ingênuo - `Command::new(current_exe())` e nada mais - perde os
//! argumentos de linha de comando. Um nó subido com `--advertise-addr` volta
//! sem ele, e os peers passam a guardar um endereço temporário que expira.
//!
//! O ambiente não precisa de cuidado: `Command` o herda por padrão, e é isso
//! que preserva `POM_CONFIG` e `POM_NODE_KEY` de cada nó de uma
//! máquina com vários.

use std::fs::OpenOptions;
use std::io::Write;
use std::process::{Command, Stdio};
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
    respawn_at(&exe)
}

/// Sobe uma cópia por um caminho capturado antes de uma troca atômica.
///
/// Em Unix, depois de `rename` sobre o executável atual, `current_exe()` pode
/// apontar para o inode antigo com o sufixo `(deleted)`. Quem faz a troca deve
/// capturar o caminho antes e passá-lo aqui.
pub(crate) fn respawn_at(exe: &std::path::Path) -> std::io::Error {
    let log_path = crate::config::project_root().join("pom.log");
    let mut log = match OpenOptions::new().create(true).append(true).open(&log_path) {
        Ok(file) => file,
        Err(error) => return error,
    };
    if let Err(error) = writeln!(
        log,
        "\n--- respawn solicitado pelo processo {} ---",
        std::process::id()
    ) {
        return error;
    }
    let stderr = match log.try_clone() {
        Ok(file) => file,
        Err(error) => return error,
    };

    let mut command = Command::new(exe);
    command
        .args(original_argv())
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(stderr));
    // A controlled restart from the supervised Windows child must promote the
    // new process to supervisor. Otherwise the old parent sees exit code 0 and
    // leaves the replacement orphaned without crash monitoring.
    #[cfg(windows)]
    command.env_remove(crate::windows_status::SUPERVISED_CHILD_ENV);
    detach_from_parent_session(&mut command);

    match command.spawn() {
        Ok(_) => std::process::exit(0),
        Err(e) => e,
    }
}

/// O wizard é salvo a partir de um processo que pode estar ligado a um
/// terminal ou supervisor. O filho precisa sobreviver ao encerramento desse
/// grupo, e os descritores precisam ir para um arquivo que permita diagnosticar
/// falhas logo no arranque.
#[cfg(unix)]
fn detach_from_parent_session(command: &mut Command) {
    use std::os::unix::process::CommandExt;

    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(not(unix))]
fn detach_from_parent_session(_command: &mut Command) {}

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
            "--gateway-port".into(),
            "8090".into(),
            "--advertise-addr".into(),
            "[2804:d55::1]:42777".into(),
        ]);
        assert_eq!(
            original_argv(),
            vec![
                "--gateway-port".to_string(),
                "8090".to_string(),
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
