use alloc::{alloc::alloc, boxed::Box};
use alloc::vec::Vec;
use core::{alloc::Layout, mem::MaybeUninit, time::Duration};
use uefi::{CStr16, Handle};
use uefi::proto::media::file::{File, FileAttribute, FileInfo, FileMode, FileType};
use uefi::proto::media::fs::SimpleFileSystem;
use uefi::table::{Boot, SystemTable};

pub fn sleep(duration: Duration) {
    // untie the sleep function from the system table
    // so that it can be used in the opal lib
    let bt = unsafe { uefi_services::system_table().as_ref() }.boot_services();
    // duration.as_nanos() works with u128 which is unsupported lol
    let nanos = duration.as_secs() * 1_000_000_000 + duration.subsec_nanos() as u64;
    bt.stall((nanos / 1000) as usize);
}

pub unsafe fn alloc_uninit_aligned(len: usize, align: usize) -> Box<[MaybeUninit<u8>]> {
    let ptr = alloc(Layout::from_size_align(len, align).unwrap()) as _;
    Box::from_raw(core::slice::from_raw_parts_mut(ptr, len))
}

pub fn read_file(
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
        let mut buf = vec![0; size];

        let read = f
            .read(&mut buf)
            .map_err(|_| uefi::Status::BUFFER_TOO_SMALL)?;
        buf.truncate(read);
        Ok(Some(buf).into())
    } else {
        Ok(None.into())
    }
}
