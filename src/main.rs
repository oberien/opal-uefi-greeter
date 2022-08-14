#![no_std]
#![no_main]
#![feature(abi_efiapi)]
#![feature(negative_impls)]
#![feature(new_uninit)]
#![feature(maybe_uninit_slice)]
#![allow(clippy::missing_safety_doc)]
#![allow(deprecated)]

#[macro_use]
extern crate alloc;
// make sure to link this
extern crate rlibc;

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use uefi::table::boot::{AllocateType, LoadImageSource, MemoryType};
use core::{convert::TryFrom, fmt::Write, slice};
use initramfs::{Archive, File, Initramfs};
use core::str;
use log::LevelFilter;
use uefi::Handle;
use uefi::prelude::cstr16;
use uefi::table::{Boot, SystemTable};
use uefi::{CStr16, CString16, prelude::*, proto::{
    device_path::DevicePath,
    loaded_image::LoadedImage,
    media::partition::{GptPartitionType, PartitionInfo},
}, table::runtime::ResetType};
use uefi::data_types::Align;
use uefi::proto::media::file::{Directory, File as _, FileAttribute, FileInfo, FileMode, FileType};
use uefi::proto::media::fs::SimpleFileSystem;
use uefi::proto::media::partition::GptPartitionEntry;
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
mod ui;

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

    let config = config::load(image_handle, st)?;

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

    let boot_partitions = find_boot_partitions(st)?;

    let mut boot_options = Vec::new();
    let mut bootable_things = Vec::new();
    for (gpt, partition) in boot_partitions {
        let name = gpt.partition_name;
        let name = unsafe { CStr16::from_ptr(&name[0]) };
        let partuuid = gpt.unique_partition_guid;
        let lbas = gpt.ending_lba - gpt.starting_lba;
        let description = format!("\"{name}\": {partuuid} ({lbas} LBAs)");
        log::debug!("found efi partition {description}");
        boot_options.push((false, description));

        for efi_file in find_efi_files(st, partition)? {
            boot_options.push((true, format!("    {efi_file}")));
            bootable_things.push((partuuid, partition.clone(), efi_file));
        }
    }

    let index = ui::choose(st, &boot_options)?;
    log::trace!("chose index {index}");
    // remove unselectable things
    let index: usize = boot_options.iter().take(index + 1).map(|(selectable, _)| *selectable as usize).sum();
    let index = index - 1;
    log::trace!("cleaned index {index}");
    let (partuuid, partition, filename) = bootable_things[index].clone();

    let filename = CString16::try_from(&*filename).or(Err(Error::EfiImageNameNonUtf16))?;

    let buf = util::read_full_file(st, partition, &filename)?;
    if buf.get(0..2) != Some(&[0x4d, 0x5a]) {
        return Err(Error::ImageNotPeCoff);
    }

    // initramfs

    let mut initramfs = Initramfs::new();

    let mut before_archive = Archive::new();
    before_archive.add_file(File::new("/VERSION".to_string(), vec![b'a']));
    before_archive.add_file(File::new("/keymap.utf8".to_string(), vec![b'b']));
    before_archive.add_file(File::new("/before_new_file".to_string(), vec![b'c']));
    before_archive.add_file(File::new("/new_folder/".to_string(), vec![]));
    before_archive.add_file(File::new("/new_folder/foo".to_string(), vec![b'3']));
    before_archive.add_file(File::new("/new_folder2/".to_string(), vec![]));
    before_archive.add_file(File::new("/new_folder2/foo".to_string(), vec![b'3']));
    before_archive.add_trailer();
    initramfs.add_archive(before_archive);

    let filename = CString16::try_from("\\initramfs-linux.img").or(Err(Error::InitrdNameNonUtf16))?;
    log::trace!("loading initramfs");
    let actual_initramfs = util::read_full_file(st, partition, &filename)?;
    initramfs.add_raw_archive(actual_initramfs);

    let mut after_archive = Archive::new();
    after_archive.add_file(File::new("/keymap.utf8".to_string(), vec![b'z']));
    after_archive.add_file(File::new("/after_new_file".to_string(), vec![b'y']));
    after_archive.add_file(File::new("/new_folder/bar".to_string(), vec![b'8']));
    after_archive.add_file(File::new("/new_folder2/".to_string(), vec![]));
    after_archive.add_file(File::new("/new_folder2/bar".to_string(), vec![b'9']));
    after_archive.add_trailer();
    initramfs.add_archive(after_archive);

    let mut serialized = Vec::new();
    initramfs.write(&mut serialized);

    let num_pages = (serialized.len() + 4095) / 4096;
    // Allocate initramfs in RUNTIME_SERVICES_DATA such that it is available after the EFISTUB calls exit_boot_services.
    // After reallocating the RAMDISK, the kernel frees our memory in `reserve_initrd` via `memblock_phys_free`.
    let initramfs_addr = st.boot_services()
        .allocate_pages(AllocateType::AnyPages, MemoryType::RUNTIME_SERVICES_DATA, num_pages)
        .fix(info!())?;
    let buffer = unsafe { slice::from_raw_parts_mut(initramfs_addr as *mut u8, num_pages * 4096) };
    (&mut buffer[..serialized.len()]).copy_from_slice(&serialized);
    log::debug!("initramfs loaded");

    // LoadedImage

    let dp = st
        .boot_services()
        .handle_protocol::<DevicePath>(partition)
        .fix(info!())?;
    let dp = unsafe { &mut *dp.get() };
    let loaded_image_handle = st
        .boot_services()
        .load_image(image_handle, LoadImageSource::FromBuffer { file_path: Some(dp), buffer: &buf })
        .fix(info!())?;
    let loaded_image = st
        .boot_services()
        .handle_protocol::<LoadedImage>(loaded_image_handle)
        .fix(info!())?;
    let loaded_image = unsafe { &mut *loaded_image.get() };

    // chain-load efistub

    let initrdmem = format!("initrdmem={initramfs_addr},{}", serialized.len());
    let args: Vec<_> = config.args.iter().map(|a| a.as_str()).chain([&*initrdmem, "loglevel=8"]).collect();
    let args = args.join(" ");
    log::debug!("passing args: `{args}`");
    let args = CString16::try_from(&*args).or(Err(Error::EfiImageNameNonUtf16))?;
    unsafe { loaded_image.set_load_options(args.as_ptr() as *const u8, args.num_bytes() as _) };

    log::debug!("loading image {partuuid}:{filename} on Enter");
    let _ = input::line(st);

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
        log::trace!("selected {mode:?}");
        st.stdout().set_mode(mode)?;
    };

    Ok(().into())
}

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

