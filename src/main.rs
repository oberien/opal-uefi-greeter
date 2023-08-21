#![no_std]
#![no_main]

#![allow(clippy::missing_safety_doc)]
#![allow(deprecated)]

#[macro_use]
extern crate alloc;
// make sure to link this
extern crate rlibc;

use alloc::boxed::Box;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use either::Either;
use low_level::ata_passthru::{AtaPassthru, AtaProtocol};
use opal::PasswordOrRaw;
use uefi::proto::device_path::text::{DisplayOnly, AllowShortcuts};
use uefi::table::boot::{AllocateType, LoadImageSource, MemoryType, OpenProtocolParams, OpenProtocolAttributes};
use core::time::Duration;
use core::{convert::TryFrom, fmt::Write, slice};
use acid_io::{IoSliceMut, Read};
use bootsector::{ReadGPT, ReadMBR, SectorSize};
use ext4::SuperBlock;
use initramfs::{Archive, Initramfs};
use io_compat::AcidReadCompat;
use log::LevelFilter;
use luks2::{LuksDevice, LuksHeader};
use luks2::error::LuksError;
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
use uefi::proto::media::block::{BlockIO, Lba};
use uefi::proto::media::file::{Directory, File as _, FileAttribute, FileInfo, FileMode, FileType};
use uefi::proto::media::fs::SimpleFileSystem;
use uefi::proto::media::partition::GptPartitionEntry;
use uuid::Uuid;
use low_level::nvme_device::NvmeDevice;
use low_level::nvme_passthru::*;
use crate::low_level::nvme_device::RestartableNvmeDevice;
use crate::{
    config::Config,
    error::{Error, Result, Context},
    util::sleep,
};
use crate::config::{AdditionalInitrdFile, BootEntry, File, Initrd, Keyslot, KeyslotSource, Partition};
use crate::error::ErrorSource;
use crate::io::{BlockIoReader, PartialReader, OptimizedSeek, ReadSeek, IgnoreWriteWrapper};

pub mod config;
pub mod error;
pub mod util;
pub mod low_level;
mod ui;
mod io;

#[entry]
fn main(image_handle: Handle, mut st: SystemTable<Boot>) -> Status {
    if uefi_services::init(&mut st).is_err() {
        log::error!("Failed to initialize UEFI services");
    }


    let exit = |st: &SystemTable<Boot>| {
        let _ = ui::line(st);
        st.runtime_services()
        .reset(ResetType::COLD, Status::SUCCESS, None)
    };

    let config: Config = match config::load(image_handle, &mut st) {
        Ok(config) => config,
        Err(err) => {
            log::error!("Error loading config: {err}");
            return exit(&mut st);
        }
    };
    log::trace!("loaded config");
    loop {
        match run(image_handle, &mut st, &config) {
           Ok(()) => (),
           Err(err) => {
               log::error!("Error during execution: {err}");
               break
           }
       }
    }

    exit(&mut st)
}

fn run(image_handle: Handle, st: &SystemTable<Boot>, config: &Config) -> Result {
    // set size of console
    config_stdout(st).context("can't configure stdout")?;
    log::trace!("configured stdout");

    // disable watchdog
    st.boot_services().set_watchdog_timer(0, 0x31337, None)
        .context("error disabling 5min reboot watchdog")?;
    log::trace!("disabled watchdog");

    let mut options: Vec<_> = config.boot_entries.iter().map(|e| (true, e.name.clone())).collect();
    options.push((true, "Unlock configured opal drives".to_string()));
    log::trace!("created chooser-options");
    let selected = ui::choose(st, &options)?;
    let boot_entry_len = config.boot_entries.len();

    match selected {
        i if i < boot_entry_len => {
            let boot_entry = &config.boot_entries[selected];
            handle_boot_entry(st, image_handle, config, boot_entry)?;
        },
        i if i == boot_entry_len => handle_unlock_configured_opal_drives(st, config)?,
        i => unreachable!("unknown boot entry selection {}", i),
    }

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

    Ok(())
}


