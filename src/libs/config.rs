use alloc::string::{String, ToString};
use core::fmt::Debug;

use crate::eyre::{Result, eyre};
use serde::de::DeserializeOwned;
use serde_json::Value;

struct CliArgument {
    config: String,
    config_entry: Option<String>,
}

impl CliArgument {
    fn parse() -> Self {
        let mut config = eko::env::var("CONFIG").unwrap_or_else(|| "etc/config.json".to_string());
        let mut config_entry = None;
        let args = eko::env::args();
        let mut i = 1;
        while i < args.len() {
            if let Ok(arg) = core::str::from_utf8(&args[i]) {
                if (arg == "-c" || arg == "--config") && i + 1 < args.len() {
                    if let Ok(val) = core::str::from_utf8(&args[i + 1]) {
                        config = val.to_string();
                    }
                    i += 2;
                    continue;
                } else if let Some(val) = arg.strip_prefix("--config=") {
                    config = val.to_string();
                } else if arg == "--config-entry" && i + 1 < args.len() {
                    if let Ok(val) = core::str::from_utf8(&args[i + 1]) {
                        config_entry = Some(val.to_string());
                    }
                    i += 2;
                    continue;
                } else if let Some(val) = arg.strip_prefix("--config-entry=") {
                    config_entry = Some(val.to_string());
                }
            }
            i += 1;
        }
        Self {
            config,
            config_entry,
        }
    }
}

pub fn load_config<Config: DeserializeOwned + Debug>(mut service_name: String) -> Result<Config> {
    let args = CliArgument::parse();
    let config = eko::file::read_to_string(args.config.as_str())?;

    let mut config: Value = serde_json::from_str(&config)?;
    if let Some(entry) = args.config_entry {
        service_name = entry;
    }
    let service_config = config
        .get_mut(&service_name)
        .ok_or_else(|| eyre!("Service {} not found in config", service_name))?
        .clone();
    let root = config.as_object_mut().unwrap();
    for (k, v) in service_config.as_object().unwrap() {
        root.insert(k.clone(), v.clone());
    }
    root.remove(&service_name);
    root.insert("name".to_string(), Value::String(service_name.clone()));
    let config: Config = serde_json::from_value(config)?;
    Ok(config)
}
