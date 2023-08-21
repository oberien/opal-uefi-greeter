use alloc::vec::Vec;
use snafu::Snafu;
use uefi::table::{SystemTable, Boot};
use core::mem::MaybeUninit;

use uefi::{Status, StatusExt, Handle};

use crate::low_level::nvme_passthru::{self, Command, CommandPacket, NvmExpressPassthru, QueueType, SendTarget};
use opal::SecureProtocol;

pub struct NvmeDevice {
    passthru: *mut NvmExpressPassthru,
    align: usize,
    serial_num: Vec<u8>,
}

pub struct RestartableNvmeDevice<'a> {
    dev: &'a NvmeDevice,
    st: &'a SystemTable<Boot>,
    handle: Handle,
}

impl NvmeDevice {
    pub unsafe fn new(passthru: *mut NvmExpressPassthru) -> uefi::Result<NvmeDevice> {
        let serial_num = recv_serial_num(passthru)?;
        let align = unsafe { &mut *passthru }.mode().io_align as _;
        Ok(Self {
            passthru,
            align,
            serial_num,
        })
    }

    pub fn serial_num(&self) -> &[u8] {
        &self.serial_num
    }
}

fn recv_serial_num(passthru: *mut NvmExpressPassthru) -> uefi::Result<Vec<u8>> {
    let passthru = unsafe { &mut *passthru };
    let mut data =
        unsafe { crate::util::alloc_uninit_aligned(4096, passthru.mode().io_align as usize) };
    let command = Command::new(0x06).cdw_10(1);
    let mut packet = CommandPacket::new(
        nvme_passthru::NVME_GENERIC_TIMEOUT,
        Some(&mut data),
        None,
        QueueType::ADMIN,
        &command,
    );

    unsafe { passthru.send(SendTarget::Controller, &mut packet) }?;

    let serial_num = unsafe { core::slice::from_raw_parts(data.as_ptr().offset(4) as *const u8, 20) };
    //let serial_num = unsafe { MaybeUninit::slice_assume_init_ref(&data[4..24]) };
    Ok(serial_num.to_vec())
}

#[repr(u8)]
enum Direction {
    Send = 0x81,
    Recv = 0x82,
}

unsafe fn secure_protocol(
    passthru: *mut NvmExpressPassthru,
    direction: Direction,
    protocol: u8,
    com_id: u16,
    buffer: &mut [MaybeUninit<u8>],
) -> uefi::Result {
    let command = Command::new(direction as u8)
        .cdw_10((protocol as u32) << 24 | (com_id as u32) << 8)
        .cdw_11(buffer.len() as u32);

    let mut packet = CommandPacket::new(
        nvme_passthru::NVME_GENERIC_TIMEOUT,
        Some(buffer),
        None,
        QueueType::ADMIN,
        &command,
    );
    (*passthru).send(SendTarget::Controller, &mut packet)?;

    Status::SUCCESS.to_result()
}

#[derive(Debug, Snafu)]
pub struct UefiError {
    pub error: uefi::Error<()>,
}

impl<'a> RestartableNvmeDevice<'a> {
    pub fn new(dev: &'a NvmeDevice, st: &'a SystemTable<Boot>, handle: Handle) -> Self {
        Self { dev, st, handle }
    }
}
impl<'a> SecureProtocol for RestartableNvmeDevice<'a> {
    type Error = UefiError;

    unsafe fn secure_send(&mut self, protocol: u8, com_id: u16, data: &mut [u8]) -> Result<(), UefiError> {
        secure_protocol(
            self.dev.passthru,
            Direction::Send,
            protocol,
            com_id,
            core::slice::from_raw_parts_mut(data.as_mut_ptr() as _, data.len()),
        ).map_err(|error| UefiError { error })
    }

    unsafe fn secure_recv(
        &mut self,
        protocol: u8,
        com_id: u16,
        buffer: &mut [u8],
    ) -> Result<(), UefiError> {
        secure_protocol(
            self.dev.passthru,
            Direction::Recv,
            protocol,
            com_id,
            core::slice::from_raw_parts_mut(buffer.as_mut_ptr() as _, buffer.len()),
        ).map_err(|error| UefiError { error })
    }

    fn align(&self) -> usize {
        self.dev.align
    }

    fn serial_num(&self) -> &[u8] {
        &self.dev.serial_num
    }

    fn reconnect_controller(&mut self) -> Result<(), Self::Error> {
        self.st.boot_services()
            .disconnect_controller(self.handle, None, None)
            .map_err(|error| UefiError { error })?;
        self.st.boot_services()
            .connect_controller(self.handle, None, None, true)
            .map_err(|error| UefiError { error })?;
        Ok(())
    }
}
