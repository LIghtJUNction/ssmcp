//! Fixed-width lowercase hex used by persisted digests and identifiers.

/// Keep digest text independent of the hash crate's output container.
pub(crate) fn encode(bytes: impl AsRef<[u8]>) -> String {
    let bytes = bytes.as_ref();
    let mut text = vec![0; zhex::encoded_len(bytes.len()).expect("hex output length")];
    zhex::encode_lower(bytes, &mut text).expect("exact hex output capacity");
    String::from_utf8(text).expect("zhex produces ASCII")
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

    #[test]
    fn all_byte_values_keep_two_lowercase_digits() {
        let bytes: Vec<u8> = (0..=255).collect();
        assert_eq!(
            encode(&bytes),
            concat!(
                "000102030405060708090a0b0c0d0e0f",
                "101112131415161718191a1b1c1d1e1f",
                "202122232425262728292a2b2c2d2e2f",
                "303132333435363738393a3b3c3d3e3f",
                "404142434445464748494a4b4c4d4e4f",
                "505152535455565758595a5b5c5d5e5f",
                "606162636465666768696a6b6c6d6e6f",
                "707172737475767778797a7b7c7d7e7f",
                "808182838485868788898a8b8c8d8e8f",
                "909192939495969798999a9b9c9d9e9f",
                "a0a1a2a3a4a5a6a7a8a9aaabacadaeaf",
                "b0b1b2b3b4b5b6b7b8b9babbbcbdbebf",
                "c0c1c2c3c4c5c6c7c8c9cacbcccdcecf",
                "d0d1d2d3d4d5d6d7d8d9dadbdcdddedf",
                "e0e1e2e3e4e5e6e7e8e9eaebecedeeef",
                "f0f1f2f3f4f5f6f7f8f9fafbfcfdfeff",
            )
        );
    }

    #[test]
    fn empty_and_boundary_lengths_preserve_exact_output() {
        for len in [
            0, 1, 7, 15, 16, 17, 31, 32, 33, 63, 64, 65, 255, 256, 257, 4096,
        ] {
            let bytes = vec![0xaf; len];
            assert_eq!(encode(bytes.as_slice()), "af".repeat(len), "length {len}");
        }
    }

    #[test]
    fn byte_values_and_vector_boundaries_preserve_the_existing_format() {
        use std::fmt::Write as _;

        let source: Vec<u8> = (0..4098).map(|index| index as u8).collect();
        for length in [
            0, 1, 2, 7, 15, 16, 17, 31, 32, 33, 63, 64, 65, 127, 128, 129, 255, 256, 257, 1024,
            4096,
        ] {
            // The offset exercises unaligned buffers as well as every byte value.
            let input = &source[1..1 + length];
            let mut previous = String::new();
            for byte in input {
                write!(&mut previous, "{byte:02x}").unwrap();
            }
            assert_eq!(encode(input), previous, "input length {length}");
        }
    }
}
