//! Webhook authentication shared by the surfaces: HMAC-SHA256 hex signatures and
//! constant-time comparison of shared secrets.

use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

/// Whether `signature` is hex(HMAC-SHA256(secret, body)). The comparison is constant-time.
pub fn hmac_sha256_hex_matches(secret: &str, body: &[u8], signature: &str) -> bool {
    let Some(sig) = decode_hex(signature) else {
        return false;
    };
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC takes any key length");
    mac.update(body);
    mac.verify_slice(&sig).is_ok()
}

/// Compares in time that depends only on the lengths. An empty `expected` never matches, so an
/// unset secret cannot be satisfied by an empty one.
pub fn constant_time_eq(given: &[u8], expected: &[u8]) -> bool {
    !expected.is_empty()
        && given.len() == expected.len()
        && given
            .iter()
            .zip(expected)
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

/// Decodes hex digits in either case. Anything else, including the sign `from_str_radix` would
/// accept, is refused.
pub fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_sha256_hex() {
        let body = br#"{"zen":"hi"}"#;
        // python3 -c 'import hmac,hashlib;print(hmac.new(b"s",b"{\"zen\":\"hi\"}",hashlib.sha256).hexdigest())'
        let hex = "5b64481908428a9d02cbf79c42a7777672f9ec84641e7e227a2aab5637b35bb2";
        assert!(hmac_sha256_hex_matches("s", body, hex));
        assert!(hmac_sha256_hex_matches("s", body, &hex.to_uppercase()));
        assert!(!hmac_sha256_hex_matches("t", body, hex), "wrong secret");
        assert!(!hmac_sha256_hex_matches("s", b"{}", hex), "changed body");
        assert!(!hmac_sha256_hex_matches("s", body, &hex[..62]), "truncated");
        assert!(!hmac_sha256_hex_matches("s", body, "zz"), "not hex");
        assert!(!hmac_sha256_hex_matches("s", body, ""), "missing");
    }

    #[test]
    fn constant_time_eq_needs_a_nonempty_exact_match() {
        assert!(
            constant_time_eq(b"tok", b"tok")
                && !constant_time_eq(b"tok", b"tox")
                && !constant_time_eq(b"", b"")
        );
        assert!(!constant_time_eq(b"to", b"tok"));
        assert!(!constant_time_eq(b"tokk", b"tok"));
    }

    #[test]
    fn decode_hex_is_strict() {
        assert_eq!(decode_hex("00ff7A"), Some(vec![0x00, 0xff, 0x7a]));
        assert_eq!(decode_hex(""), Some(vec![]));
        assert_eq!(decode_hex("abc"), None, "odd length");
        assert_eq!(decode_hex("+a"), None, "sign");
        assert_eq!(decode_hex("-a"), None, "sign");
        assert_eq!(decode_hex("zz"), None);
        assert_eq!(decode_hex(" a"), None);
        assert_eq!(decode_hex("é"), None, "two-byte character");
    }
}
