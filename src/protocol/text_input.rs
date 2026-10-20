//! Explicit clipboard typing through standard RFB KeyEvent messages.
//!
//! Validate the complete text before it can be queued. This keeps unsupported
//! controls and oversized input from producing a partially typed prefix.
use anyhow::{ensure, Result};

pub const MAX_TYPED_TEXT_BYTES: usize = 64 * 1024;
const RETURN: u32 = 0xff0d;
const TAB: u32 = 0xff09;

/// A validated, bounded sequence of key taps. The private representation prevents
/// callers from bypassing the text/control limits after validation.
pub struct TypedText {
    keysyms: Vec<u32>,
}

impl TypedText {
    pub fn new(text: &str) -> Result<Self> {
        ensure!(!text.is_empty(), "Clipboard has no text to type");
        ensure!(
            text.len() <= MAX_TYPED_TEXT_BYTES,
            "Clipboard typing is limited to 64 KiB of UTF-8 text"
        );
        ensure!(
            !text
                .chars()
                .any(|ch| ch.is_control() && !matches!(ch, '\r' | '\n' | '\t')),
            "Clipboard contains unsupported control characters"
        );

        let mut keysyms = Vec::with_capacity(text.len());
        let mut chars = text.chars().peekable();
        while let Some(ch) = chars.next() {
            keysyms.push(match ch {
                '\r' => {
                    if chars.peek() == Some(&'\n') {
                        chars.next();
                    }
                    RETURN
                }
                '\n' => RETURN,
                '\t' => TAB,
                _ => char_to_keysym(ch),
            });
        }
        Ok(Self { keysyms })
    }

    /// Number of key taps, after normalizing line endings.
    pub fn len(&self) -> usize {
        self.keysyms.len()
    }

    pub fn keysyms(&self) -> impl Iterator<Item = u32> + '_ {
        self.keysyms.iter().copied()
    }

    /// At most 1 MiB: each of at most 64 KiB input characters yields two
    /// eight-byte RFB messages. One serialized write preserves tap ordering
    /// without flooding the command channel with per-character commands.
    pub(super) fn wire_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(self.len() * 16);
        for keysym in self.keysyms() {
            for down in [1, 0] {
                bytes.extend_from_slice(&[super::messages::client_msg::KEY_EVENT, down, 0, 0]);
                bytes.extend_from_slice(&keysym.to_be_bytes());
            }
        }
        bytes
    }
}

pub fn char_to_keysym(ch: char) -> u32 {
    let codepoint = ch as u32;
    if codepoint <= 0xff {
        codepoint
    } else {
        0x0100_0000 | codepoint
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_preserves_unicode_and_uses_return_and_tab_keysyms() {
        let text = TypedText::new("Aé中🙂\r\nB\tC\rD\n").unwrap();
        assert_eq!(
            text.keysyms().collect::<Vec<_>>(),
            [
                0x41,
                0xe9,
                0x0100_4e2d,
                0x0101_f642,
                0xff0d,
                0x42,
                0xff09,
                0x43,
                0xff0d,
                0x44,
                0xff0d,
            ]
        );
        assert_eq!(text.len(), 11);
    }

    #[test]
    fn text_rejects_empty_or_unsupported_controls_without_returning_a_prefix() {
        assert!(TypedText::new("").is_err());
        for control in [
            '\0', '\u{0001}', '\u{0008}', '\u{001b}', '\u{007f}', '\u{0085}',
        ] {
            let text = format!("valid prefix{control}suffix");
            assert!(TypedText::new(&text).is_err());
        }
    }

    #[test]
    fn text_limit_counts_utf8_bytes_and_bounds_serialized_output() {
        let ascii = "a".repeat(MAX_TYPED_TEXT_BYTES);
        let text = TypedText::new(&ascii).unwrap();
        assert_eq!(text.len(), MAX_TYPED_TEXT_BYTES);
        assert_eq!(text.wire_bytes().len(), 1024 * 1024);
        assert!(TypedText::new(&(ascii + "a")).is_err());

        let unicode = "🙂".repeat(MAX_TYPED_TEXT_BYTES / 4);
        assert_eq!(
            TypedText::new(&unicode).unwrap().len(),
            MAX_TYPED_TEXT_BYTES / 4
        );
        assert!(TypedText::new(&(unicode + "é")).is_err());
    }
}
