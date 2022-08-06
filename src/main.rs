#![no_std]
#![no_main]
#![feature(abi_efiapi)]
#![feature(negative_impls)]
#![feature(new_uninit)]
#![feature(maybe_uninit_slice)]
#![allow(clippy::missing_safety_doc)]

#[macro_use]
extern crate alloc;
// make sure to link this
extern crate rlibc;

use alloc::vec::Vec;
use uefi::table::boot::LoadImageSource;
use core::{convert::TryFrom, fmt::Write};

use uefi::{
    CStr16,
    CString16,
    prelude::*,
    proto::{
        device_path::DevicePath,
        loaded_image::LoadedImage,
        media::{
            file::{File, FileAttribute, FileInfo, FileMode, FileType},
            fs::SimpleFileSystem,
            partition::{GptPartitionType, PartitionInfo},
        },
    }, table::{boot::MemoryType, runtime::ResetType},
};
use low_level::nvme_device::NvmeDevice;
use low_level::nvme_passthru::*;
use low_level::secure_device::SecureDevice;

use crate::{
    config::Config,
    error::{Error, OpalError, Result, ResultFixupExt},
    low_level::opal::{LockingState, session::OpalSession, StatusCode, uid},
    util::sleep,
};

pub mod config;
pub mod dp_to_text;
pub mod error;
pub mod util;
pub mod input;
pub mod low_level;
pub mod unlock_opal;

#[entry]
fn main(image_handle: Handle, mut st: SystemTable<Boot>) -> Status {
    if uefi_services::init(&mut st).is_err() {
        log::error!("Failed to initialize UEFI services");
    }
    if let Err(err) = run(image_handle, &mut st) {
        log::error!("Error: {:?}", err);
    }
    log::error!("Encountered error. Reboot on Enter...");
    let _ = input::line(&mut st);
    st.runtime_services()
        .reset(ResetType::Cold, Status::SUCCESS, None)
}

fn run(image_handle: Handle, st: &mut SystemTable<Boot>) -> Result {
    // set size of console
    config_stdout(st).fix(info!())?;
    // disable watchdog
    st.boot_services().set_watchdog_timer(0, 0x31337, None).fix(info!())?;

    let config = load_config(image_handle, st)?;

    let devices = unlock_opal::find_secure_devices(st).fix(info!())?;

    for mut device in devices {
        if !device.recv_locked().fix(info!())? {
            continue;
        }
        let prompt = config.prompt.as_deref().unwrap_or("password: ");
        let retry_prompt = config.retry_prompt.as_deref().unwrap_or("bad password, retry: ");
        st.stdout().write_str(prompt).unwrap();
        loop {
            let password = input::password(st)?;

            match unlock_opal::try_unlock_device(st, &config, &mut device, password)? {
                Ok(()) => break,
                Err(()) => (),
            }

            if config.clear_on_retry {
                st.stdout().clear().fix(info!())?;
            }
            st.stdout().write_str(retry_prompt).unwrap();
        };
    }

    let handle = find_boot_partition(st)?;

    let dp = st
        .boot_services()
        .handle_protocol::<DevicePath>(handle)
        .fix(info!())?;
    let dp = unsafe { &mut *dp.get() };

    let image = CString16::try_from(config.image.as_str()).or(Err(Error::ConfigArgsBadUtf16))?;

    let buf = read_file(st, handle, &image)
        .fix(info!())?
        .ok_or(Error::ImageNotFound(config.image))?;

    if buf.get(0..2) != Some(&[0x4d, 0x5a]) {
        return Err(Error::ImageNotPeCoff);
    }

    let loaded_image_handle = st
        .boot_services()
        .load_image(image_handle, LoadImageSource::FromBuffer { file_path: Some(dp), buffer: &buf })
        .fix(info!())?;
    let loaded_image = st
        .boot_services()
        .handle_protocol::<LoadedImage>(loaded_image_handle)
        .fix(info!())?;
    let loaded_image = unsafe { &mut *loaded_image.get() };

    let args = CString16::try_from(&*config.args).or(Err(Error::ConfigArgsBadUtf16))?;
    unsafe { loaded_image.set_load_options(args.as_ptr() as *const u8, args.num_bytes() as _) };

    st.boot_services()
        .start_image(loaded_image_handle)
        .fix(info!())?;

    Ok(())
}

fn config_stdout(st: &mut SystemTable<Boot>) -> uefi::Result {
    st.stdout().reset(false)?;

    if let Some(mode) = st.stdout().modes().min_by_key(|m| {
        (m.rows() as i32 * m.columns() as i32 - 200*64).abs()
    }) {
        log::info!("selected {mode:?}");
        st.stdout().set_mode(mode)?;
    };

    Ok(().into())
}

fn load_config(image_handle: Handle, st: &mut SystemTable<Boot>) -> Result<Config> {
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
    let buf = read_file(st, device_handle, cstr16!("config"))
        .fix(info!())?
        .ok_or(Error::ConfigMissing)?;
    let config = Config::parse(&buf)?;
    log::set_max_level(config.log_level);
    log::debug!("loaded config = {:#?}", config);
    Ok(config)
}


fn find_boot_partition(st: &mut SystemTable<Boot>) -> Result<Handle> {
    let mut res = None;
    for handle in st
        .boot_services()
        .find_handles::<PartitionInfo>()
        .fix(info!())?
    {
        let pi = st
            .boot_services()
            .handle_protocol::<PartitionInfo>(handle)
            .fix(info!())?;
        let pi = unsafe { &mut *pi.get() };

        match pi.gpt_partition_entry() {
            Some(gpt) if { gpt.partition_type_guid } == GptPartitionType::EFI_SYSTEM_PARTITION => {
                if res.replace(handle).is_some() {
                    return Err(Error::MultipleBootPartitions);
                }
            }
            _ => {}
        }
    }
    res.ok_or(Error::NoBootPartitions)
}

fn read_file(
    st: &mut SystemTable<Boot>,
    device: Handle,
    file: &CStr16,
) -> uefi::Result<Option<Vec<u8>>> {
    let sfs = st
        .boot_services()
        .handle_protocol::<SimpleFileSystem>(device)?;
    let sfs = unsafe { &mut *sfs.get() };

    let file_handle = sfs
        .open_volume()?
        .open(&file, FileMode::Read, FileAttribute::empty())?;

    if let FileType::Regular(mut f) = file_handle.into_type()? {
        let info = f.get_boxed_info::<FileInfo>()?;
        let size = info.file_size() as usize;
        let ptr = st
            .boot_services()
            .allocate_pool(MemoryType::LOADER_DATA, size)?;
        let mut buf = unsafe { Vec::from_raw_parts(ptr, size, size) };

        let read = f
            .read(&mut buf)
            .map_err(|_| uefi::Status::BUFFER_TOO_SMALL)?;
        buf.truncate(read);
        Ok(Some(buf).into())
    } else {
        Ok(None.into())
    }
}
