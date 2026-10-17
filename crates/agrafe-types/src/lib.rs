//! What a file is: content type constants, classification into categories,
//! magic-byte sniffing, and the loader allowlist for the renderer. Shared
//! by the server, the renderer and the sandboxed helpers, so it stays pure
//! and small.

pub mod classify;
pub mod sniff;
pub mod text;

pub use classify::{Category, classify};

/// Error returned when a text column holds a value outside an enum.
#[derive(Debug, thiserror::Error)]
#[error("unknown {kind} `{value}`")]
pub struct ParseEnumError {
    pub kind: &'static str,
    pub value: String,
}

/// Implements `as_str`, `Display`, `FromStr` and `TryFrom<String>` for a
/// fieldless enum stored as text.
#[macro_export]
macro_rules! str_enum {
    ($name:ident { $($variant:ident => $s:literal),* $(,)? }) => {
        impl $name {
            pub const ALL: &'static [$name] = &[$($name::$variant),*];

            pub fn as_str(self) -> &'static str {
                match self { $($name::$variant => $s),* }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl std::str::FromStr for $name {
            type Err = $crate::ParseEnumError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                match s {
                    $($s => Ok($name::$variant),)*
                    _ => Err($crate::ParseEnumError { kind: stringify!($name), value: s.to_string() }),
                }
            }
        }

        impl TryFrom<String> for $name {
            type Error = $crate::ParseEnumError;

            fn try_from(s: String) -> Result<Self, Self::Error> {
                s.parse()
            }
        }
    };
}
