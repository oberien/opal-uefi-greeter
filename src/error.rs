use alloc::string::String;
use core::fmt::{Debug, Display, Formatter};
use core::panic::Location;
use crate::io::ErrorWrapper;

pub type Result<T = ()> = core::result::Result<T, Error>;

// pub struct UefiAcidioWrapper(pub uefi::Error, pub &'static str);
// impl Debug for UefiAcidioWrapper {
//     fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
//         Debug::fmt(&self.0, f)
//     }
// }
// impl Display for UefiAcidioWrapper {
//     fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
//         // uefi::Error doesn't have a Display impl
//         Debug::fmt(&self.0, f)
//     }
// }
impl acid_io::ErrorTrait for Error {}

#[derive(Debug)]
pub struct Error {
    pub source: Option<ErrorSource>,
    pub location: &'static Location<'static>,
    pub context: String,
}

impl Error {
    #[track_caller]
    pub fn new(source: impl Into<ErrorSource>, context: impl Into<String>) -> Error {
        Error {
            source: Some(source.into()),
            location: Location::caller(),
            context: context.into(),
        }
    }
    #[track_caller]
    pub fn new_from_uefi<E: Debug>(source: uefi::Error<E>, context: impl Into<String>) -> Error {
        Error {
            source: Some(ErrorSource::Uefi(uefi::Error::from(source.status()))),
            location: Location::caller(),
            context: context.into(),
        }
    }
    #[track_caller]
    pub fn new_without_source(msg: impl Into<String>) -> Error {
        Error {
            source: None,
            location: Location::caller(),
            context: msg.into(),
        }
    }
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.context)?;
        if let Some(source) = &self.source {
            f.write_str(", reason: ")?;
            Display::fmt(source, f)?;
        }
        write!(f, ", at {}", self.location)?;
        Ok(())
    }
}

#[derive(Debug, thiserror_no_std::Error)]
pub enum ErrorSource {
    #[error("file not found")]
    FileNotFound,
    #[error("opal: {0:?}")]
    Opal(#[from] crate::low_level::opal::OpalError),
    #[error("uefi: {0:?}")]
    Uefi(uefi::Error),
    #[error("utf8: {0}")]
    Utf8(#[from] core::str::Utf8Error),
    #[error("toml: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("cstring16fromstr: {0:?}")]
    CString16FromStrError(#[from] uefi::data_types::FromStrError),
    #[error("io: {0}")]
    Io(#[from] acid_io::Error),
    #[error("luks: {0}")]
    Luks(#[from] luks2::error::LuksError),
    #[error("fat: {0}")]
    Fat(#[from] fatfs::Error<ErrorWrapper>),
}

pub trait Context {
    type Ok;
    #[track_caller]
    fn context(self, context: impl Into<String>) -> core::result::Result<Self::Ok, Error>;
}

impl<T, E: Into<ErrorSource>> Context for core::result::Result<T, E> {
    type Ok = T;
    #[track_caller]
    fn context(self, context: impl Into<String>) -> core::result::Result<T, Error> {
        self.map_err(|err| Error::new(err, context))
    }
}
impl<T, E: Debug> Context for core::result::Result<T, uefi::Error<E>> {
    type Ok = T;
    #[track_caller]
    fn context(self, context: impl Into<String>) -> core::result::Result<T, Error> {
        self.map_err(|err| Error::new_from_uefi(err, context))
    }
}
