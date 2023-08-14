#![no_std]

extern crate core;
extern crate alloc;

use alloc::fmt::{Debug, Display};
use alloc::string::String;
use defs::uid;
use io::SecureDevice;
use session::OpalSession;
use snafu::{Snafu, Location, AsErrorSource, OptionExt, ensure};

mod defs;
mod util;
mod io;
mod command;
mod session;

pub use defs::{OpalError, StatusCode};
#[derive(Debug, Snafu)]
pub enum Error<E: Debug + Display + AsErrorSource> {
    Io { source: E, location: Location },
    Unsupported,
    IncompatibleVersion,
    Pbkdf,
    RawKeyInvalidLength,
    Opal { source: OpalError, msg: String },
}
type Result<O, E> = core::result::Result<O, Error<E>>;

pub use io::SecureProtocol;

pub struct OpalDrive<P> {
    dev: SecureDevice<P>,
}
impl<P: SecureProtocol> OpalDrive<P> {
    pub fn new(p: P) -> Result<Self, P::Error> {
        let dev = io::SecureDevice::new(p)?;
        Ok(Self { dev })
    }

    pub fn serial(&mut self) -> &[u8] {
        self.dev.proto().serial_num()
    }

    pub fn was_locked(&self) -> bool {
        self.dev.was_locked()
    }

    pub fn unlock(&mut self, pwd: PasswordOrRaw) -> Result<(), P::Error> {
        let mut hash = alloc::vec![0; 32];

        match pwd {
            PasswordOrRaw::Password(pwd) => {
                pbkdf2::pbkdf2::<hmac::Hmac<sha1::Sha1>>(
                    pwd,
                    self.dev.proto().serial_num(),
                    75000,
                    &mut hash,
                ).ok().context(PbkdfSnafu)?;
            }
            PasswordOrRaw::Raw(r) => {
                ensure!(r.len() == hash.len(), RawKeyInvalidLengthSnafu);
                hash.copy_from_slice(r);
            }
        }

        tracing::info!("{hash:x?}");

        let mut session = OpalSession::start(&mut self.dev, uid::OPAL_LOCKINGSP, uid::OPAL_ADMIN1, Some(&hash))?;
        session.set_locking_range(0, defs::LockingState::ReadWrite)?;
        session.set_mbr_done(true)?;
        Ok(())
    }
}

pub enum PasswordOrRaw<'a> {
    Password(&'a [u8]),
    /// Must be 32 bytes
    Raw(&'a [u8]),
}
