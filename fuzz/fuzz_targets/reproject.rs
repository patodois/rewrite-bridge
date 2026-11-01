//! `compose::reproject`, `composite`, `linearize_*` with cameras that came from a peer (any f32 in
//! any field) and small images: never a panic or a hang.
#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use rewrite_bridge::link::compose::{composite, delinearize_depth, linearize_buffer, linearize_depth, reproject};
use rewrite_bridge::link::{Camera, DepthRange};

#[derive(Arbitrary, Debug)]
struct Cam {
    view: [f32; 16],
    proj: [f32; 16],
    fov_y: f32,
    near: f32,
    far: f32,
    viewport: [u32; 2],
    range: u8,
}

impl Cam {
    fn camera(&self) -> Camera {
        Camera {
            view: self.view,
            proj: self.proj,
            fov_y: self.fov_y,
            near: self.near,
            far: self.far,
            viewport: self.viewport,
            depth_range: match self.range % 3 {
                0 => DepthRange::NegOneToOne,
                1 => DepthRange::ZeroToOne,
                _ => DepthRange::Reversed,
            },
        }
    }
}

#[derive(Arbitrary, Debug)]
struct Input {
    render: Cam,
    present: Cam,
    w: u8,
    h: u8,
    dst_w: u8,
    dst_h: u8,
    rgba: Vec<u8>,
    depth: Vec<f32>,
}

fuzz_target!(|i: Input| {
    let (render, present) = (i.render.camera(), i.present.camera());
    let (w, h) = (u32::from(i.w % 17), u32::from(i.h % 17));
    let n = (w * h) as usize;
    // Pad or cut the arbitrary vectors to the sizes the function wants, and also pass them as they are.
    let mut rgba = i.rgba.clone();
    rgba.resize(n * 4, 7);
    let mut depth = i.depth.clone();
    depth.resize(n, 1.0);
    let _ = reproject(
        &rgba,
        &depth,
        w,
        h,
        &render,
        &present,
        u32::from(i.dst_w % 33),
        u32::from(i.dst_h % 33),
    );
    let _ = reproject(&i.rgba, &i.depth, w, h, &render, &present, 3, 3);
    let lin = linearize_buffer(&depth, &render);
    let _ = composite(&rgba, &lin, &rgba, &depth);
    let _ = composite(&i.rgba, &i.depth, &rgba, &depth);
    if let Some(&d) = i.depth.first() {
        let l = linearize_depth(d, render.near, render.far, render.depth_range);
        let _ = delinearize_depth(l, render.near, render.far, render.depth_range);
    }
});
