use alloc::boxed::Box;
use alloc::string::String;
use bitflags::bitflags;
use opal::SecureProtocol;
use uefi::table::{SystemTable, Boot};
use uefi::table::boot::ScopedProtocol;
use uefi::{Event, StatusExt, Handle};
use uefi::proto::device_path::{FfiDevicePath, DevicePath};
use uefi::proto::unsafe_protocol;
use uefi_raw::Status;

use crate::error::Error;
use crate::low_level::nvme_device::UefiError;
use crate::util::{alloc_init_aligned, alloc_aligned_t};

pub struct AtaProtocol<'a> {
    passthru: ScopedProtocol<'a, AtaPassthru>,
    port: u16,
    port_multiplier_port: u16,
    serial: [u8; 20],

    st: &'a SystemTable<Boot>,
    handle: Handle,
}
impl<'a> AtaProtocol<'a> {
    pub fn try_make(passthru: ScopedProtocol<'a, AtaPassthru>, devpath: &DevicePath, st: &'a SystemTable<Boot>, handle: Handle) -> Result<Self, Error> {
        let mut port = 0;
        let mut port_multiplier_port = 0;
        unsafe {
            (passthru.get_device)(&*passthru, devpath.as_ffi_ptr(), &mut port, &mut port_multiplier_port).to_result().map_err(|e| Error::new_from_uefi(e, "error mapping device"))?;
        }
        log::info!("port={port} pmp={port_multiplier_port}");
        //let (port, port_multiplier_port) = passthru.find_first_dev().map_err(|e| Error::new_from_uefi(e, "find first dev"))?;
        let serial = passthru.get_serial_num(port, port_multiplier_port).map_err(|e| Error::new_from_uefi(e, "get serial num"))?;
        log::info!("serial = {}", String::from_utf8_lossy(&serial));
        Ok(Self {
            passthru,
            port,
            port_multiplier_port,
            serial,
            st,
            handle,
        })
    }
}
impl<'a> SecureProtocol for AtaProtocol<'a> {
    type Error = UefiError;

    unsafe fn secure_send(&mut self, protocol: u8, cmd_id: u16, data: &mut [u8]) -> Result<(), Self::Error> {
        self.passthru.do_io(self.port, self.port_multiplier_port, IoMode::Send { protocol, cmd_id, data }).map_err(|error| UefiError { error })?;
        Ok(())
    }

    unsafe fn secure_recv(
        &mut self,
        protocol: u8,
        cmd_id: u16,
        buffer: &mut [u8],
    ) -> Result<(), Self::Error> {
        let data = self.passthru.do_io(self.port, self.port_multiplier_port, IoMode::Recv { protocol, cmd_id }).map_err(|error| UefiError { error })?;
        let s = core::cmp::min(data.len(), buffer.len());
        buffer[..s].copy_from_slice(&data[..s]);
        Ok(())
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

    fn align(&self) -> usize {
        unsafe { (*self.passthru.mode).io_align as usize }
    }

    fn serial_num(&self) -> &[u8] {
        &self.serial
    }
}

#[unsafe_protocol("1d3de7f0-0807-424f-aa69-11a54e19a46f")]
#[repr(C)]
pub struct AtaPassthru {
    mode: *const Mode,
    pass_thru: unsafe extern "efiapi" fn(
        this: &AtaPassthru,
        port: u16,
        port_multiplier_port: u16,
        packet: &mut CommandPacket,
        event: Option<Event>,
    ) -> Status,
    get_next_port: unsafe extern "efiapi" fn(
        this: &AtaPassthru,
        port: &mut u16,
    ) -> Status,
    get_next_device: unsafe extern "efiapi" fn(
        this: &AtaPassthru,
        port: u16,
        port_multiplier_port: &mut u16,
    ) -> Status,
    build_device_path: unsafe extern "efiapi" fn(
        this: &AtaPassthru,
        port: u16,
        port_multiplier_port: u16,
        device_path: &mut *mut FfiDevicePath,
    ) -> Status,
    pub get_device: unsafe extern "efiapi" fn(
        this: &AtaPassthru,
        device_path: *const FfiDevicePath,
        port: &mut u16,
        port_multiplier_port: &mut u16,
    ) -> Status,
    reset_port: unsafe extern "efiapi" fn(
        this: &AtaPassthru,
        port: u16,
    ) -> Status,
    reset_device: unsafe extern "efiapi" fn(
        this: &AtaPassthru,
        port: u16,
        port_multiplier_port: u16,
    ) -> Status,
}

#[repr(u8)]
#[derive(Clone, Copy, Default)]
pub enum AtaCommand {
    #[default]
    Identify = 0xec,
    IfRecv = 0x5c,
    IfSend = 0x5e,
}

#[derive(Clone, Copy)]
enum IoMode<'a> {
    Identify,
    Recv { protocol: u8, cmd_id: u16 },
    Send {
        protocol: u8,
        cmd_id: u16,
        data: &'a [u8],
    }
}