fn try_get_ata_device(st: &SystemTable<Boot>, blockio_handle: Handle) -> Result<Option<opal::OpalDrive<AtaProtocol<'_>>>> {
    let params = OpenProtocolParams { handle: blockio_handle, agent: st.boot_services().image_handle(), controller: None };
    let device_path = unsafe {
        st
            .boot_services()
            .open_protocol::<DevicePath>(params, OpenProtocolAttributes::GetProtocol)
            .context("can't get DevicePath of BlockIO-Handle")?
    };

    let mut locate_path = &*device_path;
    let path_before_locate = locate_path.to_string(st.boot_services(), DisplayOnly(true), AllowShortcuts(false)).map_err(|e| Error::new_without_source(format!("dtos: {e}")))?.unwrap();
    log::info!("path before locate: {path_before_locate}");
    let ata_passthrough_handle = st.boot_services().locate_device_path::<AtaPassthru>(&mut locate_path);
    let path_after_locate = locate_path.to_string(st.boot_services(), DisplayOnly(true), AllowShortcuts(false)).map_err(|e| Error::new_without_source(format!("dtos: {e}")))?.unwrap();
    log::info!("path after locate: {path_after_locate}");

    match ata_passthrough_handle {
        Ok(nvme) => {
            let nvme = st
                .boot_services()
                .open_protocol_exclusive::<AtaPassthru>(nvme)
                .context("error creating AtaPassthru handle")?;

            let proto = AtaProtocol::try_make(nvme, locate_path, st, blockio_handle)?;
            let opal = opal::OpalDrive::new(proto).map_err(|e| Error::new(e, "error opening opal"))?;

            Ok(Some(opal))
        },
        Err(_) => Ok(None),
    }
}


fn handle_unlock_configured_opal_drives(st: &SystemTable<Boot>, config: &Config) -> Result<()> {
    for (i, (blockio_handle, start_lba, end_lba)) in block_devices(st)?.into_iter().enumerate() {
        log::debug!("probing blockio #{i} {start_lba:#x} - {end_lba:#x}");

        // probe OPAL
        let mut dev = match try_get_nvme_device(st, blockio_handle)? {
            Some(nvme) => Either::Left(nvme),
            None => match try_get_ata_device(st, blockio_handle)? {
                Some(ata) => Either::Right(ata),
                None => continue,
            },
        };

        let serial = match &mut dev {
            Either::Left(nvme) => nvme.serial_num(),
            Either::Right(ata) => ata.serial(),
        };

        let serial = core::str::from_utf8(serial)
            .context("can't convert disk serial number to UTF8")?
            .trim();
        log::debug!("found disk with serial: `{}`", serial);

        let partition = match config.partitions.values().find(|part| part.uuid == serial) {
            Some(partition) => partition,
            None => continue,
        };

        // decrypt
        let keyslot = partition.keyslot.as_deref().unwrap();
        let keyslot = &config.keyslots[keyslot];
        match dev {
            Either::Left(nvme) => unlock_opal(st, opal::OpalDrive::new(RestartableNvmeDevice::new(&nvme, st, blockio_handle)).map_err(|e| Error::new(e, "open opal"))?, config, keyslot)?,
            Either::Right(ata) => unlock_opal(st, ata, config, keyslot)?,
        }
    }
    Ok(())
}

fn find_boot_partition(st: &SystemTable<Boot>) -> Result<Option<Handle>> {
    log::info!("reconnecting all controllers to hopefully make ParitionInfo show up");
    for (blockio_handle, _, _) in block_devices(st)? {
        let _ = st.boot_services().disconnect_controller(blockio_handle, None, None);
        if let Err(e) = st.boot_services().connect_controller(blockio_handle, None, None, true) {
            log::error!("rc error {e}");
        }
    }
    log::info!("done with rc");

    let mut res = None;
    for handle in st
        .boot_services()
        .find_handles::<PartitionInfo>()
        .map_err(|e| Error::new_from_uefi(e, "list partitions"))?
    {
        log::trace!("FBP iter");
        let pi = st.boot_services().open_protocol_exclusive::<PartitionInfo>(handle)
            .map_err(|e| Error::new_from_uefi(e, "open partinfo"))?;

        match pi.gpt_partition_entry() {
            Some(gpt) if { gpt.partition_type_guid } == GptPartitionType::EFI_SYSTEM_PARTITION => {
                if res.replace(handle).is_some() {
                    log::error!("multiple ESPs found :(");
                    return Ok(None);
                }
            }
            _ => {}
        }
    }

    Ok(res)
}
fn find_random_esp_path(st: &SystemTable<Boot>) -> Result<Option<Box<DevicePath>>> {
    let Some(part) = find_boot_partition(st)? else { return Ok(None) };
    let device_path = st
        .boot_services()
        .open_protocol_exclusive::<DevicePath>(part)
        .context("can't get DevicePath of BlockIO-Handle")?;

    let dp = device_path.to_string(st.boot_services(), DisplayOnly(true), AllowShortcuts(false))
        .map_err(|e| Error::new_without_source(format!("devicepath print error: {e}")))?.unwrap();
    log::info!("esp dp = {dp}");

    Ok(Some(device_path.to_boxed()))
}

