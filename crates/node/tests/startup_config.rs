use std::fs;
use std::path::PathBuf;
use std::process::Command;

struct TestHome(PathBuf);

impl Drop for TestHome {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn invalid_role_exits_with_config_error() {
    let home = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(".test-runtime")
        .join(format!("invalid-role-{}", std::process::id()));
    fs::create_dir_all(&home).expect("criar diretório de configuração de teste");
    let _home = TestHome(home.clone());
    fs::write(
        home.join("pom.json"),
        r#"{"role":"infer","profile":"infer"}"#,
    )
    .expect("escrever configuração inválida");

    let output = Command::new(env!("CARGO_BIN_EXE_pom"))
        .args(["--list-models"])
        .env("POM_HOME", &home)
        .env_remove("POM_CONFIG")
        .output()
        .expect("executar pom com configuração inválida");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "configuração inválida terminou com status 0; stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        stderr
    );
    assert!(
        stderr.contains("Failed to parse config"),
        "a falha precisa nomear o erro de configuração; stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("role") && stderr.contains("infer"),
        "a falha precisa nomear o valor inválido; stderr:\n{stderr}"
    );
}