impl AtaPassthru {
    // https://edk2.groups.io/g/devel/message/22393
    unsafe fn do_io(&self, port: u16, port_multiplier_port: u16, mode: IoMode) -> uefi::Result<Box<[u8]>> {
        let align = (*self.mode).io_align as usize;
        let asb = alloc_aligned_t(AtaStatusBlock::default(), align);

        let command = match mode {
            IoMode::Identify => AtaCommand::Identify,
            IoMode::Recv { .. } => AtaCommand::IfRecv,
            IoMode::Send { .. } => AtaCommand::IfSend,
        };
        let mut acb = AtaCommandBlock {
            command,
            device_head: (1<<7) | (1<<6) | (1<<5) | ((port_multiplier_port << 4) as u8), // ???????
            ..Default::default()
        };
        match mode {
            IoMode::Identify => (),
            IoMode::Recv { protocol, cmd_id } | IoMode::Send { protocol, cmd_id, .. } => {
                acb.features = protocol;
                /*
                acb.cylinder_high = cmd_id as u8;
                acb.cylinder_low = (cmd_id >> 8) as u8;
                */
                acb.cylinder_high = (cmd_id >> 8) as u8;
                acb.cylinder_low = cmd_id as u8;
                acb.device_head = 0x40;
            }
        }

        let protocol = match mode {
            IoMode::Identify | IoMode::Recv { .. } => AtaPassthruProtocol::PioDataIn,
            IoMode::Send { .. } => AtaPassthruProtocol::PioDataOut,
        };

        let mut return_data = alloc_init_aligned(2048, align);
        let mut packet = CommandPacket {
            protocol,
            length: AtaPassthruLength::BYTES | AtaPassthruLength::SECTOR_COUNT,
            in_data_buffer: return_data.as_mut_ptr(),
            in_transfer_length: return_data.len() as u32,
            out_data_buffer: core::ptr::null_mut(),
            out_transfer_length: 0,
            timeout: 3 * 10000000,
            asb: &asb,
            acb: &acb,
        };

        let _out_buf = match mode {
            IoMode::Send { data, .. } => {
                let rounded_len = ((data.len() + 512 - 1) / 512) * 512;
                let mut out_buf = alloc_init_aligned(rounded_len, align);
                out_buf[..data.len()].copy_from_slice(data);
                packet.out_data_buffer = out_buf.as_mut_ptr();
                packet.out_transfer_length = rounded_len as u32;
                Some(out_buf)
            }
            _ => None,
        };

        (self.pass_thru)(self, port, port_multiplier_port, &mut packet, None).to_result()?;
        Ok(return_data)
    }

    pub fn find_first_dev(&self) -> uefi::Result<(u16, u16)> {
        unsafe {
            let mut port = 0xFFFF;
            (self.get_next_port)(self, &mut port).to_result()?;
            let mut port_multiplier_port = 0xFFFF;
            (self.get_next_device)(self, port, &mut port_multiplier_port).to_result()?;
            Ok((port, port_multiplier_port))
        }
    }

    pub fn get_serial_num(&self, port: u16, port_multiplier_port: u16) -> uefi::Result<[u8; 20]> {
        unsafe {
            let identify_data = self.do_io(port, port_multiplier_port, IoMode::Identify)?;

            #[repr(C)]
            #[derive(Debug)]
            pub struct IdentifyResponse {
                reserved0: u8,
                reserved1: u8,
                reserved2: [u8; 18],
                serial_num: [u8; 20],
                reserved3: [u8; 6],
                firmware_rev: [u8; 8],
                model_num: [u8; 40],
            }
            fn byteswap(v: &mut [u8]) {
                for c in v.chunks_exact_mut(2) {
                    c.swap(0, 1);
                }
            }

            let identify = &*(identify_data.as_ptr() as *const IdentifyResponse);
            /*
            let serial = byteswap_to_string(&identify.serial_num);
            let firmware = byteswap_to_string(&identify.firmware_rev);
            let model_num = byteswap_to_string(&identify.model_num);

            log::error!("serial={serial} firmware={firmware} model_num={model_num}");
            */
            let mut serial = identify.serial_num;
            byteswap(&mut serial);

            Ok(serial)
        }
    }
}

#[derive(Debug)]
#[repr(C)]
pub struct Mode {
    pub attributes: Attributes,
    pub io_align: u32,
}

bitflags! {
    #[repr(transparent)]
    pub struct Attributes: u32 {
        const PHYSICAL    = 0x01;
        const LOGICAL     = 0x02;
        const NONBLOCKIO  = 0x04;
    }
}

#[repr(C)]
pub struct CommandPacket<'a> {
    pub asb: &'a AtaStatusBlock,
    pub acb: &'a AtaCommandBlock,
    pub timeout: u64,
    pub in_data_buffer: *mut u8,
    pub out_data_buffer: *mut u8,
    pub in_transfer_length: u32,
    pub out_transfer_length: u32,
    pub protocol: AtaPassthruProtocol,
    pub length: AtaPassthruLength,
}

#[repr(u8)]
pub enum AtaPassthruProtocol {
    PioDataIn = 0x4,
    PioDataOut = 0x5,
    // others not relevant
}

bitflags! {
    #[repr(transparent)]
    pub struct AtaPassthruLength: u8 {
        const NO_DATA_TRANSFER = 0;
        const BYTES = 0x80;
        const SECTOR_COUNT = 0x20;
    }
}


#[repr(C)]
#[derive(Default)]
pub struct AtaStatusBlock {
    pub reserved1: [u8; 2],
    pub status: u8,
    pub error: u8,
    pub sector_number: u8,
    pub cylinder_low: u8,
    pub cylinder_high: u8,
    pub device_head: u8,
    pub sector_number_exp: u8,
    pub cylinder_low_exp: u8,
    pub cylinder_high_exp: u8,
    pub reserved2: u8,
    pub sector_count: u8,
    pub sector_count_exp: u8,
    pub reserved3: [u8; 6],
}

#[repr(C)]
#[derive(Default)]
pub struct AtaCommandBlock {
    pub reserved1: [u8; 2],
    pub command: AtaCommand,
    pub features: u8,
    pub sector_number: u8,
    pub cylinder_low: u8,
    pub cylinder_high: u8,
    pub device_head: u8,
    pub sector_number_exp: u8,
    pub cylinder_low_exp: u8,
    pub cylinder_high_exp: u8,
    pub features_exp: u8,
    pub sector_count: u8,
    pub sector_count_exp: u8,
    pub reserved2: [u8; 6],
}
