use super::{RegionDetector, candidate_rank};
use crate::editor;
use ab_glyph::Font;
use image::Rgba;
use imageproc::{
    drawing::{draw_hollow_rect_mut, draw_text_mut},
    rect::Rect,
};
use std::{path::Path, time::Instant};

/// 离线诊断同一张截图，输出候选框、来源、分数与分析耗时；不启动截图窗口。
pub fn write_diagnostics(source: &Path, directory: &Path) -> Result<(), String> {
    let image = image::open(source)
        .map_err(|error| format!("读取图片失败: {error}"))?
        .to_rgba8();
    let started = Instant::now();
    let detector = RegionDetector::new(&image);
    let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
    let candidates = detector.candidates().iter().enumerate().map(|(id, candidate)| {
        serde_json::json!({
            "id": id,
            "rect": { "x": candidate.rect.x, "y": candidate.rect.y, "width": candidate.rect.width, "height": candidate.rect.height },
            "quality": candidate.quality,
            "rank": candidate_rank(candidate),
            "sides": candidate.sides,
            "interior_edges": candidate.interior_edges,
            "sources": candidate.sources,
        })
    }).collect::<Vec<_>>();
    let report = serde_json::json!({
        "schema_version": 1,
        "image_width": detector.image_width(),
        "image_height": detector.image_height(),
        "analysis_ms": elapsed_ms,
        "candidates": candidates,
    });

    let mut overlay = image;
    let font = editor::annotation_fonts().first().map(|entry| &entry.font);
    for (id, candidate) in detector.candidates().iter().enumerate().rev() {
        let rect = candidate.rect;
        let color = if candidate.quality >= 0.85 {
            Rgba([58, 220, 140, 255])
        } else {
            Rgba([255, 184, 65, 255])
        };
        draw_hollow_rect_mut(
            &mut overlay,
            Rect::at(rect.x, rect.y).of_size(rect.width, rect.height),
            color,
        );
        if let Some(font) = font {
            draw_text_mut(
                &mut overlay,
                color,
                rect.x + 2,
                rect.y + 2,
                font.pt_to_px_scale(11.0).unwrap_or_else(|| 14.0.into()),
                font,
                &id.to_string(),
            );
        }
    }
    std::fs::create_dir_all(directory).map_err(|error| error.to_string())?;
    overlay
        .save(directory.join("candidates.png"))
        .map_err(|error| error.to_string())?;
    let json = serde_json::to_string_pretty(&report).map_err(|error| error.to_string())?;
    std::fs::write(directory.join("candidates.json"), json).map_err(|error| error.to_string())?;
    eprintln!(
        "CV: {} 个候选，分析 {:.1} ms；诊断结果: {}",
        detector.candidates().len(),
        elapsed_ms,
        directory.display()
    );
    Ok(())
}
