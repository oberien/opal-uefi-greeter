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

use alloc::collections::btree_map::Entry;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use uefi::table::boot::{AllocateType, LoadImageSource, MemoryType};
use core::{convert::TryFrom, fmt::Write, slice};
use acid_io::{Read, Seek};
use bootsector::{ReadGPT, ReadMBR, SectorSize};
use ext4::SuperBlock;
use initramfs::{Archive, Initramfs};
use log::LevelFilter;
use luks2::{LuksDevice, LuksHeader};
use lvm2::Lvm2;
use positioned_io2::SeekWrapper;
use uefi::Handle;
use uefi::table::{Boot, SystemTable};
use uefi::{CStr16, CString16, prelude::*, proto::{
    device_path::DevicePath,
    loaded_image::LoadedImage,
    media::partition::{GptPartitionType, PartitionInfo},
}, table::runtime::ResetType};
use uefi::data_types::Align;
use uefi::proto::media::block::BlockIO;
use uefi::proto::media::file::{Directory, File as _, FileAttribute, FileInfo, FileMode, FileType};
use uefi::proto::media::fs::SimpleFileSystem;
use uefi::proto::media::partition::GptPartitionEntry;
use uuid::Uuid;
use low_level::nvme_device::NvmeDevice;
use low_level::nvme_passthru::*;
use low_level::secure_device::SecureDevice;
use crate::{
    config::Config,
    error::{Error, OpalError, Result, ResultFixupExt},
    low_level::opal::{LockingState, session::OpalSession, StatusCode, uid},
    util::sleep,
};
use crate::config::{AdditionalInitrdFile, BootEntry, File, Keyslot, KeyslotSource, Partition};
use crate::io::{BlockIoReader, PartialReader};
use sha1::{Sha1, Digest};

pub mod config;
pub mod error;
pub mod util;
pub mod low_level;
pub mod unlock_opal;
mod ui;
mod io;

#[entry]
fn main(image_handle: Handle, mut st: SystemTable<Boot>) -> Status {
    if uefi_services::init(&mut st).is_err() {
        log::error!("Failed to initialize UEFI services");
    }
    if let Err(err) = run(image_handle, &mut st) {
        log::error!("Error: {:?}", err);
    }
    log::error!("Encountered error. Reboot on Enter...");
    let _ = ui::line(&mut st);
    st.runtime_services()
        .reset(ResetType::Cold, Status::SUCCESS, None)
}