fn handle_boot_entry(st: &SystemTable<Boot>, image_handle: Handle, config: &Config, boot_entry: &BootEntry) -> Result<()> {
    let BootEntry { name, file: efi_file, initrd, additional_initrd_files, options, default } = boot_entry;

    for part in &efi_file.extra_partitions {
        let partitions = [&config.partitions[part]];
        let _ = find_read_file(st, config, &partitions, &efi_file.file);
    }

    let efi_image = resolve_and_read_file(st, config, efi_file)?;
    if efi_image.get(0..2) != Some(&[0x4d, 0x5a]) {
        return Err(Error::new_without_source("image is not a valid PeCoff"));
    }

    let initramfs_addr = if initrd.is_some() || additional_initrd_files.is_some() {
        Some(construct_initramfs(st, config, initrd, additional_initrd_files)?)
    } else {
        None
    };

    let dev_path = find_random_esp_path(st)?;

    // LoadedImage

    let file_path = dev_path.as_ref().map(|x| x.as_ref());
    let loaded_image_handle = st
        .boot_services()
        .load_image(image_handle, LoadImageSource::FromBuffer { file_path, buffer: &efi_image })
        .context("can't get handle to new LoadedImage-to-boot")?;
    let mut loaded_image = st
        .boot_services()
        .open_protocol_exclusive::<LoadedImage>(loaded_image_handle)
        .context("error creating a LoadedImage from a LoadedImage-Handle")?;

    // chain-load efistub

    let mut options = options.clone().unwrap_or_default();
    if let Some((initramfs_addr, len)) = initramfs_addr {
        options.push_str(&format!(" initrdmem={initramfs_addr},{len}"));
    }
    log::debug!("passing options: `{options}`");
    let options = CString16::try_from(&*options)
        .context("efi image name is not valid UTF-16")?;
    unsafe { loaded_image.set_load_options(options.as_ptr() as *const u8, options.num_bytes() as _) };

    if config.log_level >= LevelFilter::Debug {
        log::debug!("loading image {} on \"uiae\" + Enter", efi_file.file);
        loop {
            if ui::line(st).unwrap() == "uiae" {
                break;
            }
        }
    }

    st.boot_services()
        .start_image(loaded_image_handle)
        .context("error booting loaded bootimage")?;

    Ok(())
}

