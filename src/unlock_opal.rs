use core::fmt::Write;
use core::time::Duration;
use uefi::Status;
use uefi::table::{Boot, SystemTable};
use uefi::table::runtime::ResetType;
use crate::{Error, info, OpalError, OpalSession, SecureDevice, sleep, StatusCode, uid, Result, LockingState, ResultFixupExt};

/// Returns Ok(Ok((())) if unlocking was successful, Ok(Err(())) if the password was wrong
pub fn try_unlock_device(st: &mut SystemTable<Boot>, device: &mut SecureDevice, password: &[u8]) -> Result<core::result::Result<(), ()>> {
    let mut hash = vec![0; 32];

    // as in sedutil-cli, maybe will change
    pbkdf2::pbkdf2::<hmac::Hmac<sha1::Sha1>>(
        password,
        device.proto().serial_num(),
        75000,
        &mut hash,
    );

    {
        let session = pretty_session(st, device, &*hash)?;
        if let Some(mut s) = session {
            s.set_mbr_done(true)?;
            s.set_locking_range(0, LockingState::ReadWrite)?;
        } else {
            return Ok(Err(()))
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
                .write_str("Too many bad tries, SED locked out, resetting in 10s..")
                .unwrap();
            sleep(Duration::from_secs(10));
            st.runtime_services()
                .reset(ResetType::Cold, Status::WARN_RESET_REQUIRED, None);
        }
        e => e.map(Some),
    }
}