fn run(image_handle: Handle, st: &mut SystemTable<Boot>) -> Result {
    // set size of console
    config_stdout(st).fix(info!())?;
    // disable watchdog
    st.boot_services().set_watchdog_timer(0, 0x31337, None).fix(info!())?;

    let config: Config = config::load(image_handle, st)?;

    let options = config.boot_entries.iter().map(|e| (true, e.name.clone())).collect();
    let selected = ui::choose(st, &options)?;
    let BootEntry { name, file, initrd, additional_initrd_files, options, default } = &config.boot_entries[selected];


    let efi_image = resolve_and_read_file(st, &config, file)?;

    let mut initramfs = Initramfs::new();

    for initrd in initrd.iter().flat_map(|initrd| initrd.iter()) {
        log::debug!("loading initrd file {}", initrd.file);
        let content = resolve_and_read_file(st, &config, initrd)?;
        log::debug!("hash of loaded initrd file: {:x?}", Sha1::new().chain(&content).finalize());
        log::debug!("hash of loaded efi_image file: {:x?}", Sha1::new().chain(&efi_image).finalize());
        initramfs.add_raw_archive(content);
    }

    let mut additional_files = Archive::new();
    for AdditionalInitrdFile { source, target_file } in additional_initrd_files.iter().flatten() {
        log::debug!("loading additional initrd file {}", source.file);
        let content = resolve_and_read_file(st, &config, source)?;
        additional_files.add_file(initramfs::File::new(target_file.clone(), content));
    }
    initramfs.add_archive(additional_files);

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

    // let dp = st
    //     .boot_services()
    //     .handle_protocol::<DevicePath>(partition)
    //     .fix(info!())?;
    // let dp = unsafe { &mut *dp.get() };
    let loaded_image_handle = st
        .boot_services()
        .load_image(image_handle, LoadImageSource::FromBuffer { file_path: None, buffer: &efi_image })
        .fix(info!())?;
    let loaded_image = st
        .boot_services()
        .handle_protocol::<LoadedImage>(loaded_image_handle)
        .fix(info!())?;
    let loaded_image = unsafe { &mut *loaded_image.get() };

    // chain-load efistub

    // let initrdmem = ;
    // let options: Vec<_> = config.options.iter().map(|a| a.as_str()).chain([&*initrdmem, "loglevel=8"]).collect();
    let mut options = options.clone().unwrap_or_default();
    options.push_str(&format!(" initrdmem={initramfs_addr},{}", serialized.len()));
    log::debug!("passing options: `{options}`");
    let options = CString16::try_from(&*options).or(Err(Error::EfiImageNameNonUtf16))?;
    unsafe { loaded_image.set_load_options(options.as_ptr() as *const u8, options.num_bytes() as _) };

    if config.log_level >= LevelFilter::Debug {
        log::debug!("loading image {} on \"uiae\" + Enter", file.file);
        loop {
            if ui::line(st).unwrap() == "uiae" {
                break;
            }
        }
    }

    st.boot_services()
        .start_image(loaded_image_handle)
        .fix(info!())?;

    // let devices = unlock_opal::find_secure_devices(st).fix(info!())?;
    //
    // for mut device in devices {
    //     if !device.recv_locked().fix(info!())? {
    //         continue;
    //     }
    //     st.stdout().write_str("password: ").unwrap();
    //     loop {
    //         let password = ui::password(st)?;
    //
    //         match unlock_opal::try_unlock_device(st, &mut device, password)? {
    //             Ok(()) => break,
    //             Err(()) => (),
    //         }
    //
    //         st.stdout().write_str("bad password, retry: ").unwrap();
    //     };
    // }
    //
    // let boot_partitions = find_boot_partitions(st)?;
    //
    // let mut boot_options = Vec::new();
    // let mut bootable_things = Vec::new();
    // for (gpt, partition) in boot_partitions {
    //     let name = gpt.partition_name;
    //     let name = unsafe { CStr16::from_ptr(&name[0]) };
    //     let partuuid = gpt.unique_partition_guid;
    //     let lbas = gpt.ending_lba - gpt.starting_lba;
    //     let description = format!("\"{name}\": {partuuid} ({lbas} LBAs)");
    //     log::debug!("found efi partition {description}");
    //     boot_options.push((false, description));
    //
    //     for efi_file in find_efi_files(st, partition)? {
    //         boot_options.push((true, format!("    {efi_file}")));
    //         bootable_things.push((partuuid, partition.clone(), efi_file));
    //     }
    // }
    //
    // let index = ui::choose(st, &boot_options)?;
    // log::trace!("chose index {index}");
    // // remove unselectable things
    // let index: usize = boot_options.iter().take(index + 1).map(|(selectable, _)| *selectable as usize).sum();
    // let index = index - 1;
    // log::trace!("cleaned index {index}");
    // let (partuuid, partition, filename) = bootable_things[index].clone();
    //
    // let filename = CString16::try_from(&*filename).or(Err(Error::EfiImageNameNonUtf16))?;
    //
    // let buf = util::read_full_file(st, partition, &filename)?;
    // if buf.get(0..2) != Some(&[0x4d, 0x5a]) {
    //     return Err(Error::ImageNotPeCoff);
    // }


    Ok(())
}

fn resolve_and_read_file(st: &mut SystemTable<Boot>, config: &Config, file: &File) -> Result<Vec<u8>> {
    let mut partitions = Vec::new();
    let mut current = &file.partition;
    loop {
        let partition = &config.partitions[current];
        partitions.push(partition);
        match &partition.parent {
            Some(parent) => current = parent,
            None => break,
        }
    }
    partitions.reverse();
    find_read_file(st, config, &partitions, &file.file)
}

