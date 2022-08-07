use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;
use uefi::proto::console::text::{Key, ScanCode};
use uefi::table::{Boot, SystemTable};
use crate::{info, Result, ResultFixupExt};

/// options is a Vec<(selectable, String)>; returns the chosen index within the options-vec
pub fn choose(st: &mut SystemTable<Boot>, options: &Vec<(bool, String)>) -> Result<usize> {
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
