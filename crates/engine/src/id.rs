//! Identifiers (DESIGN.md §4): node, folder and batch IDs, and the hostname
//! carried as `author_host` (§7.1).
//!
//! The three IDs are 16 random bytes. The host generates them and hands them
//! to the engine in configuration or events; the engine never draws
//! randomness (§7). They are ordered by byte order, which is the derived
//! `Ord` here: wherever DESIGN.md says one ID is "larger" than another, it
//! means this. Displayed as 32 lowercase hex characters; the short form is
//! the first 8.
//!
//! The byte-newtype machinery is shared with [`crate::entry::ContentHash`] through
//! the `bytes_newtype!` macro.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Number of bytes in every identifier.
pub const ID_LEN: usize = 16;

/// Error from parsing the hex form of an identifier or hash.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParseHexError {
    /// The input was not the expected number of characters long.
    Length { expected: usize, got: usize },
    /// A character at this byte offset was not a hex digit.
    NotHex(usize),
}

impl fmt::Display for ParseHexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Length { expected, got } => {
                write!(f, "expected {expected} hex characters, got {got}")
            }
            Self::NotHex(at) => write!(f, "not a hex digit at offset {at}"),
        }
    }
}

impl std::error::Error for ParseHexError {}

fn hex_digit(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Parse exactly `2 * N` hex characters. Works on bytes, so a non-ASCII
/// input cannot panic on a char boundary.
pub(crate) fn parse_hex<const N: usize>(s: &str) -> Result<[u8; N], ParseHexError> {
    let bytes = s.as_bytes();
    if bytes.len() != N * 2 {
        return Err(ParseHexError::Length {
            expected: N * 2,
            got: bytes.len(),
        });
    }
    let mut out = [0u8; N];
    for (i, slot) in out.iter_mut().enumerate() {
        let hi = hex_digit(bytes[2 * i]).ok_or(ParseHexError::NotHex(2 * i))?;
        let lo = hex_digit(bytes[2 * i + 1]).ok_or(ParseHexError::NotHex(2 * i + 1))?;
        *slot = (hi << 4) | lo;
    }
    Ok(out)
}

pub(crate) fn write_hex(f: &mut fmt::Formatter<'_>, bytes: &[u8]) -> fmt::Result {
    for b in bytes {
        write!(f, "{b:02x}")?;
    }
    Ok(())
}

/// The first 8 hex characters of an identifier or hash, for display.
///
/// Produced by [`NodeId::short`] and the equivalents on the other byte
/// newtypes.
#[derive(Clone, Copy)]
pub struct Short<'a>(pub(crate) &'a [u8]);

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

/// A fixed-length byte newtype: byte-order `Ord`, hex `Display`, `FromStr`,
/// and a serde impl that emits hex for human-readable formats (JSON) and
/// raw bytes for compact ones (postcard on the wire).
macro_rules! bytes_newtype {
    ($(#[$doc:meta])* $name:ident, $len:expr) => {
        $(#[$doc])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name([u8; $len]);

        impl $name {
            /// Number of bytes.
            pub const LEN: usize = $len;

            /// Wrap bytes the host produced.
            pub const fn from_bytes(bytes: [u8; $len]) -> Self {
                Self(bytes)
            }

            /// The raw bytes.
            pub const fn as_bytes(&self) -> &[u8; $len] {
                &self.0
            }

            /// The short display form: the first 8 hex characters.
            pub const fn short(&self) -> $crate::id::Short<'_> {
                $crate::id::Short(&self.0)
            }
        }

        impl ::std::fmt::Display for $name {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                $crate::id::write_hex(f, &self.0)
            }
        }

        impl ::std::fmt::Debug for $name {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                write!(f, concat!(stringify!($name), "({})"), self)
            }
        }

        impl ::std::str::FromStr for $name {
            type Err = $crate::id::ParseHexError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                $crate::id::parse_hex::<$len>(s).map(Self)
            }
        }

        impl ::serde::Serialize for $name {
            fn serialize<S: ::serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                if serializer.is_human_readable() {
                    serializer.collect_str(self)
                } else {
                    serializer.serialize_bytes(&self.0)
                }
            }
        }

        impl<'de> ::serde::Deserialize<'de> for $name {
            fn deserialize<D: ::serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                struct BytesVisitor;

                impl<'de> ::serde::de::Visitor<'de> for BytesVisitor {
                    type Value = $name;

                    fn expecting(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                        write!(
                            f,
                            concat!("a ", stringify!($name), " as {} hex characters or {} bytes"),
                            $len * 2,
                            $len
                        )
                    }

                    fn visit_str<E: ::serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                        v.parse().map_err(E::custom)
                    }

                    fn visit_bytes<E: ::serde::de::Error>(self, v: &[u8]) -> Result<Self::Value, E> {
                        <[u8; $len]>::try_from(v)
                            .map($name)
                            .map_err(|_| E::invalid_length(v.len(), &self))
                    }
                }

                if deserializer.is_human_readable() {
                    deserializer.deserialize_str(BytesVisitor)
                } else {
                    deserializer.deserialize_bytes(BytesVisitor)
                }
            }
        }
    };
}

pub(crate) use bytes_newtype;

bytes_newtype! {
    /// Identifies one installation of delocal (§4, §5). Generated on first
    /// `up`, stored in the state dir, never changes. Keys version vectors.
    NodeId, ID_LEN
}

