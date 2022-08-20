use crate::low_level::opal::StatusCode;
use alloc::string::String;
use core::fmt::{Debug, Display, Formatter};
use core::str::Utf8Error;
use luks2::error::LuksError;
use uefi::Status;
use crate::ErrorWrapper;

pub type Result<T = ()> = core::result::Result<T, Error>;

#[derive(Debug, Copy, Clone)]
pub enum OpalError {
    Status(StatusCode),
    NoMethodStatus,
}

pub struct UefiAcidioWrapper(pub uefi::Error, pub &'static str);
impl Debug for UefiAcidioWrapper {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        Debug::fmt(&self.0, f)
    }
}
impl Display for UefiAcidioWrapper {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        // uefi::Error doesn't have a Display impl
        Debug::fmt(&self.0, f)
    }
}
impl acid_io::ErrorTrait for UefiAcidioWrapper {}

#[derive(Debug, thiserror_no_std::Error)]
pub enum Error {
    Uefi(Status, &'static str),
    Opal(#[from] OpalError),
    IoError(#[from] acid_io::Error),
    ConfigMissing,
    InvalidConfig(#[from] toml::de::Error),
    EfiImageNameNonUtf16,
    InitrdNameNonUtf16,
    FileNameNonUtf16,
    FileNotFound,
    ImageNotFound(String),
    ImageNotPeCoff,
    Luks(#[from] LuksError),
    Utf8(#[from] Utf8Error),
    Fatfs(#[from] fatfs::Error<ErrorWrapper>),
}

impl From<StatusCode> for Error {
    fn from(status: StatusCode) -> Self {
        OpalError::Status(status).into()
    }
}

pub trait ResultFixupExt<T>: Sized {
    fn fix(self, name: &'static str) -> Result<T>;
}

#[macro_export]
macro_rules! info {
    () => {
        concat!(file!(), ":", line!())
    };
}

impl<T, D: Debug> ResultFixupExt<T> for uefi::Result<T, D> {
    fn fix(self, info: &'static str) -> Result<T> {
        self
            .map_err(|e| Error::Uefi(e.status(), info))
    }
}