fn find_read_file(st: &mut SystemTable<Boot>, config: &Config, partitions: &[&Partition], file: &str) -> Result<Vec<u8>> {
    for (i, handle) in st.boot_services().find_handles::<BlockIO>().fix(info!())?.into_iter().enumerate() {
        let blockio = st.boot_services().handle_protocol::<BlockIO>(handle).fix(info!())?;
        let media = unsafe { &* blockio.get() }.media();
        let start_lba = media.lowest_aligned_lba();
        let end_lba = media.last_block();

        let is_logical_partition = unsafe { &mut *blockio.get() }.media().is_logical_partition();
        if is_logical_partition {
            continue;
        }

        log::error!("probing blockio {start_lba:#x} - {end_lba:#x}");
        let mut reader = BlockIoReader::new(unsafe { &* blockio.get() }, start_lba, end_lba);
        match find_read_file_internal(st, &mut reader, config, partitions, file) {
            Ok(file) => return Ok(file),
            Err(e) => log::trace!("file was not found on BlockIO #{i}: {e:?}"),
        }

        // let device_path = st
        //     .boot_services()
        //     .handle_protocol::<DevicePath>(handle).fix(info!())?;
        // let device_path = unsafe { &mut &*device_path.get() };
        //
        // if let Ok(nvme) = st
        //     .boot_services()
        //     .locate_device_path::<NvmExpressPassthru>(device_path)
        // {
        //     let nvme = st
        //         .boot_services()
        //         .handle_protocol::<NvmExpressPassthru>(nvme).fix(info!())?;
        //     let nvme = NvmeDevice::new(nvme.get()).fix(info!())?;
        //     log::error!("found nvme with serial: {:?}", nvme.serial_num());
        //
        //     // result.push(SecureDevice::new(handle, NvmeDevice::new(nvme.get())?)?)
        // }
    }

    Err(Error::FileNotFound)
}

trait ReadSeek: Read + Seek {}
impl<T: Read + Seek> ReadSeek for T {}

