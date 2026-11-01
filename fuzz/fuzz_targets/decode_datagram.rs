//! `decode_datagram` / `decode_header` on arbitrary bytes: never a panic, and whatever decodes
//! re-encodes to bytes that decode to the same bytes again.
#![no_main]

use libfuzzer_sys::fuzz_target;
use rewrite_bridge::link::{decode_datagram, decode_header, encode_datagram, Datagram};

fuzz_target!(|data: &[u8]| {
    let header = decode_header(data);
    let Ok((h, body)) = decode_datagram(data) else {
        return;
    };
    // A datagram that decodes has a header that decodes, to the same values.
    let (h2, kind) = header.expect("decode_datagram succeeded, decode_header must too");
    assert_eq!(h, h2);
    assert_eq!(kind, body.kind());
    if matches!(body, Datagram::Unknown { .. }) {
        return;
    }
    let mut a = Vec::new();
    encode_datagram(&h, &body, &mut a).expect("a decoded datagram can be encoded");
    assert!(a.len() <= rewrite_bridge::link::MAX_DATAGRAM);
    let (h3, body3) = decode_datagram(&a).expect("what we encode decodes");
    let mut b = Vec::new();
    encode_datagram(&h3, &body3, &mut b).unwrap();
    assert_eq!(a, b, "encode(decode(encode(x))) is stable");
});
