use alloc::string::{String, ToString};
use core::fmt::Debug;

use crate::eyre::{Result, eyre};

#[cfg(feature = "std")]
use clap::Parser;
use serde::de::DeserializeOwned;
use serde_json::Value;
#[cfg(feature = "std")]
use std::path::PathBuf;

#[cfg(feature = "std")]
#[derive(Parser)]
#[clap(author, version, about, long_about = None)]
struct CliArgument {
    /// The path to config file
    #[arg(
        short,
        long,
        value_name = "FILE",
        default_value = "etc/config.json",
        env = "CONFIG"
    )]
    config: PathBuf,
    /// The path to config file
    #[clap(long)]
    config_entry: Option<String>,
}

#[cfg(not(feature = "std"))]
struct CliArgument {
    config: String,
    config_entry: Option<String>,
}

#[cfg(not(feature = "std"))]
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
    let args: CliArgument = CliArgument::parse();

    #[cfg(feature = "std")]
    let config = std::fs::read_to_string(&args.config)?;
    #[cfg(not(feature = "std"))]
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
