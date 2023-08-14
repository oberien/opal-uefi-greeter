use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;
use core::time::Duration;
use uefi::proto::console::text::{Key, ScanCode};
use uefi::table::{Boot, SystemTable};
use uefi::{CStr16, Status};
use uefi::table::runtime::ResetType;
use crate::{Result, Context, util};

/// options is a Vec<(selectable, String)>; returns the chosen index within the options-vec
pub fn choose(st: &SystemTable<Boot>, options: &Vec<(bool, String)>) -> Result<usize> {
    consume_old_keypresses(st)?;

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
        false => next_selectable(0, options, false),
    };
    // initialize menu output; only update the `>` later on
    let output: String = options.iter()
        .flat_map(|(_, option)| ["  ", option, "\r\n"])
        .collect();

    let mut st = unsafe { st.unsafe_clone() };
    st.stdout().write_str(&output).unwrap();

    let next_row = st.stdout().cursor_position().1;

    loop {
        // reset old `>`
        let row = st.stdout().cursor_position().1;
        st.stdout().set_cursor_position(0, row)
            .context("can't set cursor position to overwrite old `>`")?;
        st.stdout().write_char(' ').unwrap();
        // write `>`
        st.stdout().set_cursor_position(0, next_row - options.len() + chosen)
            .context("can't set cursor position to write `>`")?;
        st.stdout().write_char('>').unwrap();

        loop {
            match key(&st)? {
                Key::Special(ScanCode::DOWN) => {
                    chosen = next_selectable(chosen, options, false);
                    break;
                },
                Key::Special(ScanCode::UP) => {
                    chosen = next_selectable(chosen, options, true);
                    break;
                },
                // enter
                Key::Printable(k) if [0xD, 0xA].contains(&u16::from(k)) => {
                    // reset cursor position
                    st.stdout().set_cursor_position(0, next_row).context("can't reset cursor position")?;
                    return Ok(chosen)
                },
                _ => (),
            }
        }
    }
}

pub fn password(st: &SystemTable<Boot>) -> Result<String> {
    consume_old_keypresses(st)?;
    read(st, Some('*'))
}
pub fn line(st: &SystemTable<Boot>) -> Result<String> {
    consume_old_keypresses(st)?;
    read(st, None)
}
fn consume_old_keypresses(st: &SystemTable<Boot>) -> Result<()> {
    let mut st = unsafe { st.unsafe_clone() };
    // yield to let UEFI queue all stale key events
    util::sleep(Duration::from_millis(10));
    loop {
        match st.stdin().read_key().context("can't read key to consume old stale events")? {
            Some(key) => log::trace!("consumed stale key: {key:?}"),
            None => break,
        }
    }

    Ok(())
}
pub fn key(st: &SystemTable<Boot>) -> Result<Key> {
    let mut st = unsafe { st.unsafe_clone() };
    let mut wait_for_key = [unsafe { st.stdin().wait_for_key_event().unsafe_clone() }];

    loop {
        st.boot_services()
            .wait_for_event(&mut wait_for_key)
            .context("error waiting for key event")?;
        if let Some(key) = st.stdin().read_key().context("error reading key")? {
            return Ok(key)
        }
    }
}
fn read(st: &SystemTable<Boot>, replacement_char: Option<char>) -> Result<String> {
    let mut data = String::with_capacity(32);
    loop {
        match key(st)? {
            // cr / lf
            Key::Printable(k) if [0xD, 0xA].contains(&u16::from(k)) && !data.is_empty() => {
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
                    .reset(ResetType::SHUTDOWN, Status::SUCCESS, None)
            }
            _ => {}
        }
    }
}

fn write_char(st: &SystemTable<Boot>, ch: u16) -> Result {
    let mut st = unsafe { st.unsafe_clone() };
    let str = &[ch, 0];
    st.stdout()
        .output_string(unsafe { CStr16::from_u16_with_nul_unchecked(str) })
        .context("")
}

