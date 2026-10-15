//! The input checks libyaml's reader made before parsing: the bytes must be
//! UTF-8, and only printable characters may appear.
//!
//! Follows libyaml's reader (`reader.c`, Copyright 2006-2016 Kirill Simonov,
//! 2017-2020 Ingy döt Net, MIT) as translated by unsafe-libyaml (David
//! Tolnay, MIT), modified; see `THIRD-PARTY-NOTICES`.

/// A reader error: libyaml's problem text and the byte offset it occurred at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ReaderError {
    pub(crate) problem: &'static str,
    pub(crate) offset: usize,
}

/// Decodes `input` as UTF-8 text of printable characters, reporting the
/// first problem as libyaml did.
pub(crate) fn decode(input: &[u8]) -> Result<&str, ReaderError> {
    let err = |problem, offset| ReaderError { problem, offset };
    match std::str::from_utf8(input) {
        Ok(text) => check_printable(text).map(|()| text),
        Err(e) => {
            let start = e.valid_up_to();
            let valid = input.get(..start).unwrap_or_default();
            check_printable(std::str::from_utf8(valid).unwrap_or_default())?;
            let rest = input.get(start..).unwrap_or_default();
            let Some(&lead) = rest.first() else {
                return Err(err("incomplete UTF-8 octet sequence", start));
            };
            let width = match lead {
                0x00..=0x7F => 1,
                _ if lead & 0xE0 == 0xC0 => 2,
                _ if lead & 0xF0 == 0xE0 => 3,
                _ if lead & 0xF8 == 0xF0 => 4,
                _ => return Err(err("invalid leading UTF-8 octet", start)),
            };
            if rest.len() < width {
                return Err(err("incomplete UTF-8 octet sequence", start));
            }
            let mut value = u32::from(match width {
                2 => lead & 0x1F,
                3 => lead & 0x0F,
                _ => lead & 0x07,
            });
            for k in 1..width {
                let octet = rest.get(k).copied().unwrap_or_default();
                if octet & 0xC0 != 0x80 {
                    return Err(err("invalid trailing UTF-8 octet", start.saturating_add(k)));
                }
                value = (value << 6) | u32::from(octet & 0x3F);
            }
            let overlong = match width {
                2 => value < 0x80,
                3 => value < 0x800,
                _ => value < 0x10000,
            };
            if overlong {
                return Err(err("invalid length of a UTF-8 sequence", start));
            }
            Err(err("invalid Unicode character", start))
        }
    }
}

/// Rejects the control characters libyaml refused anywhere in a document.
pub(crate) fn check_printable(text: &str) -> Result<(), ReaderError> {
    match text.char_indices().find(|&(_, c)| {
        !matches!(
            u32::from(c),
            0x09 | 0x0A
                | 0x0D
                | 0x20..=0x7E
                | 0x85
                | 0xA0..=0xD7FF
                | 0xE000..=0xFFFD
                | 0x10000..=0x10FFFF
        )
    }) {
        Some((offset, _)) => Err(ReaderError {
            problem: "control characters are not allowed",
            offset,
        }),
        None => Ok(()),
    }
}
