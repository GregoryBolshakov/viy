//! Prints the effective config of the given layer files as one JSON object,
//! with this process's environment on top. For `tools/parity/config_diff.py`,
//! which points it at rp's files and prints only key names.
//!
//!     config_dump GLOBAL PROFILES_DIR [PROJECT_CONFIG ...]

use std::path::PathBuf;
use std::process::ExitCode;

use viy_config::Layers;

fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1).map(PathBuf::from);
    let (Some(global), Some(profiles_dir)) = (args.next(), args.next()) else {
        eprintln!("usage: config_dump GLOBAL PROFILES_DIR [PROJECT_CONFIG ...]");
        return ExitCode::from(2);
    };
    let layers = Layers {
        global,
        profiles_dir,
        project: args.collect(),
    };
    let cfg = match layers.load(|key| std::env::var(key).ok()) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("config_dump: {e}");
            return ExitCode::FAILURE;
        }
    };
    let map: serde_json::Map<String, serde_json::Value> =
        cfg.iter().map(|(k, v)| (k.to_string(), v.into())).collect();
    println!("{}", serde_json::Value::Object(map));
    ExitCode::SUCCESS
}
