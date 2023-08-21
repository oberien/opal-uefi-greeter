use alloc::{string::String, vec::Vec};
use alloc::collections::BTreeMap;
use core::cell::RefCell;
use log::LevelFilter;
use serde::{Deserialize, Deserializer};
use either::Either;
#[cfg(target_os = "uefi")] use uefi::prelude::cstr16;
#[cfg(target_os = "uefi")] use uefi::{Handle, table::SystemTable, table::Boot};
#[cfg(target_os = "uefi")] use uefi::proto::loaded_image::LoadedImage;
#[cfg(target_os = "uefi")] use uefi::proto::device_path::DevicePath;
#[cfg(target_os = "uefi")] use uefi::proto::media::fs::SimpleFileSystem;
#[cfg(target_os = "uefi")] use crate::error::Context;

#[cfg(target_os = "uefi")]
pub fn load(image_handle: Handle, st: &SystemTable<Boot>) -> crate::Result<Config> {
    let loaded_image = st
        .boot_services()
        .open_protocol_exclusive::<LoadedImage>(image_handle)
        .context("cannot get LoadedImage")?;
    let device_path = st
        .boot_services()
        .open_protocol_exclusive::<DevicePath>(loaded_image.device())
        .context("cannot get DevicePath of LoadedImage")?;
    let device_handle = st
        .boot_services()
        .locate_device_path::<SimpleFileSystem>(&mut &*device_path)
        .context("cannot get SimpleFileSystem from DevicePath from LoadedImage")?;
    let buf = crate::util::read_full_file(st, device_handle, cstr16!("config.toml"))?;
    let config: Config = toml::from_slice(&buf)
        .context("error decoding config file as toml")?;
    log::set_max_level(config.log_level);
    // log::debug!("loaded config = {:#?}", config);
    Ok(config)
}


#[derive(Debug, serde::Deserialize)]
pub struct Config {
    #[serde(deserialize_with = "deserialize_keyslots")]
    pub keyslots: BTreeMap<String, Keyslot>,
    #[serde(skip)]
    pub keyslot_buffer: RefCell<BTreeMap<String, Vec<u8>>>,
    #[serde(skip)]
    pub luks_masterkey_buffer: RefCell<BTreeMap<String, luks2::SecretMasterKey>>,
    #[serde(deserialize_with = "deserialize_partitions")]
    pub partitions: BTreeMap<String, Partition>,
    pub boot_entries: Vec<BootEntry>,
    pub log_level: LevelFilter,
}

fn deserialize_keyslots<'de, D: Deserializer<'de>>(deserializer: D) -> Result<BTreeMap<String, Keyslot>, D::Error> {
    let keyslots = Vec::<Keyslot>::deserialize(deserializer)?;
    Ok(keyslots.into_iter().map(|ks| (ks.name.clone(), ks)).collect())
}
fn deserialize_partitions<'de, D: Deserializer<'de>>(deserializer: D) -> Result<BTreeMap<String, Partition>, D::Error> {
    let partitions = Vec::<Partition>::deserialize(deserializer)?;
    Ok(partitions.into_iter().map(|p| (p.name.clone(), p)).collect())
}

#[derive(Debug, serde::Deserialize)]
pub struct Keyslot {
    pub name: String,
    pub source: KeyslotSource,
}

#[derive(Debug, serde::Deserialize)]
#[serde(untagged)]
pub enum KeyslotSource {
    #[serde(deserialize_with = "deserialize_stdin")]
    Stdin,
    File(File),
}
fn deserialize_stdin<'de, D: Deserializer<'de>>(deserializer: D) -> Result<(), D::Error> {
    #[derive(Deserialize)]
    enum Helper { #[serde(rename = "stdin")] Stdin }
    Helper::deserialize(deserializer)?;
    Ok(())
}

#[derive(Debug, serde::Deserialize)]
pub struct File {
    pub partition: String,
    #[serde(default)]
    pub extra_partitions: Vec<String>,
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

impl Initrd {
    pub fn iter(&self) -> impl Iterator<Item = &File> {
        match self {
            Initrd::Single(file) => Either::Left(core::iter::once(file)),
            Initrd::Multiple(files) => Either::Right(files.iter()),
        }
    }
}

#[derive(Debug, serde::Deserialize)]
pub struct AdditionalInitrdFile {
    #[serde(flatten)]
    pub source: File,
    pub target_file: String,
}
