use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;
use uefi::proto::console::text::{Key, ScanCode};
use uefi::table::{Boot, SystemTable};
use uefi::{CStr16, Status};
use uefi::table::runtime::ResetType;
use crate::{info, Result, ResultFixupExt};

/// options is a Vec<(selectable, String)>; returns the chosen index within the options-vec
pub fn choose(st: &mut SystemTable<Boot>, options: &Vec<(bool, String)>) -> Result<usize> {
    fn next_selectable<T>(current: usize, options: &[(bool, T)], rev: bool) -> usize {
        let dir = match rev {
            false => 1i32,
            true => -1i32,
        };
        let mut index = current as i32;
        loop {
            index += dir;
            if index == options.len() as i32 {
                index = 0;
            } else if index == -1 {
                index = (options.len() - 1) as i32;
            }
            if options[index as usize].0 {
                return index as usize
            }
        }
    }

    assert!(!options.is_empty());
    let mut chosen = match options[0].0 {
        true => 0,
        false => next_selectable(0, &options, false),
    };
    // initialize menu output; only update the `>` later on
    let output: String = options.iter()
        .flat_map(|(_, option)| ["  ", option, "\r\n"])
        .collect();
    st.stdout().write_str(&output).unwrap();

    let next_row = st.stdout().cursor_position().1;

    loop {
        // reset old `>`
        let row = st.stdout().cursor_position().1;
        st.stdout().set_cursor_position(0, row).fix(info!())?;
        st.stdout().write_char(' ').unwrap();
        // write `>`
        st.stdout().set_cursor_position(0, next_row - options.len() + chosen).fix(info!())?;
        st.stdout().write_char('>').unwrap();

        loop {
            match crate::input::key(st)? {
                Key::Special(ScanCode::DOWN) => {
                    chosen = next_selectable(chosen, &options, false);
                    break;
                },
                Key::Special(ScanCode::UP) => {
                    chosen = next_selectable(chosen, &options, true);
                    break;
                },
                // enter
                Key::Printable(k) if [0xD, 0xA].contains(&u16::from(k)) => {
                    // reset cursor position
                    st.stdout().set_cursor_position(0, next_row).fix(info!())?;
                    return Ok(chosen)
                },
                _ => (),
            }
        }
    }
}

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

