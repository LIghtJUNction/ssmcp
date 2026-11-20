//! Fixed-width lowercase hex used by persisted digests and identifiers.

use std::fmt::Write as _;

/// Keep digest text independent of the hash crate's output container.
pub(crate) fn encode(bytes: impl AsRef<[u8]>) -> String {
    let bytes = bytes.as_ref();
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut text, "{byte:02x}").expect("writing to String");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    #[test]
    fn digest_text_preserves_known_sha256_values_and_zero_padding() {
        assert_eq!(encode([]), "");
        assert_eq!(encode([0, 1, 15, 16, 255]), "00010f10ff");
        assert_eq!(
            encode(Sha256::digest(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            encode(Sha256::digest(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );

        let mut hash = Sha256::new();
        hash.update(b"hello ");
        hash.update(b"world");
        let bytes: [u8; 32] = hash.finalize().into();
        assert_eq!(
            encode(bytes),
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
    }
}
