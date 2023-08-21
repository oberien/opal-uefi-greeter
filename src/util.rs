use alloc::{alloc::alloc, boxed::Box};
use alloc::vec::Vec;
use core::{alloc::Layout, mem::MaybeUninit, time::Duration};
use uefi::{CStr16, Handle, Status};
use uefi::proto::media::file::{File, FileAttribute, FileInfo, FileMode, FileType};
use uefi::proto::media::fs::SimpleFileSystem;
use uefi::table::{Boot, SystemTable};
use uefi::table::boot::{EventType, TimerTrigger, Tpl};
use crate::{Error, Result, Context};

pub fn sleep(duration: Duration) {
    // duration.as_nanos() works with u128 which is unsupported on some devices lol
    // let nanos = (duration.as_nanos() / 100) as u64;
    let nanos = duration.as_secs() * 1_000_000_000 + duration.subsec_nanos() as u64;

    // untie the sleep function from the system table
    // so that it can be used in the opal lib
    let bt = unsafe { uefi_services::system_table().as_ref() }.boot_services();

    let res = unsafe { bt.create_event(EventType::TIMER, Tpl::APPLICATION, None, None) };
    let event = match res {
        Ok(event) => event,
        Err(err) if err.status() == Status::INVALID_PARAMETER => {
            log::error!("Mainboard doesn't support Timer-Event -> falling back to stall");
            bt.stall((nanos / 1000) as usize);
            return;
        }
        Err(e) => Err(e).unwrap(),
    };
    bt.set_timer(&event, TimerTrigger::Periodic(nanos / 100)).unwrap();

    bt.wait_for_event(&mut [event]).unwrap();
}

pub unsafe fn alloc_init_aligned(len: usize, align: usize) -> Box<[u8]> {
    let ptr = alloc(Layout::from_size_align(len, align).unwrap()) as _;
    core::ptr::write_bytes(ptr, 0, len);
    Box::from_raw(core::slice::from_raw_parts_mut(ptr, len))
}

pub unsafe fn alloc_aligned_t<T>(t: T, align: usize) -> Box<T> {
    let ptr = alloc(Layout::from_size_align(core::mem::size_of::<T>(), align).unwrap()) as _;
    core::ptr::write(ptr, t);
    Box::from_raw(ptr)
}

pub unsafe fn alloc_uninit_aligned(len: usize, align: usize) -> Box<[MaybeUninit<u8>]> {
    let ptr = alloc(Layout::from_size_align(len, align).unwrap()) as _;
    Box::from_raw(core::slice::from_raw_parts_mut(ptr, len))
}

pub fn read_full_file(
    st: &SystemTable<Boot>,
    device: Handle,
    file: &CStr16,
) -> Result<Vec<u8>> {
    let mut vec = Vec::new();
    read_full_file_to_vec(st, device, file, &mut vec)?;
    Ok(vec)
}
/// resizes the vector to fit the whole file
pub fn read_full_file_to_vec(
    st: &SystemTable<Boot>,
    device: Handle,
    file: &CStr16,
    vec: &mut Vec<u8>,
) -> Result<()> {
    read_to_vec(st, device, file, vec, true).map(|_| ())
}

/// reads into the existing vector-length; does not resize the vector
pub fn read_partial_file_to_vec(
    st: &SystemTable<Boot>,
    device: Handle,
    file: &CStr16,
    vec: &mut Vec<u8>,
) -> Result<usize> {
    read_to_vec(st, device, file, vec, false)
}

fn read_to_vec(
    st: &SystemTable<Boot>,
    device: Handle,
    file: &CStr16,
    vec: &mut Vec<u8>,
    full: bool,
) -> Result<usize> {
    let mut sfs = st
        .boot_services()
        .open_protocol_exclusive::<SimpleFileSystem>(device)
        .context(format!("can't get SimpleFileSystem from device to get file {}", file))?;

    let file_handle = sfs
        .open_volume().context(format!("can't open SimpleFileSystem to get file {}", file))?
        .open(file, FileMode::Read, FileAttribute::empty())
        .context(format!("can't open file {}", file))?;

    let file_type = file_handle.into_type()
        .context(format!("error converting file handle to file type for file {}", file))?;
    if let FileType::Regular(mut f) = file_type {
        if full {
            let info = f.get_boxed_info::<FileInfo>()
                .context(format!("can't get file info for file {}", file))?;
            let size = info.file_size() as usize;
            vec.resize(size, 0);
        }

        let read = f
            .read(vec)
            .map_err(|_| uefi::Error::new(uefi::Status::BUFFER_TOO_SMALL, ()))
            .context(format!("error reading from file {}", file))?;
        vec.truncate(read);
        Ok(read)
    } else {
        Err(Error::new_without_source(format!("file {} note found", file)))
    }
}
