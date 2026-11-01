//! `Endpoint::parse` on arbitrary bytes (the contents of an endpoint file).
#![no_main]

use libfuzzer_sys::fuzz_target;
use rewrite_bridge::link::{Endpoint, MAX_ENDPOINT_FILE, MAX_TOKEN_BYTES};

fuzz_target!(|data: &[u8]| {
    let Ok(e) = Endpoint::parse(data) else {
        return;
    };
    assert!(data.len() <= MAX_ENDPOINT_FILE);
    assert_ne!(e.control_port, 0);
    assert!((1..=MAX_TOKEN_BYTES).contains(&e.token.len()));
    assert!(!e.token.contains('\0'));
});
