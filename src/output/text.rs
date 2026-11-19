//! Incremental Unicode-scalar counting without retaining or repairing text.

pub(super) struct Counter {
    characters: u64,
    valid: bool,
    pending: [u8; 4],
    pending_len: usize,
}

impl Default for Counter {
    fn default() -> Self {
        Self {
            characters: 0,
            valid: true,
            pending: [0; 4],
            pending_len: 0,
        }
    }
}

impl Counter {
    pub(super) fn characters(&self) -> Option<u64> {
        (self.valid && self.pending_len == 0).then_some(self.characters)
    }

    pub(super) fn push(&mut self, mut bytes: &[u8]) {
        if !self.valid {
            return;
        }
        if self.pending_len != 0 {
            let needed = match self.pending[0] {
                0xc2..=0xdf => 2,
                0xe0..=0xef => 3,
                0xf0..=0xf4 => 4,
                _ => {
                    self.valid = false;
                    return;
                }
            };
            let take = (needed - self.pending_len).min(bytes.len());
            self.pending[self.pending_len..self.pending_len + take].copy_from_slice(&bytes[..take]);
            self.pending_len += take;
            bytes = &bytes[take..];
            if self.pending_len != needed {
                return;
            }
            if std::str::from_utf8(&self.pending[..needed]).is_err() {
                self.valid = false;
                return;
            }
            self.characters += 1;
            self.pending_len = 0;
        }
        if bytes.is_ascii() {
            self.characters += bytes.len() as u64;
            return;
        }
        match std::str::from_utf8(bytes) {
            Ok(text) => self.characters += text.chars().count() as u64,
            Err(error) => {
                if error.error_len().is_some() {
                    self.valid = false;
                    return;
                }
                let (prefix, trailing) = bytes.split_at(error.valid_up_to());
                self.characters += std::str::from_utf8(prefix)
                    .expect("validated UTF-8 prefix")
                    .chars()
                    .count() as u64;
                self.pending[..trailing.len()].copy_from_slice(trailing);
                self.pending_len = trailing.len();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_fragment_boundary_preserves_scalar_counts() {
        let sample = "ASCII\n中e\u{301}🌍\r\nend";
        for size in 1..=sample.len() {
            let mut counter = Counter::default();
            for bytes in sample.as_bytes().chunks(size) {
                counter.push(bytes);
            }
            assert_eq!(
                counter.characters(),
                Some(sample.chars().count() as u64),
                "chunk={size}"
            );
        }
        assert_eq!(Counter::default().characters(), Some(0));
    }

    #[test]
    fn incomplete_scalars_are_unknown_until_their_bytes_arrive() {
        let mut counter = Counter::default();
        counter.push(b"start\xe4\xb8");
        assert_eq!(counter.characters(), None);
        counter.push(b"\xad end");
        assert_eq!(counter.characters(), Some(10));
    }

    #[test]
    fn binary_surrogates_overlong_sequences_and_invalid_fragments_are_unknown() {
        for invalid in [
            b"\xff".as_slice(),
            b"\xc0\x80",
            b"\xed\xa0\x80",
            b"\xf4\x90\x80\x80",
            b"\xe4\x41\x80",
            b"\x80",
        ] {
            for size in 1..=invalid.len() {
                let mut counter = Counter::default();
                counter.push(b"valid prefix");
                for bytes in invalid.chunks(size) {
                    counter.push(bytes);
                }
                counter.push(b"valid suffix");
                assert_eq!(counter.characters(), None);
            }
        }
    }
}
