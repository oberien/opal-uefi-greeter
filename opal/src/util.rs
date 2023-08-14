use alloc::alloc::Layout;
use alloc::boxed::Box;

pub fn alloc_aligned(len: usize, align: usize) -> Box<[u8]> {
    unsafe {
        let ptr = alloc::alloc::alloc(Layout::from_size_align(len, align).unwrap()) as _;
        core::ptr::write_bytes(ptr, 0, len);
        Box::from_raw(core::slice::from_raw_parts_mut(ptr, len))
    }
}