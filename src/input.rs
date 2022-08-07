use alloc::string::String;
use uefi::proto::console::text::{Key, ScanCode};
use uefi::{CStr16, Status};
use uefi::table::{Boot, SystemTable};
use uefi::table::runtime::ResetType;
use crate::{info, ResultFixupExt, Result};

pub fn password(st: &mut SystemTable<Boot>) -> Result<String> {
    read(st, Some('*'))
}
pub fn line(st: &mut SystemTable<Boot>) -> Result<String> {
    read(st, None)
}
pub fn key(st: &mut SystemTable<Boot>) -> Result<Key> {
    let mut wait_for_key = [unsafe { st.stdin().wait_for_key_event().unsafe_clone() }];

    loop {
        st.boot_services()
            .wait_for_event(&mut wait_for_key)
            .fix(info!())?;
        if let Some(key) = st.stdin().read_key().fix(info!())? {
            return Ok(key)
        }
    }
}
fn read(st: &mut SystemTable<Boot>, replacement_char: Option<char>) -> Result<String> {
    let mut data = String::with_capacity(32);
    loop {
        match key(st)? {
            // cr / lf
            Key::Printable(k) if [0xD, 0xA].contains(&u16::from(k)) => {
                write_char(st, 0x0D)?;
                write_char(st, 0x0A)?;
                break Ok(data);
            }
            // backspace
            Key::Printable(k) if u16::from(k) == 0x8 => {
                if data.pop().is_some() {
                    write_char(st, 0x08)?;
                }
            }
            Key::Printable(k) => {
                match replacement_char {
                    Some(c) => write_char(st, c as u16)?,
                    None => write_char(st, u16::from(k))?,
                }
                data.push(k.into());
            }
            Key::Special(ScanCode::ESCAPE) => {
                st.runtime_services()
                    .reset(ResetType::Shutdown, Status::SUCCESS, None)
            }
            _ => {}
        }
    }
}

fn write_char(st: &mut SystemTable<Boot>, ch: u16) -> Result {
    let str = &[ch, 0];
    st.stdout()
        .output_string(unsafe { CStr16::from_u16_with_nul_unchecked(str) })
        .fix(info!())
}

