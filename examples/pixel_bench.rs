//! CPU reader benchmark, warm file cache. Run with --release --features link.
use rewrite_bridge::link::*;
use std::time::Instant;
fn main() {
    for (file_width, file_height, width, height) in [
        (1280, 720, 1280, 720),
        (1920, 1080, 1920, 1080),
        (3840, 2160, 3840, 2160),
        (8192, 8192, 8192, 8192),
        (16384, 1, 16384, 1),
        (16384, 4096, 16384, 4096),
        (1920, 1080, 1280, 720),
        (3840, 2160, 1920, 1080),
        (16384, 4096, 8192, 4096),
        (16384, 4096, 1, 4096),
    ] {
        if std::env::var_os("RBL_BENCH_CROPPED_ONLY").is_some() && width == file_width {
            continue;
        }
        for with_depth in [false, true] {
            let mut opts = FrameFileOptions::new(file_width, file_height, 1);
            if with_depth {
                opts = opts.with_depth();
            }
            let mut w = FrameWriter::create_temp_with("bench", &opts).unwrap();
            let pixels = vec![7; width as usize * height as usize * 4];
            let dep = with_depth.then(|| vec![2.0; width as usize * height as usize]);
            let ready = w
                .write_depth(&pixels, dep.as_deref(), width, height, 0, 1)
                .unwrap();
            let r = FrameReader::open(w.path()).unwrap();
            let (mut rgba, mut depth) = (vec![], vec![]);
            let n = if width as u64 * height as u64 > 10_000_000 {
                5
            } else {
                30
            };
            let mut times = Vec::new();
            for i in 0..n + 2 {
                let start = Instant::now();
                assert!(r.read_depth(&ready, &mut rgba, &mut depth).is_ok());
                if i >= 2 {
                    times.push(start.elapsed().as_secs_f64() * 1000.0);
                }
                std::hint::black_box((&rgba, &depth));
            }
            times.sort_by(|a, b| a.total_cmp(b));
            println!(
                "rust,{file_width},{file_height},{width},{height},{with_depth},{n},{:.6},{:.6},{:.6}",
                times[times.len() / 10],
                times[times.len() / 2],
                times[(times.len() * 95 / 100).min(times.len() - 1)]
            );
        }
    }
}
