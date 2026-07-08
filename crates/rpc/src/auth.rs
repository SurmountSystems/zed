use base64::prelude::*;
use rand::prelude::*;

/// Generate a random 64-character base64 string.
pub fn random_token() -> String {
    let mut rng = rand::rng();
    let mut token_bytes = [0; 48];
    for byte in token_bytes.iter_mut() {
        *byte = rng.random();
    }
    BASE64_URL_SAFE.encode(token_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_token_is_url_safe_and_expected_length() {
        for _ in 0..5 {
            let token = random_token();
            assert_eq!(token.len(), 64);
            for c in token.chars() {
                assert!(c.is_ascii_graphic());
                assert_ne!(c, '/');
                assert_ne!(c, '&');
            }
        }
    }
}