use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;
use core::time::Duration;
use uefi::proto::device_path::DevicePath;
use uefi::proto::media::block::BlockIO;
use uefi::Status;
use uefi::table::{Boot, SystemTable};
use uefi::table::runtime::ResetType;
use crate::{Error, info, NvmeDevice, NvmExpressPassthru, OpalError, OpalSession, SecureDevice, sleep, StatusCode, uid, Result, LockingState, Config, ResultFixupExt};

/// Returns Ok(Ok((())) if unlocking was successful, Ok(Err(())) if the password was wrong
pub fn try_unlock_device(st: &mut SystemTable<Boot>, config: &Config, device: &mut SecureDevice, password: String) -> Result<core::result::Result<(), ()>> {
    let mut hash = vec![0; 32];

    // as in sedutil-cli, maybe will change
    pbkdf2::pbkdf2::<hmac::Hmac<sha1::Sha1>>(
        password.as_bytes(),
        device.proto().serial_num(),
        75000,
        &mut hash,
    );

    {
        let session = pretty_session(st, device, &*hash, config.sed_locked_msg.as_deref())?;
        if let Some(mut s) = session {
            s.set_mbr_done(true)?;
            s.set_locking_range(0, LockingState::ReadWrite)?;
        }
    }

    // reconnect the controller to see
    // the real partition pop up after unlocking
    device.reconnect_controller(st).fix(info!())?;
    Ok(Ok(()))
}

fn pretty_session<'d>(
    st: &mut SystemTable<Boot>,
    device: &'d mut SecureDevice,
    challenge: &[u8],
    sed_locked_msg: Option<&str>,
) -> Result<Option<OpalSession<'d>>> {
    match OpalSession::start(
        device,
        uid::OPAL_LOCKINGSP,
        uid::OPAL_ADMIN1,
        Some(challenge),
    ) {
        Ok(session) => Ok(Some(session)),
        Err(Error::Opal(OpalError::Status(StatusCode::NOT_AUTHORIZED))) => Ok(None),
        Err(Error::Opal(OpalError::Status(StatusCode::AUTHORITY_LOCKED_OUT))) => {
            st.stdout()
                .write_str(
                    sed_locked_msg
                        .unwrap_or("Too many bad tries, SED locked out, resetting in 10s.."),
                )
                .unwrap();
            sleep(Duration::from_secs(10));
            st.runtime_services()
                .reset(ResetType::Cold, Status::WARN_RESET_REQUIRED, None);
        }
        e => e.map(Some),
    }
}

pub(crate) fn find_secure_devices(st: &mut SystemTable<Boot>) -> uefi::Result<Vec<SecureDevice>> {
    let mut result = Vec::new();

    for handle in st.boot_services().find_handles::<BlockIO>()? {
        let blockio = st.boot_services().handle_protocol::<BlockIO>(handle)?;

        if unsafe { &mut *blockio.get() }
            .media()
            .is_logical_partition()
        {
            continue;
        }

        let device_path = st
            .boot_services()
            .handle_protocol::<DevicePath>(handle)?;
        let device_path = unsafe { &mut &*device_path.get() };

        if let Ok(nvme) = st
            .boot_services()
            .locate_device_path::<NvmExpressPassthru>(device_path)
        {
            let nvme = st
                .boot_services()
                .handle_protocol::<NvmExpressPassthru>(nvme)?;

            result.push(SecureDevice::new(handle, NvmeDevice::new(nvme.get())?)?)
        }

        // todo something like that:
        //
        // if let Ok(ata) = st
        //     .boot_services()
        //     .locate_device_path::<AtaExpressPassthru>(device_path)
        //     .log_warning()
        // {
        //     let ata = st
        //         .boot_services()
        //         .handle_protocol::<AtaExpressPassthru>(ata)?
        //         .log();
        //
        //     result.push(SecureDevice::new(handle, AtaDevice::new(ata.get())?.log())?.log())
        // }
        //
        // ..etc
    }
    Ok(result.into())
}