fn construct_initramfs(st: &SystemTable<Boot>, config: &Config, initrd: &Option<Initrd>, additional_initrd_files: &Option<Vec<AdditionalInitrdFile>>) -> Result<(u64, usize)> {
    let mut initramfs = Initramfs::new();

    for initrd in initrd.iter().flat_map(|initrd| initrd.iter()) {
        log::debug!("loading initrd file {}", initrd.file);
        let content = resolve_and_read_file(st, config, initrd)?;
        initramfs.add_raw_archive(content);
    }

    let mut additional_files = Archive::new();
    for AdditionalInitrdFile { source, target_file } in additional_initrd_files.iter().flatten() {
        log::debug!("loading additional initrd file {}", source.file);
        let content = resolve_and_read_file(st, config, source)?;
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
        .context("can't align memory for initramfs")?;
    let buffer = unsafe { slice::from_raw_parts_mut(initramfs_addr as *mut u8, num_pages * 4096) };
    buffer[..serialized.len()].copy_from_slice(&serialized);
    log::debug!("initramfs loaded");
    Ok((initramfs_addr, serialized.len()))
}

fn resolve_and_read_file(st: &SystemTable<Boot>, config: &Config, file: &File) -> Result<Vec<u8>> {
    log::info!("fetching `{}` from `{}`", file.file, file.partition);
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
    let res = find_read_file(st, config, &partitions, &file.file);
    log::info!("fetched `{}` from `{}`", file.file, file.partition);
    res
}

fn block_devices(st: &SystemTable<Boot>) -> Result<Vec<(Handle, Lba, Lba)>> {
    Ok(st.boot_services().find_handles::<BlockIO>()
        .context("error getting list of BlockIO Handles")?
        .into_iter()
        .filter_map(|handle| {
            let blockio = st.boot_services().open_protocol_exclusive::<BlockIO>(handle).ok()?;
            let media = blockio.media();
            let start_lba = media.lowest_aligned_lba();
            let end_lba = media.last_block();
            match media.is_logical_partition() {
                true => None,
                false => Some((handle, start_lba, end_lba))
            }
        }).collect())
}

fn try_get_nvme_device(st: &SystemTable<Boot>, blockio_handle: Handle) -> Result<Option<NvmeDevice>> {
    let device_path = st
        .boot_services()
        .open_protocol_exclusive::<DevicePath>(blockio_handle)
        .context("can't get DevicePath of BlockIO-Handle")?;
    let device_path = &mut &*device_path;

    let nvme_passthrough_handle = st.boot_services().locate_device_path::<NvmExpressPassthru>(device_path);
    match nvme_passthrough_handle {
        Ok(nvme) => {
            let mut nvme = st
                .boot_services()
                .open_protocol_exclusive::<NvmExpressPassthru>(nvme)
                .context("error creating NvmExpressPassthru handle")?;
            let nvme = unsafe { NvmeDevice::new(&mut *nvme) }
                .context("error creating NvmeDevice from NvmExpressPassthru-Handle")?;
            Ok(Some(nvme))
        },
        Err(_) => Ok(None),
    }
}

/// returns if it was already unlocked
fn unlock_opal<P: opal::SecureProtocol>(st: &SystemTable<Boot>, mut secure_device: opal::OpalDrive<P>, config: &Config, keyslot: &Keyslot) -> Result<()>
where opal::Error<P::Error>: Into<ErrorSource>
{
    if !secure_device.was_locked() {
        return Ok(());
    }

    let mut cached = Cache::Cached;
    loop {
        let password = get_password_of_keyslot(st, config, keyslot, cached)?;
        let password_or_raw = match keyslot.source {
            KeyslotSource::Stdin => PasswordOrRaw::Password(&password),
            KeyslotSource::File(_) => PasswordOrRaw::Raw(&password),
        };
        match secure_device.unlock(password_or_raw) {
            Ok(()) => break,
            Err(opal::Error::Opal { source: opal::OpalError::Status { code: opal::StatusCode::NOT_AUTHORIZED }, .. }) => {
                log::error!("Invalid Password, try again!");
            }
            Err(opal::Error::Opal { source: opal::OpalError::Status { code: opal::StatusCode::AUTHORITY_LOCKED_OUT }, .. }) => {
                let mut st = unsafe { st.unsafe_clone() };
                st.stdout()
                    .write_str("Too many bad tries, SED locked out, resetting in 10s..")
                    .unwrap();
                sleep(Duration::from_secs(10));
                st.runtime_services()
                    .reset(ResetType::COLD, Status::WARN_RESET_REQUIRED, None);
            }
            Err(e) => return Err(Error::new(e, "efi error trying to unlock device")),
        }
        cached = Cache::Discard;
    }
    Ok(())
}

fn find_read_file(st: &SystemTable<Boot>, config: &Config, mut partitions: &[&Partition], file: &str) -> Result<Vec<u8>> {
    for (i, (blockio_handle, start_lba, end_lba)) in block_devices(st)?.into_iter().enumerate() {
        log::debug!("probing blockio #{i} {start_lba:#x} - {end_lba:#x}");

        // probe OPAL
        if let Some(nvme) = try_get_nvme_device(st, blockio_handle)? {
            let serial = core::str::from_utf8(nvme.serial_num())
                .context("can't convert nvme serial number to UTF8")?
                .trim();
            log::debug!("found nvme with serial: `{}`", serial);

            if partitions[0].uuid == serial {
                // decrypt
                if partitions[0].keyslot.is_some() {
                    let keyslot = partitions[0].keyslot.as_deref().unwrap();
                    let keyslot = &config.keyslots[keyslot];
                    let secure_device = opal::OpalDrive::new(RestartableNvmeDevice::new(&nvme, st, blockio_handle)).unwrap();
                    unlock_opal(st, secure_device, config, keyslot)?;
                }
                partitions = &partitions[1..];
                if partitions.is_empty() {
                    return Err(Error::new_without_source("out of partitions"));
                }
            }
        }

        if let Some(mut ata) = try_get_ata_device(st, blockio_handle)? {
            let serial = core::str::from_utf8(ata.serial())
                .context("can't convert ATA serial number to UTF8")?
                .trim();
            log::debug!("found ATA with serial: `{}`", serial);

            if partitions[0].uuid == serial {
                // decrypt
                if partitions[0].keyslot.is_some() {
                    let keyslot = partitions[0].keyslot.as_deref().unwrap();
                    let keyslot = &config.keyslots[keyslot];
                    unlock_opal(st, ata, config, keyslot)?;
                }
                partitions = &partitions[1..];
                if partitions.is_empty() {
                    return Err(Error::new_without_source("out of partitions"));
                }
            }
        }

        // probe partitions and stuff
        // recreate blockio for borrow-checker
        let blockio = st.boot_services().open_protocol_exclusive::<BlockIO>(blockio_handle)
            .context("can't get BlockIO from BlockIO-Handle")?;
        if start_lba == 0 && end_lba == 0xffffffff && blockio.media().block_size() == 65535 {
            log::error!("Spurious blockio #{i} reports having 256 TiB of space, skipping");
            continue;
        }
        // ignore start_lba and always read from 0
        let reader = BlockIoReader::new(&*blockio, 0, end_lba);
        let mut reader = OptimizedSeek::new(reader);
        match find_read_file_internal(st, &mut reader, config, partitions, file) {
            Ok(file) => return Ok(file),
            Err(e) => log::trace!("file was not found on BlockIO #{i}: {e}"),
        }

    }

    Err(Error::new_without_source("file not found"))
}


fn find_read_file_internal(st: &SystemTable<Boot>, reader: &mut dyn ReadSeek, config: &Config, partitions: &[&Partition], file: &str) -> Result<Vec<u8>> {
    if partitions.is_empty() {
        return Err(Error::new(ErrorSource::FileNotFound, "empty partition tabe"));
    }
    let partition = partitions.first().copied().unwrap();

    match Lvm2::open(&mut *reader) {
        Ok(lvm2) if lvm2.pv_id() == partition.uuid => {
            log::debug!("{}: found lvm2 with correct pv_id {}", partition.name, partition.uuid);
            for lv in lvm2.lvs() {
                let mut open_lv = lvm2.open_lv(lv, &mut *reader);
                match find_read_file_internal(st, &mut open_lv, config, &partitions[1..], file) {
                    Ok(file) => return Ok(file),
                    Err(e) => log::trace!("error probing lv {}: {e}", lv.name()),
                }
            }
            return Err(Error::new(ErrorSource::FileNotFound, "no lv contains requested file"));
        },
        Ok(lvm2) => log::trace!("found lvm2 with wrong id; expected {}, got {}", partition.uuid, lvm2.pv_id()),
        Err(e) => log::trace!("error trying to parse lvm2: {e}"),
    }
    reader.rewind().context("can't rewind reader after lvm2 probe")?;

    let mut buf = [0u8; 4096];
    reader.read_exact(&mut buf).context("error reading luks header")?;
    reader.rewind().context("can't rewind reader after luks2-header read")?;
    match LuksHeader::from_slice(&buf) {
        Ok(header) if header.uuid() == partition.uuid => {
            log::debug!("{}: found luks with correct uuid {}", partition.name, partition.uuid);
            let keyslot = match &partition.keyslot {
                Some(name) => &config.keyslots[name],
                None => {
                    log::error!("{}: no keyslot defined for luks in `config.toml`", partition.name);
                    return Err(Error::new(ErrorSource::FileNotFound, format!("no keyslot defined for partition `{}`", partition.uuid)));
                }
            };

            let master_key = config.luks_masterkey_buffer.borrow().get(&partition.uuid).cloned();
            let mut luks = match master_key {
                Some(master_key) => LuksDevice::from_device_with_master_key(reader, master_key, 512)
                    .context("error opening luks2 with known master key")?,
                None => {
                    let mut cached = Cache::Cached;
                    let luks = loop {
                        let password = get_password_of_keyslot(st, config, keyslot, cached)?;
                        match LuksDevice::from_device(&mut *reader, &password, 512) {
                            Ok(luks) => break luks,
                            Err(LuksError::InvalidPassword) => log::error!("Invalid Password, try again!"),
                            Err(e) => return Err(e).context("error opening luks2 with password"),
                        }
                        reader.rewind().context("can't rewind reader after luks2 invalid password")?;
                        cached = Cache::Discard;
                    };
                    config.luks_masterkey_buffer.borrow_mut().insert(partition.uuid.clone(), luks.master_key());
                    luks
                }
            };
            match find_read_file_internal(st, &mut luks, config, &partitions[1..], file) {
                Ok(file) => return Ok(file),
                Err(e) => log::trace!("error probing luks: {e}"),
            }
            return Err(Error::new(ErrorSource::FileNotFound, "luks device didn't contain file"));
        }
        Ok(header) => log::trace!("found luks with wrong id; expected {}, got {}", partition.uuid, header.uuid()),
        Err(e) => log::trace!("error trying to parse luks: {e}"),
    }
    reader.rewind().context("can't rewind reader after luks2 probe")?;

    let options = ext4::Options { checksums: ext4::Checksums::Enabled };
    match SuperBlock::new_with_options(SeekWrapper::new(&mut *reader), &options) {
        Ok(ext4) if Uuid::from_slice(&ext4.uuid).unwrap().to_string() == partition.uuid => {
            log::debug!("{}: found ext4 with correct uuid {}", partition.name, partition.uuid);
            if partitions.len() != 1 {
                log::error!("{}: found ext4 with correct uuid {} but there are still inner partitions left", partition.name, partition.uuid);
                return Err(Error::new(ErrorSource::FileNotFound, "ext4 with correct uuid, but there are partitions left in path"));
            }
            let entry = ext4.resolve_path(file)
                .map_err(|_| Error::new(ErrorSource::FileNotFound, "can't find path in ext4 with correct uuid"))?;
            let inode = ext4.load_inode(entry.inode)
                .map_err(|_| Error::new(ErrorSource::FileNotFound,"can't load inode"))?;
            let mut reader = ext4.open(&inode).unwrap();
            let mut data = Vec::new();
            reader.read_to_end(&mut data).unwrap();
            return Ok(data);
        }
        Ok(ext4) => log::trace!("found ext4 with wrong id; expected {}, got {}", partition.uuid, Uuid::from_slice(&ext4.uuid).unwrap().to_string()),
        Err(e) => log::trace!("error trying to parse ext4: {e}"),
    }
    reader.rewind().context("can't rewind reader after ext4 probe")?;

    match fatfs::FileSystem::new(IgnoreWriteWrapper::new(&mut *reader), fatfs::FsOptions::new()) {
        Ok(fat) if partition.uuid == format!("{:X}-{:X}", fat.volume_id() >> 16, fat.volume_id() as u16) => {
            log::debug!("{}: found FAT with correct uuid {}", partition.name, partition.uuid);
            if partitions.len() != 1 {
                log::error!("{}: found FAT with correct uuid {} but there are still inner partitions left", partition.name, partition.uuid);
                return Err(Error::new(ErrorSource::FileNotFound, "FAT with correct uuid, but there are partitions left in path"));
            }
            let mut file = fat.root_dir().open_file(file).context("error opening file in FAT")?;
            log::trace!("start reading file");
            let mut data = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                use fatfs::Read as _;
                let read = file.read(&mut buf).context("error reading file in FAT")?;
                if read == 0 { break }
                data.extend_from_slice(&buf[..read]);
            }
            log::trace!("file read");
            return Ok(data)
        }
        Ok(fat) => log::trace!("found FAT with wrong id; expected {}, got {}", partition.uuid, format!("{:X}-{:X}", fat.volume_id() >> 16, fat.volume_id() as u16)),
        Err(e) => log::trace!("error trying to parse fat: {e:?}"),
    }
    reader.rewind().context("can't rewind reader after FAT probe")?;

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
                    Err(e) => log::trace!("error probing gpt partition: {e}"),
                }
            }

            return Err(Error::new(ErrorSource::FileNotFound, "file not found in any gpt partition"));
        },
        Err(e) => log::trace!("error trying to parse gpt: {e:?}"),
    }
    reader.rewind().context("can't rewind reader after gpt probe")?;

    Err(Error::new(ErrorSource::FileNotFound, "file not found on this device"))
}