bytes_newtype! {
    /// Identifies a synced folder across machines (§4, §9).
    FolderId, ID_LEN
}

bytes_newtype! {
    /// Identifies one batch of announced changes (§7.4).
    BatchId, ID_LEN
}

/// Why a string is not a valid [`HostName`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostNameError {
    /// Longer than 63 bytes.
    TooLong(usize),
    /// A character outside `[a-z0-9-]` at this byte offset.
    BadChar(usize),
}

impl fmt::Display for HostNameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLong(n) => write!(f, "hostname is {n} bytes, at most 63 allowed"),
            Self::BadChar(at) => write!(
                f,
                "hostname has a character outside [a-z0-9-] at offset {at}"
            ),
        }
    }
}

impl std::error::Error for HostNameError {}

/// A Tailscale hostname as carried in `author_host` (§7.1): lowercase,
/// `[a-z0-9-]` only, at most 63 bytes. May be empty, because §7.6 requires
/// the conflict-name format to be total even when it should never happen.
///
/// The host lowercases and validates what Tailscale reports; the engine only
/// checks. Used for conflict-copy names and display, never for comparison.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct HostName(String);

impl HostName {
    /// Longest allowed hostname, a DNS label.
    pub const MAX_LEN: usize = 63;

    /// Validate and wrap.
    pub fn new(s: impl Into<String>) -> Result<Self, HostNameError> {
        let s = s.into();
        if s.len() > Self::MAX_LEN {
            return Err(HostNameError::TooLong(s.len()));
        }
        if let Some(at) = s
            .bytes()
            .position(|b| !(b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'))
        {
            return Err(HostNameError::BadChar(at));
        }
        Ok(Self(s))
    }

    /// The empty hostname, the fallback §7.6 talks about.
    pub const fn empty() -> Self {
        Self(String::new())
    }

    /// The hostname as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// True for the empty hostname.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl TryFrom<String> for HostName {
    type Error = HostNameError;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::new(s)
    }
}

impl From<HostName> for String {
    fn from(h: HostName) -> Self {
        h.0
    }
}

impl FromStr for HostName {
    type Err = HostNameError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

impl fmt::Display for HostName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for HostName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "HostName({:?})", self.0)
    }
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
        assert_eq!(
            "0011".parse::<NodeId>(),
            Err(ParseHexError::Length {
                expected: 32,
                got: 4
            })
        );
        assert_eq!(
            "0011223344556677889 aabbccddeeff".parse::<NodeId>(),
            Err(ParseHexError::NotHex(19))
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
    fn json_form_is_the_hex_string() {
        let id = NodeId::from_bytes(sample());
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"00112233445566778899aabbccddeeff\"");
        let back: NodeId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, id);
    }

    #[test]
    fn postcard_form_is_the_raw_bytes() {
        let id = NodeId::from_bytes(sample());
        let bytes = postcard::to_stdvec(&id).unwrap();
        // A varint length prefix (one byte for 16) plus the 16 bytes; the hex
        // string would be 33.
        assert!(
            bytes.len() <= 17,
            "postcard NodeId is {} bytes",
            bytes.len()
        );
        assert_eq!(&bytes[bytes.len() - ID_LEN..], &sample());
        let back: NodeId = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(back, id);
    }

    #[test]
    fn json_rejects_wrong_length_and_bad_hex() {
        assert!(serde_json::from_str::<NodeId>("\"0011\"").is_err());
        assert!(serde_json::from_str::<NodeId>("\"zz112233445566778899aabbccddeeff\"").is_err());
        assert!(serde_json::from_str::<NodeId>("[1, 2, 3]").is_err());
    }

    #[test]
    fn ids_of_different_kinds_are_different_types() {
        // Compile-time check: a NodeId is not a FolderId even with equal bytes.
        fn takes_node(_: NodeId) {}
        takes_node(NodeId::from_bytes(sample()));
        let _folder = FolderId::from_bytes(sample());
    }

    #[test]
    fn hostname_accepts_dns_labels_and_empty() {
        assert_eq!(HostName::new("laptop").unwrap().as_str(), "laptop");
        assert_eq!(HostName::new("pi-4b").unwrap().to_string(), "pi-4b");
        assert!(HostName::new("").unwrap().is_empty());
        assert_eq!(HostName::empty(), HostName::new("").unwrap());
        assert!(HostName::new("a".repeat(63)).is_ok());
    }

    #[test]
    fn hostname_rejects_case_dots_spaces_and_length() {
        assert_eq!(HostName::new("Laptop"), Err(HostNameError::BadChar(0)));
        assert_eq!(HostName::new("nas.local"), Err(HostNameError::BadChar(3)));
        assert_eq!(HostName::new("my box"), Err(HostNameError::BadChar(2)));
        assert_eq!(HostName::new("é"), Err(HostNameError::BadChar(0)));
        assert_eq!(
            HostName::new("a".repeat(64)),
            Err(HostNameError::TooLong(64))
        );
    }

    #[test]
    fn hostname_serde_validates_on_the_way_in() {
        let h: HostName = serde_json::from_str("\"desktop\"").unwrap();
        assert_eq!(h.as_str(), "desktop");
        assert_eq!(serde_json::to_string(&h).unwrap(), "\"desktop\"");
        assert!(serde_json::from_str::<HostName>("\"Desktop\"").is_err());
        let bytes = postcard::to_stdvec(&h).unwrap();
        assert_eq!(postcard::from_bytes::<HostName>(&bytes).unwrap(), h);
    }
}
