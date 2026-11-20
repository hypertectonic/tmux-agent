use std::fmt::Write;

pub(crate) fn sha256_hex(digest: [u8; 32]) -> String {
    let mut hex = String::with_capacity(64);
    for byte in digest {
        write!(hex, "{byte:02x}").expect("writing to a string cannot fail");
    }
    hex
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    #[test]
    fn sha256_hex_preserves_canonical_lowercase_encoding() {
        assert_eq!(
            sha256_hex(Sha256::digest(b"hello world").into()),
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
    }
}