#[derive(Debug, Copy, Clone)]
enum Cache {
    Cached,
    Discard,
}

fn get_password_of_keyslot(st: &SystemTable<Boot>, config: &Config, keyslot: &Keyslot, cached: Cache) -> Result<Vec<u8>> {
    // we can't use entry API here as we need to drop the borrow when searching for keyfiles
    // in case those are again on an encrypted partition
    match cached {
        Cache::Cached => if let Some(key) = config.keyslot_buffer.borrow().get(&keyslot.name) {
            return Ok(key.clone());
        },
        Cache::Discard => (),
    }

    let password = match &keyslot.source {
        KeyslotSource::Stdin => {
            let mut st = unsafe { st.unsafe_clone() };
            st.stdout().write_str(&format!("Password for keyslot {}: ", keyslot.name)).unwrap();
            ui::password(&st)?.into_bytes()
        },
        KeyslotSource::File(file) => {
            resolve_and_read_file(st, config, file)?
        }
    };
    config.keyslot_buffer.borrow_mut().insert(keyslot.name.clone(), password.clone());
    Ok(password)
}

fn config_stdout(st: &SystemTable<Boot>) -> uefi::Result {
    let mut st = unsafe { st.unsafe_clone() };
    st.stdout().reset(false)?;
    st.stdout().clear()?;

    if let Some(mode) = st.stdout().modes().max_by_key(|m| {
        m.rows() * m.columns()
    // if let Some(mode) = st.stdout().modes().min_by_key(|m| {
    //     (m.rows() as i32 * m.columns() as i32 - 200*64).abs()
    }) {
        log::trace!("selected {mode:?}");
        st.stdout().set_mode(mode)?;
    };

    Ok(())
}

