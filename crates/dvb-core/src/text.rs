//! Exact text forms for the small shared enums, and shared text clipping and cleaning.

// Capture name tokens directly so utoipa sees the same string literals as serde.
macro_rules! text_enum {
    ($(#[$meta:meta])* pub enum $name:ident { $( $(#[$variant_meta:meta])* $variant:ident => $text:tt, )* }) => {
        $(#[$meta])*
        pub enum $name {
            $( $(#[$variant_meta])* #[serde(rename = $text)] $variant, )*
        }

        impl $name {
            pub const fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $text,)* }
            }
        }

        impl std::str::FromStr for $name {
            type Err = String;
            fn from_str(value: &str) -> Result<Self, Self::Err> {
                match value {
                    $($text => Ok(Self::$variant),)*
                    _ => Err(format!("unknown {} {value}", stringify!($name).to_lowercase())),
                }
            }
        }
    };
}
pub(crate) use text_enum;

/// `s` cut to at most `max` bytes on a character boundary.
pub fn clip(s: &str, max: usize) -> &str {
    &s[..s.floor_char_boundary(max)]
}

/// `line` without terminal escape sequences or other control characters.
pub fn plain(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // CSI sequences end at their first letter.
            if chars.next() == Some('[') {
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        } else if c == '\t' {
            out.push(' ');
        } else if !c.is_control() {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_keeps_character_boundaries() {
        assert_eq!(clip("åäö", 3), "å");
        assert_eq!(clip("abc", 10), "abc");
    }

    #[test]
    fn plain_drops_colours() {
        assert_eq!(plain("\u{1b}[1;32mok\u{1b}[0m"), "ok");
    }
}
