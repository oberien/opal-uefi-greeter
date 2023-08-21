use alloc::fmt::{Debug, Display};

use snafu::{ResultExt, AsErrorSource};
use uefi_raw::newtype_enum;

pub trait SecureProtocol {
    type Error: Debug + Display + AsErrorSource;

    /// # Safety
    /// This method allows to send arbitrary secure protocol commands to the underlying device.
    /// Very unsafe and might even brick a device if used incorrectly.
    unsafe fn secure_send(&mut self, protocol: u8, com_id: u16, data: &mut [u8]) -> Result<(), Self::Error>;

    unsafe fn secure_recv(
        &mut self,
        protocol: u8,
        com_id: u16,
        buffer: &mut [u8],
    ) -> Result<(), Self::Error>;

    fn reconnect_controller(&mut self) -> Result<(), Self::Error>;

    fn align(&self) -> usize;

    fn serial_num(&self) -> &[u8];
}

newtype_enum! {
    pub enum FeatureCodes: u16 => {
        // TPER       = 0x0001,
        LOCKING    = 0x0002,
        // GEOMETRY   = 0x0003,
        ENTERPRISE = 0x0100,
        // DATASTORE  = 0x0202,
        // SINGLEUSER = 0x0201,
        // OPAL_V1    = 0x0200,
        OPAL_V2    = 0x0203,
    }
}

bitflags::bitflags! {
    #[derive(Debug)]
    pub struct LockingFlags: u8 {
        const LOCKING_SUPPORTED = 0x01;
        const LOCKING_ENABLED   = 0x02;
        const LOCKED            = 0x04;
        const MEDIA_ENCRYPTION  = 0x08;
        const MBR_ENABLED       = 0x10;
        const MBR_DONE          = 0x20;
    }
}

#[derive(Debug)]
pub struct SecureDeviceInfo {
    pub locking: Option<LockingFlags>,
    pub opal_v2: Option<ComIdInfo>,
    pub enterprise: Option<ComIdInfo>,
}

#[derive(Debug)]
pub struct ComIdInfo {
    pub base_com_id: u16,
    pub num_com_ids: u16,
}

pub struct SecureDevice<P> {
    device: P,
    com_id: u16,
    is_eprise: bool,
    was_locked: bool,
}

impl<P: SecureProtocol> SecureDevice<P> {
    pub fn new(mut device: P) -> crate::Result<Self, P::Error> {
        let info = recv_info(&mut device)?;
        tracing::debug!(?info);
        let is_eprise = info.enterprise.is_some();
        let com_id = match info.enterprise.or(info.opal_v2) {
            Some(x) => x,
            None => super::UnsupportedSnafu.fail()?,
        }
        .base_com_id;
        Ok(Self {
            device,
            com_id,
            is_eprise,
            was_locked: info.locking.map_or(false, |l| l.contains(LockingFlags::LOCKED)),
        })
    }

    /// whether the SecureDevice was locked upon it's creation
    pub fn was_locked(&self) -> bool {
        self.was_locked
    }

    pub fn reconnect_controller(&mut self) -> crate::Result<(), P::Error> {
        self.device.reconnect_controller().context(super::IoSnafu)?;
        Ok(())
    }

    pub fn com_id(&self) -> u16 {
        self.com_id
    }

    pub fn is_eprise(&self) -> bool {
        self.is_eprise
    }

    pub fn proto(&mut self) -> &mut P {
        &mut self.device
    }

    pub fn recv_locked(&mut self) -> crate::Result<bool, P::Error> {
        Ok(recv_info(self.proto())?
            .locking
            .map_or(false, |locking| {
                locking.contains(LockingFlags::LOCKED)
            }))
    }
}

/// Level 0 discovery subset
fn recv_info<P: SecureProtocol>(proto: &mut P) -> crate::Result<SecureDeviceInfo, P::Error> {
    let mut device_info = SecureDeviceInfo {
        locking: None,
        opal_v2: None,
        enterprise: None,
    };

    let mut buffer = crate::util::alloc_aligned(1024, proto.align());

    // level 0 discovery
    unsafe { proto.secure_recv(1, 1, buffer.as_mut()) }.context(super::IoSnafu)?;

    // check the version for sanity
    if buffer[4..8] != [0, 0, 0, 1] {
        return Err(crate::Error::IncompatibleVersion);
    }

    // ignore the rest of the header
    let mut offset = 48;

    fn get_com_id(buffer: &[u8], offset: usize) -> ComIdInfo {
        ComIdInfo {
            base_com_id: (buffer[offset] as u16) << 8 | buffer[offset + 1] as u16,
            num_com_ids: (buffer[offset + 2] as u16) << 8 | buffer[offset + 3] as u16,
        }
    }

    while offset < buffer.len() - 1 {
        match FeatureCodes((buffer[offset] as u16) << 8 | buffer[offset + 1] as u16) {
            FeatureCodes::LOCKING => {
                device_info.locking = LockingFlags::from_bits(match buffer.get(offset + 4) {
                    Some(&bits) => bits,
                    None => break,
                })
            }
            FeatureCodes::ENTERPRISE => {
                device_info.enterprise = Some(get_com_id(&buffer, offset + 4));
            }
            FeatureCodes::OPAL_V2 => device_info.opal_v2 = Some(get_com_id(&buffer, offset + 4)),
            _ => {}
        }
        let len = match buffer.get(offset + 3) {
            Some(&len) => len as usize,
            None => break,
        };
        offset += len + 4;
    }

    Ok(device_info)
}
