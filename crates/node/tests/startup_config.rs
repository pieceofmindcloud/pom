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
    fs::create_dir_all(&home).expect("create the test configuration directory");
    let _home = TestHome(home.clone());
    fs::write(
        home.join("pom.json"),
        r#"{"role":"infer","profile":"visitor"}"#,
    )
    .expect("write invalid configuration");

    // The config is read before anything else, so the failure comes out right away,
    // without needing a flag that ends the process.
    let output = Command::new(env!("CARGO_BIN_EXE_pom"))
        .env("POM_HOME", &home)
        .env_remove("POM_CONFIG")
        .output()
        .expect("run pom with an invalid configuration");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "invalid configuration ended with status 0; stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        stderr
    );
    assert!(
        stderr.contains("Failed to parse config"),
        "the failure must name the configuration error; stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("role") && stderr.contains("infer"),
        "the failure must name the invalid value; stderr:\n{stderr}"
    );
}
