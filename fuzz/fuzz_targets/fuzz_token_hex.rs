//! Fuzzes token construction and comparison.
//!
//! Arbitrary bytes must be answered rather than fatal, and a token must verify against its
//! own bytes and refuse every other byte string of the same length.

#![no_main]

use ncui_fuzz_target::fuzz_target;
use ncui_security::token::SecretToken;

fuzz_target!(|data: &[u8]| {
    let Ok(token) = SecretToken::new(data.to_vec()) else {
        return;
    };
    assert!(
        token.verify(token.expose_secret()),
        "a token must verify itself"
    );

    // Flipping one bit must break verification.
    if let Some(last) = data.last() {
        let mut altered = data.to_vec();
        if let Some(slot) = altered.last_mut() {
            *slot ^= 0x01;
            assert!(
                !token.verify(&altered),
                "a one-bit change verified against the token"
            );
        }
        let _ = last;
    }
});
