//! `Control::parse` on arbitrary text: never a panic; a line that parses re-serialises to a line
//! that parses to a message that serialises to the same line.
#![no_main]

use libfuzzer_sys::fuzz_target;
use rewrite_bridge::link::{Control, MAX_LINE};

fuzz_target!(|data: &[u8]| {
    // The link only hands `parse` lines that are valid UTF-8 (see `Ctl::read_lines`).
    let Ok(line) = std::str::from_utf8(data) else {
        return;
    };
    let Ok(msg) = Control::parse(line) else {
        return;
    };
    let out = msg.to_line();
    assert!(out.ends_with('\n'));
    // A line we write is one line.
    assert_eq!(out.matches('\n').count(), 1);
    if out.len() >= MAX_LINE {
        return; // a long string field can make the echo too long for a receiver; not a decoder bug
    }
    let again = Control::parse(&out).expect("a line we write parses");
    assert_eq!(again.to_line(), out);
});
