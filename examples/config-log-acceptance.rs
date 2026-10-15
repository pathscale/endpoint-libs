use serde::Deserialize;

#[derive(Deserialize)]
struct Config {
    name: String,
    credential: String,
    inherited: String,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        panic!("loaded configuration Debug must never run")
    }
}

// This regression fixture deliberately exercises the deprecated configuration loader.
#[allow(deprecated)]
fn main() {
    if std::env::var_os("ENDPOINT_CONFIG_LOG_CHILD").is_some() {
        let config: Config = endpoint_libs::libs::config::load_config("unused".to_owned()).unwrap();
        assert_eq!(config.name, "fixture");
        assert_eq!(config.credential, "CONFIG_LOG_SENTINEL_6B");
        assert_eq!(config.inherited, "root value");
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.json");
    std::fs::write(
        &path,
        r#"{"credential":"overridden","inherited":"root value","fixture":{"credential":"CONFIG_LOG_SENTINEL_6B"}}"#,
    ).unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .env("ENDPOINT_CONFIG_LOG_CHILD", "1")
        .args([
            "--config",
            path.to_str().unwrap(),
            "--config-entry",
            "fixture",
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "config loader child failed");
    assert!(
        output.stdout.is_empty(),
        "configuration loader wrote stdout"
    );
    assert!(
        output.stderr.is_empty(),
        "configuration loader wrote stderr"
    );
    println!("config log acceptance passed");
}
