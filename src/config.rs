use alloc::{string::String, vec::Vec};
use log::LevelFilter;
use serde::{Deserialize, Deserializer};

#[derive(Debug, serde::Deserialize)]
pub struct Config {
    pub keyslots: Vec<KeySlot>,
    pub partitions: Vec<Partition>,
    pub boot_entries: Vec<BootEntry>,
    pub log_level: LevelFilter,
}

#[derive(Debug, serde::Deserialize)]
pub struct KeySlot {
    pub name: String,
    pub source: KeySlotSource,
}

#[derive(Debug, serde::Deserialize)]
#[serde(untagged)]
pub enum KeySlotSource {
    #[serde(deserialize_with = "deserialize_stdin")]
    Stdin,
    File(File),
}
fn deserialize_stdin<'de, D>(deserializer: D) -> Result<(), D::Error>
    where D: Deserializer<'de>
{
    #[derive(Deserialize)]
    enum Helper { #[serde(rename = "stdin")] Stdin }
    Helper::deserialize(deserializer)?;
    Ok(())
}

#[derive(Debug, serde::Deserialize)]
pub struct File {
    pub partition: String,
    pub file: String,
}

#[derive(Debug, serde::Deserialize)]
pub struct Partition {
    pub name: String,
    pub parent: Option<String>,
    pub uuid: String,
    pub keyslot: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
pub struct BootEntry {
    pub name: String,
    #[serde(flatten)]
    pub file: File,
    pub initrd: Option<Initrd>,
    pub additional_initrd_files: Option<Vec<AdditionalInitrdFile>>,
    pub options: Option<String>,
    #[serde(default)]
    pub default: bool,
}

#[derive(Debug, serde::Deserialize)]
#[serde(untagged)]
pub enum Initrd {
    Single(File),
    Multiple(Vec<File>),
}

#[derive(Debug, serde::Deserialize)]
pub struct AdditionalInitrdFile {
    #[serde(flatten)]
    source: File,
    target_file: String,
}
