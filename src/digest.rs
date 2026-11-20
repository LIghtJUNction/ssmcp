//! Stable hexadecimal formatting for digest byte arrays.
use std::fmt;

/// Preserve the digest encoding used in persistent identifiers and checksums.
pub(crate) struct Hex<T>(pub T);

impl<T: AsRef<[u8]>> fmt::LowerHex for Hex<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0.as_ref() {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::Hex;
    use sha2::{Digest, Sha256};

    #[test]
    fn preserves_each_byte_and_leading_zeroes() {
        assert_eq!(format!("{:x}", Hex([0, 1, 15, 16, 255])), "00010f10ff");
        assert_eq!(format!("{:x}", Hex([] as [u8; 0])), "");
    }

    #[test]
    fn matches_known_sha256_and_persistent_identifier_encoding() {
        let digest = Sha256::digest(b"abc");
        let expected = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert_eq!(format!("{:x}", Hex(&digest)), expected);
        assert_eq!(
            format!("wait_{:x}.json", Hex(digest)),
            format!("wait_{expected}.json")
        );
        let mut incremental = Sha256::new();
        incremental.update(b"a");
        incremental.update(b"bc");
        assert_eq!(format!("{:x}", Hex(incremental.finalize())), expected);
    }
}
