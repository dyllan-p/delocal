//! Identifiers (DESIGN.md §4): node, folder and batch IDs.
//!
//! All three are 16 random bytes. The host generates them and hands them to
//! the engine in configuration or events; the engine never draws randomness
//! (§7). They are ordered by byte order, which is the derived `Ord` here:
//! wherever DESIGN.md says one ID is "larger" than another, it means this.
//! Displayed as 32 lowercase hex characters; the short form is the first 8.

use std::fmt;
use std::str::FromStr;

use serde::de::{self, Deserialize, Deserializer, Visitor};
use serde::ser::{Serialize, Serializer};

/// Number of bytes in every identifier.
pub const ID_LEN: usize = 16;

/// Error from parsing an identifier's hex form.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParseIdError {
    /// The input was not exactly 32 characters long.
    Length(usize),
    /// A character at this byte offset was not a hex digit.
    NotHex(usize),
}

impl fmt::Display for ParseIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Length(n) => write!(f, "expected 32 hex characters, got {n}"),
            Self::NotHex(at) => write!(f, "not a hex digit at offset {at}"),
        }
    }
}

impl std::error::Error for ParseIdError {}

fn hex_digit(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

fn parse_hex(s: &str) -> Result<[u8; ID_LEN], ParseIdError> {
    let bytes = s.as_bytes();
    if bytes.len() != ID_LEN * 2 {
        return Err(ParseIdError::Length(bytes.len()));
    }
    let mut out = [0u8; ID_LEN];
    for (i, slot) in out.iter_mut().enumerate() {
        let hi = hex_digit(bytes[2 * i]).ok_or(ParseIdError::NotHex(2 * i))?;
        let lo = hex_digit(bytes[2 * i + 1]).ok_or(ParseIdError::NotHex(2 * i + 1))?;
        *slot = (hi << 4) | lo;
    }
    Ok(out)
}

fn write_hex(f: &mut fmt::Formatter<'_>, bytes: &[u8]) -> fmt::Result {
    for b in bytes {
        write!(f, "{b:02x}")?;
    }
    Ok(())
}

/// The first 8 hex characters of an identifier, for display.
///
/// Produced by [`NodeId::short`] and the equivalents on the other IDs.
#[derive(Clone, Copy)]
pub struct Short<'a>(&'a [u8; ID_LEN]);

impl fmt::Display for Short<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_hex(f, &self.0[..4])
    }
}

impl fmt::Debug for Short<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

macro_rules! id_type {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name([u8; ID_LEN]);

        impl $name {
            /// Wrap 16 bytes the host generated.
            pub const fn from_bytes(bytes: [u8; ID_LEN]) -> Self {
                Self(bytes)
            }

            /// The raw bytes.
            pub const fn as_bytes(&self) -> &[u8; ID_LEN] {
                &self.0
            }

            /// The short display form: the first 8 hex characters.
            pub const fn short(&self) -> Short<'_> {
                Short(&self.0)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write_hex(f, &self.0)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!(stringify!($name), "({})"), self)
            }
        }

        impl FromStr for $name {
            type Err = ParseIdError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                parse_hex(s).map(Self)
            }
        }

        // Human-readable formats (JSON: node.json, --json output) get the hex
        // string; compact formats (postcard on the wire) get the raw bytes.
        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                if serializer.is_human_readable() {
                    serializer.collect_str(self)
                } else {
                    serializer.serialize_bytes(&self.0)
                }
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                struct IdVisitor;

                impl<'de> Visitor<'de> for IdVisitor {
                    type Value = $name;

                    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                        f.write_str(concat!(
                            "a ",
                            stringify!($name),
                            " as 32 hex characters or 16 bytes"
                        ))
                    }

                    fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                        v.parse().map_err(E::custom)
                    }

                    fn visit_bytes<E: de::Error>(self, v: &[u8]) -> Result<Self::Value, E> {
                        <[u8; ID_LEN]>::try_from(v)
                            .map($name)
                            .map_err(|_| E::invalid_length(v.len(), &self))
                    }
                }

                if deserializer.is_human_readable() {
                    deserializer.deserialize_str(IdVisitor)
                } else {
                    deserializer.deserialize_bytes(IdVisitor)
                }
            }
        }
    };
}

id_type! {
    /// Identifies one installation of delocal (§4, §5). Generated on first
    /// `up`, stored in the state dir, never changes. Keys version vectors.
    NodeId
}

id_type! {
    /// Identifies a synced folder across machines (§4, §9).
    FolderId
}

id_type! {
    /// Identifies one batch of announced changes (§7.4).
    BatchId
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> [u8; ID_LEN] {
        [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
        ]
    }

    #[test]
    fn displays_as_32_lowercase_hex() {
        let id = NodeId::from_bytes(sample());
        assert_eq!(id.to_string(), "00112233445566778899aabbccddeeff");
        assert_eq!(
            format!("{id:?}"),
            "NodeId(00112233445566778899aabbccddeeff)"
        );
    }

    #[test]
    fn short_form_is_first_8_hex() {
        let id = FolderId::from_bytes(sample());
        assert_eq!(id.short().to_string(), "00112233");
    }

    #[test]
    fn parses_upper_and_lower_hex() {
        let lower: BatchId = "00112233445566778899aabbccddeeff".parse().unwrap();
        let upper: BatchId = "00112233445566778899AABBCCDDEEFF".parse().unwrap();
        assert_eq!(lower, upper);
        assert_eq!(*lower.as_bytes(), sample());
    }

    #[test]
    fn rejects_bad_length_and_bad_digits() {
        assert_eq!("0011".parse::<NodeId>(), Err(ParseIdError::Length(4)));
        assert_eq!(
            "0011223344556677889 aabbccddeeff".parse::<NodeId>(),
            Err(ParseIdError::NotHex(19))
        );
        // Multi-byte characters must not panic; the length is in bytes.
        assert!("é".repeat(16).parse::<NodeId>().is_err());
    }

    #[test]
    fn ordering_is_byte_order() {
        let mut lo = [0u8; ID_LEN];
        let mut hi = [0u8; ID_LEN];
        lo[0] = 1;
        lo[15] = 0xff;
        hi[0] = 2;
        assert!(NodeId::from_bytes(lo) < NodeId::from_bytes(hi));
        assert!(NodeId::from_bytes([0; ID_LEN]) < NodeId::from_bytes(lo));
        assert_eq!(NodeId::from_bytes(hi), NodeId::from_bytes(hi));
    }

    #[test]
    fn ids_of_different_kinds_are_different_types() {
        // Compile-time check: a NodeId is not a FolderId even with equal bytes.
        fn takes_node(_: NodeId) {}
        takes_node(NodeId::from_bytes(sample()));
        let _folder = FolderId::from_bytes(sample());
    }
}