fn find_boot_partitions(st: &SystemTable<Boot>) -> Result<Vec<(GptPartitionEntry, Handle)>> {
    let mut res = Vec::new();
    let handles = st.boot_services().find_handles::<PartitionInfo>()
        .context("error getting all partition handles")?;
    for handle in handles {
        let pi = st
            .boot_services()
            .open_protocol_exclusive::<PartitionInfo>(handle)
            .context("can't get partition info from handle")?;

        match pi.gpt_partition_entry() {
            Some(gpt) if { gpt.partition_type_guid } == GptPartitionType::EFI_SYSTEM_PARTITION => {
                res.push((*gpt, handle));
            }
            _ => {}
        }
    }
    Ok(res)
}

fn find_efi_files(st: &SystemTable<Boot>, partition: Handle) -> Result<Vec<String>> {
    let mut sfs = st
        .boot_services()
        .open_protocol_exclusive::<SimpleFileSystem>(partition)
        .context("can't get SimpleFileSystem from partition handle")?;

    let dir = sfs.open_volume()
        .context("can't open volume of SimpleFileSystem")?;
    let mut files = Vec::new();
    find_efi_files_rec(st, &mut files, String::from(""), partition, dir)?;
    Ok(files)
}

fn find_efi_files_rec(st: &SystemTable<Boot>, files: &mut Vec<String>, prefix: String, partition: Handle, mut directory: Directory) -> Result<()> {
    let mut buf = vec![0; 1024];
    let buf = FileInfo::align_buf(&mut buf).unwrap();
    let dir_info = directory.get_info::<FileInfo>(buf)
        .context("can't get FileInfo from directory")?;
    let name = dir_info.file_name().to_string();
    let prefix = format!("{prefix}{name}\\");
    log::trace!("visiting directory {prefix}");

    loop {
        let mut buf = vec![0; 1024];
        let buf = FileInfo::align_buf(&mut buf).unwrap();
        let entry = directory.read_entry(buf).context("can't read directory entry")?;
        let entry = match entry {
            Some(entry) => entry,
            None => break,
        };
        let name = entry.file_name().to_string();
        if name == "." || name == ".." {
            continue;
        }
        let file_handle = directory.open(entry.file_name(), FileMode::Read, FileAttribute::empty())
            .context(format!("can't open file {}", entry.file_name()))?;
        let file_type = file_handle.into_type().context(format!("can't get file type of file {}", entry.file_name()))?;
        match file_type {
            FileType::Regular(_) => {
                let filename = format!("{prefix}{name}");
                log::trace!("found file {filename}");
                let filename_cstr16 = CString16::try_from(&*filename).context("file name not UTF-16 compatible")?;
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