fn find_read_file_internal(st: &mut SystemTable<Boot>, reader: &mut dyn ReadSeek, config: &Config, partitions: &[&Partition], file: &str) -> Result<Vec<u8>> {
    if partitions.is_empty() {
        return Err(Error::FileNotFound);
    }
    let partition = partitions.first().copied().unwrap();

    match Lvm2::open(&mut *reader) {
        Ok(lvm2) if lvm2.pv_id() == partition.uuid => {
            log::debug!("{}: found lvm2 with correct pv_id {}", partition.name, partition.uuid);
            for lv in lvm2.lvs() {
                let mut open_lv = lvm2.open_lv(lv, &mut *reader);
                match find_read_file_internal(st, &mut open_lv, config, &partitions[1..], file) {
                    Ok(file) => return Ok(file),
                    Err(e) => log::trace!("error probing lv {}: {e:?}", lv.name()),
                }
            }
            return Err(Error::FileNotFound);
        },
        Ok(lvm2) => log::trace!("found lvm2 with wrong id; expected {}, got {}", partition.uuid, lvm2.pv_id()),
        Err(e) => log::trace!("error trying to parse lvm2: {e}"),
    }
    reader.rewind()?;

    let mut buf = [0u8; 4096];
    reader.read_exact(&mut buf)?;
    reader.rewind()?;
    match LuksHeader::from_slice(&buf) {
        Ok(header) if header.uuid() == partition.uuid => {
            log::debug!("{}: found luks with correct uuid {}", partition.name, partition.uuid);
            let keyslot = match &partition.keyslot {
                Some(name) => &config.keyslots[name],
                None => {
                    log::error!("{}: no keyslot defined for luks", partition.name);
                    return Err(Error::FileNotFound);
                }
            };

            let master_key = config.luks_masterkey_buffer.borrow().get(&partition.uuid).cloned();
            let mut luks = match master_key {
                Some(master_key) => LuksDevice::from_device_with_master_key(reader, master_key, 512)?,
                None => {
                    let password = get_password_of_keyslot(st, config, &keyslot)?;
                    let luks = LuksDevice::from_device(reader, &password, 512)?;
                    config.luks_masterkey_buffer.borrow_mut().insert(partition.uuid.clone(), luks.master_key());
                    luks
                }
            };
            match find_read_file_internal(st, &mut luks, config, &partitions[1..], file) {
                Ok(file) => return Ok(file),
                Err(e) => log::trace!("error probing luks: {e:?}"),
            }
            return Err(Error::FileNotFound);
        }
        Ok(header) => log::trace!("found luks with wrong id; expected {}, got {}", partition.uuid, header.uuid()),
        Err(e) => log::trace!("error trying to parse luks: {e}"),
    }
    reader.rewind()?;

    match SuperBlock::new(SeekWrapper::new(&mut *reader)) {
        Ok(ext4) if Uuid::from_slice(&ext4.uuid).unwrap().to_string() == partition.uuid => {
            log::debug!("{}: found ext4 with correct uuid {}", partition.name, partition.uuid);
            if partitions.len() != 1 {
                log::error!("{}: found ext4 with correct uuid {} but there are still inner partitions left", partition.name, partition.uuid);
                return Err(Error::FileNotFound);
            }
            let entry = ext4.resolve_path(file).map_err(|_| Error::FileNotFound)?;
            let inode = ext4.load_inode(entry.inode).map_err(|_| Error::FileNotFound)?;
            let mut reader = ext4.open(&inode).unwrap();
            let mut data = Vec::new();
            reader.read_to_end(&mut data).unwrap();
            return Ok(data);
        }
        Ok(ext4) => log::trace!("found luks with wrong id; expected {}, got {}", partition.uuid, Uuid::from_slice(&ext4.uuid).unwrap().to_string()),
        Err(e) => log::trace!("error trying to parse ext4: {e}"),
    }
    reader.rewind()?;

    let options = bootsector::Options {
        mbr: ReadMBR::Never,
        gpt: ReadGPT::RevisionOne,
        sector_size: SectorSize::GuessOrAssume,
    };
    match bootsector::list_partitions(SeekWrapper::new(&mut *reader), &options) {
        Ok(parts) => {
            log::debug!("{}: found gpt with partitions: {:?}", partition.name, parts);

            for part in parts {
                let mut reader = PartialReader::new(&mut *reader, part.first_byte, part.len);
                match find_read_file_internal(st, &mut reader, config, partitions, file) {
                    Ok(file) => return Ok(file),
                    Err(e) => log::trace!("error probing gpt partition: {e:?}"),
                }
            }

            return Err(Error::FileNotFound);
        },
        Err(e) => log::trace!("error trying to parse gpt: {e}"),
    }
    reader.rewind()?;

    // match fatfs::FileSystem::new(&mut *reader, fatfs::FsOptions::new())

    Err(Error::FileNotFound)
}

fn get_password_of_keyslot(st: &mut SystemTable<Boot>, config: &Config, keyslot: &Keyslot) -> Result<Vec<u8>> {
    // we can't use entry API here as we need to drop the borrow when searching for keyfiles
    // in case those are again on an encrypted partition
    if let Some(key) = config.keyslot_buffer.borrow().get(&keyslot.name) {
        return Ok(key.clone());
    }

    let password = match &keyslot.source {
        KeyslotSource::Stdin => {
            st.stdout().write_str(&format!("Password for keyslot {}: ", keyslot.name)).unwrap();
            ui::password(st)?.into_bytes()
        },
        KeyslotSource::File(file) => {
            resolve_and_read_file(st, config, file)?
        }
    };
    config.keyslot_buffer.borrow_mut().insert(keyslot.name.clone(), password.clone());
    Ok(password)
}

fn config_stdout(st: &mut SystemTable<Boot>) -> uefi::Result {
    st.stdout().reset(false)?;

    if let Some(mode) = st.stdout().modes().max_by_key(|m| {
        m.rows() * m.columns()
    // if let Some(mode) = st.stdout().modes().min_by_key(|m| {
    //     (m.rows() as i32 * m.columns() as i32 - 200*64).abs()
    }) {
        log::trace!("selected {mode:?}");
        st.stdout().set_mode(mode)?;
    };

    Ok(().into())
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
