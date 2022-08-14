extern crate alloc;

use crate::config::Config;

#[path = "../../src/config.rs"]
mod config;

fn main() {
    let config = include_bytes!("../../config-example.toml");
    let config: Config = toml::from_slice(config).unwrap();
    dbg!(config);
}
