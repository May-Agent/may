//! The script language the host `sed` is allowed to run: `p` commands
//! addressed by `N`, `N,M`, `N,$` or `$`.

use crate::Unsupported;

/// Checks that `text` is commands joined by `;` or newlines, each an
/// address, `p` and GNU `sed`'s blanks between them.
pub fn validate(text: &str) -> Result<(), Unsupported> {
    let mut cursor = Cursor(text.as_bytes());
    let mut commands = 0_usize;
    loop {
        while cursor.skip(|byte| byte == b';' || byte == b'\n' || is_blank(byte)) {}
        if cursor.0.is_empty() {
            break;
        }
        cursor.command()?;
        commands += 1;
    }
    if commands == 0 {
        return Err(Unsupported);
    }
    Ok(())
}

/// The blanks GNU `sed` skips inside a command: everything `isspace` accepts
/// but the newline.
const fn is_blank(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | 0x0b | 0x0c | b'\r')
}

struct Cursor<'a>(&'a [u8]);

impl Cursor<'_> {
    /// Consumes one byte satisfying `accept`.
    fn skip(&mut self, accept: impl Fn(u8) -> bool) -> bool {
        match self.0.split_first() {
            Some((&byte, rest)) if accept(byte) => {
                self.0 = rest;
                true
            }
            _ => false,
        }
    }

    fn blanks(&mut self) {
        while self.skip(is_blank) {}
    }

    /// A line number; one larger than `u64` saturates.
    fn number(&mut self) -> Result<u64, Unsupported> {
        let mut value = 0_u64;
        let mut digits = 0;
        while let Some((&byte, rest)) = self.0.split_first() {
            if !byte.is_ascii_digit() {
                break;
            }
            value = value
                .saturating_mul(10)
                .saturating_add(u64::from(byte - b'0'));
            digits += 1;
            self.0 = rest;
        }
        if digits == 0 {
            return Err(Unsupported);
        }
        Ok(value)
    }

    /// One command: an address, then `p`, then the end of the command.
    fn command(&mut self) -> Result<(), Unsupported> {
        if !self.skip(|byte| byte == b'$') {
            // GNU `sed` has no line 0 to start at; an end of 0 is merely
            // before the start.
            if self.number()? == 0 {
                return Err(Unsupported);
            }
            self.blanks();
            if self.skip(|byte| byte == b',') {
                self.blanks();
                if !self.skip(|byte| byte == b'$') {
                    self.number()?;
                }
            }
        }
        self.blanks();
        if !self.skip(|byte| byte == b'p') {
            return Err(Unsupported);
        }
        self.blanks();
        match self.0.first() {
            None | Some(b';' | b'\n') => Ok(()),
            Some(_) => Err(Unsupported),
        }
    }
}