fn find_boot_partitions(st: &mut SystemTable<Boot>) -> Result<Vec<(GptPartitionEntry, Handle)>> {
    let mut res = Vec::new();
    let handles = st.boot_services().find_handles::<PartitionInfo>().fix(info!())?;
    for handle in handles {
        let pi = st
            .boot_services()
            .handle_protocol::<PartitionInfo>(handle)
            .fix(info!())?;
        let pi = unsafe { &mut *pi.get() };

        match pi.gpt_partition_entry() {
            Some(gpt) if { gpt.partition_type_guid } == GptPartitionType::EFI_SYSTEM_PARTITION => {
                res.push((gpt.clone(), handle));
            }
            _ => {}
        }
    }
    Ok(res)
}

fn find_efi_files(st: &mut SystemTable<Boot>, partition: Handle) -> Result<Vec<String>> {
    let sfs = st
        .boot_services()
        .handle_protocol::<SimpleFileSystem>(partition)
        .fix(info!())?;
    let sfs = unsafe { &mut *sfs.get() };

    let dir = sfs.open_volume().fix(info!())?;
    let mut files = Vec::new();
    find_efi_files_rec(st, &mut files, String::from(""), partition, dir)?;
    Ok(files)
}

fn find_efi_files_rec(st: &mut SystemTable<Boot>, files: &mut Vec<String>, prefix: String, partition: Handle, mut directory: Directory) -> Result<()> {
    let mut buf = vec![0; 1024];
    let buf = FileInfo::align_buf(&mut buf).unwrap();
    let dir_info: &FileInfo = directory.get_info(buf).fix(info!())?;
    let name = dir_info.file_name().to_string();
    let prefix = format!("{prefix}{name}\\");
    log::trace!("visiting directory {prefix}");

    loop {
        let mut buf = vec![0; 1024];
        let buf = FileInfo::align_buf(&mut buf).unwrap();
        let entry = match directory.read_entry(buf).fix(info!())? {
            Some(entry) => entry,
            None => break,
        };
        let name = entry.file_name().to_string();
        if name == "." || name == ".." {
            continue;
        }
        let file_handle = directory.open(entry.file_name(), FileMode::Read, FileAttribute::empty())
            .fix(info!())?;
        match file_handle.into_type().fix(info!())? {
            FileType::Regular(_) => {
                let filename = format!("{prefix}{name}");
                log::trace!("found file {filename}");
                let filename_cstr16 = CString16::try_from(&*filename).or(Err(Error::FileNameNonUtf16))?;
                let mut header = vec![0; 2];
                let read = util::read_partial_file_to_vec(st, partition, &filename_cstr16, &mut header)?;
                if read != 2 {
                    log::trace!("    smaller than 2 bytes (read {read} bytes)");
                    continue;
                }
                if header != [0x4d, 0x5a] {
                    log::trace!("    not PE/COFF (header {header:x?})");
                    continue;
                }
                log::trace!("    PE/COFF");
                files.push(filename);
            },
            FileType::Dir(dir) => find_efi_files_rec(st, files, prefix.clone(), partition, dir)?,
        }
    }

    Ok(())
}
