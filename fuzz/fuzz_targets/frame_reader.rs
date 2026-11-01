//! `FrameReader::open` on arbitrary file contents, then `read` / `read_depth` for the frame
//! announcement in the first 24 bytes of the input (frame u64, slot, camera_seq, width, height as
//! little-endian). The rest of the input is the pixel file. A reader that opens must never
//! hand back more bytes than the file can hold.
#![no_main]

use libfuzzer_sys::fuzz_target;
use rewrite_bridge::link::{FrameReader, FrameReady};
use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;

fn path() -> &'static PathBuf {
    static P: OnceLock<PathBuf> = OnceLock::new();
    P.get_or_init(|| {
        let dir = if std::path::Path::new("/dev/shm").is_dir() {
            PathBuf::from("/dev/shm")
        } else {
            std::env::temp_dir()
        };
        dir.join(format!("rb-fuzz-frame-{}.bin", std::process::id()))
    })
}

fuzz_target!(|data: &[u8]| {
    if data.len() < 24 {
        return;
    }
    let (a, file) = data.split_at(24);
    let u32_at = |i: usize| u32::from_le_bytes(a[i..i + 4].try_into().unwrap());
    let ready = FrameReady {
        frame: u64::from_le_bytes(a[0..8].try_into().unwrap()),
        slot: u32_at(8),
        camera_seq: u32_at(12),
        width: u32_at(16),
        height: u32_at(20),
    };
    let p = path();
    {
        let mut f = std::fs::File::create(p).unwrap();
        f.write_all(file).unwrap();
    }
    let Ok(reader) = FrameReader::open(p) else {
        return;
    };
    let (mut rgba, mut depth) = (Vec::new(), Vec::new());
    if reader.read(&ready, &mut rgba).is_ok() {
        assert!(rgba.len() <= file.len());
    }
    if reader.read_depth(&ready, &mut rgba, &mut depth).is_ok() {
        assert!(rgba.len() <= file.len());
        assert!(depth.len() * 4 <= file.len());
    }
});
