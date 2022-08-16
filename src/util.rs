use alloc::{alloc::alloc, boxed::Box};
use alloc::vec::Vec;
use core::{alloc::Layout, mem::MaybeUninit, time::Duration};
use uefi::{CStr16, Handle};
use uefi::proto::media::file::{File, FileAttribute, FileInfo, FileMode, FileType};
use uefi::proto::media::fs::SimpleFileSystem;
use uefi::table::{Boot, SystemTable};
use uefi::table::boot::{EventType, TimerTrigger, Tpl};
use crate::{Error, info, Result, ResultFixupExt};

pub fn sleep(duration: Duration) {
    // untie the sleep function from the system table
    // so that it can be used in the opal lib
    let bt = unsafe { uefi_services::system_table().as_ref() }.boot_services();

    let event = unsafe { bt.create_event(EventType::TIMER, Tpl::APPLICATION, None, None).unwrap() };
    // duration.as_nanos() works with u128 which is unsupported lol
    // let nanos = duration.as_secs() * 1_000_000_000 + duration.subsec_nanos() as u64;
    let nanos = duration.as_nanos() / 100;
    bt.set_timer(&event, TimerTrigger::Relative(nanos as u64)).unwrap();

    bt.wait_for_event(&mut [event]).unwrap();
}

pub unsafe fn alloc_uninit_aligned(len: usize, align: usize) -> Box<[MaybeUninit<u8>]> {
    let ptr = alloc(Layout::from_size_align(len, align).unwrap()) as _;
    Box::from_raw(core::slice::from_raw_parts_mut(ptr, len))
}

pub fn read_full_file(
    st: &mut SystemTable<Boot>,
    device: Handle,
    file: &CStr16,
) -> Result<Vec<u8>> {
    let mut vec = Vec::new();
    read_full_file_to_vec(st, device, file, &mut vec)?;
    Ok(vec)
}
/// resizes the vector to fit the whole file
pub fn read_full_file_to_vec(
    st: &mut SystemTable<Boot>,
    device: Handle,
    file: &CStr16,
    vec: &mut Vec<u8>,
) -> Result<()> {
    read_to_vec(st, device, file, vec, true).map(|_| ())
}

/// reads into the existing vector-length; does not resize the vector
pub fn read_partial_file_to_vec(
    st: &mut SystemTable<Boot>,
    device: Handle,
    file: &CStr16,
    vec: &mut Vec<u8>,
) -> Result<usize> {
    read_to_vec(st, device, file, vec, false)
}

fn read_to_vec(
    st: &mut SystemTable<Boot>,
    device: Handle,
    file: &CStr16,
    vec: &mut Vec<u8>,
    full: bool,
) -> Result<usize> {
    let sfs = st
        .boot_services()
        .handle_protocol::<SimpleFileSystem>(device)
        .fix(info!())?;
    let sfs = unsafe { &mut *sfs.get() };

    let file_handle = sfs
        .open_volume().fix(info!())?
        .open(&file, FileMode::Read, FileAttribute::empty())
        .fix(info!())?;

    if let FileType::Regular(mut f) = file_handle.into_type().fix(info!())? {
        if full {
            let info = f.get_boxed_info::<FileInfo>().fix(info!())?;
            let size = info.file_size() as usize;
            vec.resize(size, 0);
        }

        let read = f
            .read(vec)
            .map_err(|_| uefi::Error::new(uefi::Status::BUFFER_TOO_SMALL, ()))
            .fix(info!())?;
        vec.truncate(read);
        Ok(read)
    } else {
        Err(Error::FileNotFound)
    }
}
