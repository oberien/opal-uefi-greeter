use alloc::{string::String, vec::Vec};
use core::str;
use log::LevelFilter;
use uefi::Handle;
use uefi::prelude::cstr16;
use uefi::proto::device_path::DevicePath;
use uefi::proto::loaded_image::LoadedImage;
use uefi::proto::media::fs::SimpleFileSystem;
use uefi::table::{Boot, SystemTable};

use crate::error::Result;
use crate::{info, ResultFixupExt};

pub fn load(image_handle: Handle, st: &mut SystemTable<Boot>) -> Result<Config> {
    let loaded_image = st
        .boot_services()
        .handle_protocol::<LoadedImage>(image_handle)
        .fix(info!())?;
    let device_path = st
        .boot_services()
        .handle_protocol::<DevicePath>(unsafe { &*loaded_image.get() }.device())
        .fix(info!())?;
    let device_handle = st
        .boot_services()
        .locate_device_path::<SimpleFileSystem>(unsafe { &mut &*device_path.get() })
        .fix(info!())?;
    let buf = crate::util::read_full_file(st, device_handle, cstr16!("config.toml"))?;
    let config: Config = toml::from_slice(&buf)?;
    log::set_max_level(config.log_level);
    log::debug!("loaded config = {:#?}", config);
    Ok(config)
}


#[derive(Debug, serde::Deserialize)]
pub struct Config {
    pub image: String,
    pub args: Vec<String>,
    pub log_level: LevelFilter,
    pub prompt: Option<String>,
    pub retry_prompt: Option<String>,
    pub sed_locked_msg: Option<String>,
    pub clear_on_retry: bool,
}
