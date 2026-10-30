//! What a file is: content type constants, classification into categories,
//! magic-byte sniffing, the loader allowlist for the renderer, the list of
//! content types an installation accepts, plus the
//! contracts between the server and its helpers (the text sample, the
//! map spec). Shared by the server, the renderer and the sandboxed
//! helpers, so it stays pure and small.

pub mod allowlist;
pub mod classify;
pub mod geojson;
pub mod gml;
pub mod gpx;
pub mod kml;
pub mod map;
pub mod office;
pub mod shapefile;
pub mod sniff;
pub mod text;
mod xml;
mod zip;

pub use classify::{Category, classify};

/// Error returned when a text column holds a value outside an enum.
#[derive(Debug, thiserror::Error)]
#[error("unknown {kind} `{value}`")]
pub struct ParseEnumError {
    /// The enum's name, as written in the code.
    pub kind: &'static str,
    /// The text that matched none of its variants.
    pub value: String,
}

/// Implements `as_str`, `Display`, `FromStr` and `TryFrom<String>` for a
/// fieldless enum stored as text.
#[macro_export]
macro_rules! str_enum {
    ($name:ident { $($variant:ident => $s:literal),* $(,)? }) => {
        impl $name {
            /// Every variant, in declaration order.
            pub const ALL: &'static [$name] = &[$($name::$variant),*];

            /// The text the variant is stored and displayed as.
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
