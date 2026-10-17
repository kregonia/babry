//! 可重复的编辑热点基准：4K 原图、选区内自由笔、局部上传和连续提交。
//! cargo run --release --locked --example editor_bench
use babry::{document::EditorDocument, editor::Mark};
use image::{Rgba, RgbaImage};
use std::{hint::black_box, sync::Arc, time::Instant};

fn main() {
    let source = Arc::new(RgbaImage::from_pixel(3840, 2160, Rgba([25, 30, 35, 255])));
    let mut document = EditorDocument::new(source);
    document.select((0.25, 0.25, 0.25, 0.25));
    let mark = Mark::new(1, 0.25, 0.25, 0.25, 0.25, Rgba([255, 0, 0, 255]), 4);
    document.begin(mark);
    let start = Instant::now();
    let mut max_patch_bytes = 0;
    for i in 1..=120 {
        document
            .pending_mut()
            .unwrap()
            .update_endpoint(0.25 + i as f32 / 2000.0, 0.25 + i as f32 / 3000.0);
        let patch = document.preview().unwrap();
        max_patch_bytes = max_patch_bytes.max(patch.image.len());
        let pixels = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
            patch.image.as_raw(),
            patch.image.width(),
            patch.image.height(),
        );
        black_box(pixels);
    }
    let preview_ms = start.elapsed().as_secs_f64() * 1000.0;
    let mark = document.pending_mut().unwrap().clone();
    let start = Instant::now();
    for _ in 0..20 {
        document.begin(mark.clone());
        document.commit();
        black_box(document.committed());
    }
    println!(
        "{}",
        serde_json::json!({
            "image": [3840,2160], "selection": [960,540],
            "preview_120_ms": preview_ms,
            "commit_20_ms": start.elapsed().as_secs_f64()*1000.0,
            "persistent_cpu_pixel_bytes": document.source().len()+document.committed().map_or(0,|image|image.len()),
            "max_preview_pixel_bytes": max_patch_bytes,
        })
    );
}
