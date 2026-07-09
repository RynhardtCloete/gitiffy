//! Git object identifiers.
//!
//! Stored as raw bytes with an explicit length so both SHA-1 (20 byte) and
//! SHA-256 (32 byte) object hashes are representable without an allocation.
//! Unused trailing bytes are always zeroed so the derived [`PartialEq`]/[`Hash`]
//! stay correct.

use std::fmt;
use std::str::FromStr;

use crate::error::ParseOidError;

/// Maximum hash length we store inline (SHA-256 = 32 bytes).
const MAX_OID_LEN: usize = 32;

/// A git object id (commit, tree, blob, or tag hash).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Oid {
    bytes: [u8; MAX_OID_LEN],
    len: u8,
}

impl Oid {
    /// The all-zero SHA-1 oid, used as a "null" / not-yet-known sentinel.
    pub const ZERO: Oid = Oid {
        bytes: [0; MAX_OID_LEN],
        len: 20,
    };

    /// Build an `Oid` from raw hash bytes. The slice length must be a valid
    /// git hash length (20 or 32 bytes).
    pub fn from_bytes(raw: &[u8]) -> Result<Self, ParseOidError> {
        if raw.len() != 20 && raw.len() != 32 {
            return Err(ParseOidError::BadLength(raw.len()));
        }
        let mut bytes = [0u8; MAX_OID_LEN];
        bytes[..raw.len()].copy_from_slice(raw);
        Ok(Oid {
            bytes,
            len: raw.len() as u8,
        })
    }

    /// Parse from a full hex string (40 or 64 hex chars).
    pub fn from_hex(hex: &str) -> Result<Self, ParseOidError> {
        let hex = hex.trim();
        if hex.len() != 40 && hex.len() != 64 {
            return Err(ParseOidError::BadLength(hex.len()));
        }
        let n = hex.len() / 2;
        let mut bytes = [0u8; MAX_OID_LEN];
        let raw = hex.as_bytes();
        for i in 0..n {
            let hi = hex_val(raw[i * 2])?;
            let lo = hex_val(raw[i * 2 + 1])?;
            bytes[i] = (hi << 4) | lo;
        }
        Ok(Oid {
            bytes,
            len: n as u8,
        })
    }

    /// Raw hash bytes.
    #[inline]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len as usize]
    }

    /// True if this is the all-zero sentinel oid.
    #[inline]
    pub fn is_zero(&self) -> bool {
        self.as_bytes().iter().all(|&b| b == 0)
    }

    /// Lowercase full hex representation.
    pub fn to_hex(&self) -> String {
        let mut s = String::with_capacity(self.len as usize * 2);
        for &b in self.as_bytes() {
            s.push(char::from_digit((b >> 4) as u32, 16).unwrap());
            s.push(char::from_digit((b & 0xf) as u32, 16).unwrap());
        }
        s
    }

    /// Abbreviated hex (first `n` chars), the common UI rendering.
    pub fn short(&self, n: usize) -> String {
        let full = self.to_hex();
        full.chars().take(n).collect()
    }
}

fn hex_val(c: u8) -> Result<u8, ParseOidError> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(ParseOidError::BadChar(c as char)),
    }
}

impl fmt::Display for Oid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for Oid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Oid({})", self.short(10))
    }
}

impl FromStr for Oid {
    type Err = ParseOidError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Oid::from_hex(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_sha1() {
        let hex = "0123456789abcdef0123456789abcdef01234567";
        let oid = Oid::from_hex(hex).unwrap();
        assert_eq!(oid.to_hex(), hex);
        assert_eq!(oid.as_bytes().len(), 20);
        assert_eq!(oid.short(7), "0123456");
    }

    #[test]
    fn roundtrip_sha256() {
        let hex = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let oid = Oid::from_hex(hex).unwrap();
        assert_eq!(oid.to_hex(), hex);
        assert_eq!(oid.as_bytes().len(), 32);
    }

    #[test]
    fn rejects_bad_input() {
        assert!(Oid::from_hex("abc").is_err());
        assert!(Oid::from_hex("zz23456789abcdef0123456789abcdef01234567").is_err());
    }

    #[test]
    fn zero_is_zero() {
        assert!(Oid::ZERO.is_zero());
        assert!(!Oid::from_hex("0123456789abcdef0123456789abcdef01234567")
            .unwrap()
            .is_zero());
    }
}
