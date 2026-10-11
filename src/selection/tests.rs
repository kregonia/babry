use super::*;
use image::{Rgba, imageops};
use imageproc::{
    drawing::{draw_filled_rect_mut, draw_hollow_rect_mut},
    rect::Rect,
};

fn canvas(width: u32, height: u32, shade: u8) -> RgbaImage {
    RgbaImage::from_pixel(width, height, Rgba([shade, shade, shade, 255]))
}

fn fill(image: &mut RgbaImage, rect: RectPx, color: [u8; 3]) {
    draw_filled_rect_mut(
        image,
        Rect::at(rect.x, rect.y).of_size(rect.width, rect.height),
        Rgba([color[0], color[1], color[2], 255]),
    );
}

fn assert_bounds(actual: RectPx, expected: RectPx, tolerance: i32) {
    assert!(
        (actual.x - expected.x).abs() <= tolerance
            && (actual.y - expected.y).abs() <= tolerance
            && (actual.right() - expected.right()).abs() <= tolerance
            && (actual.bottom() - expected.bottom()).abs() <= tolerance,
        "selected {actual:?}, expected {expected:?} ±{tolerance}px"
    );
}

#[test]
fn flat_and_tiny_images_fall_back_without_panicking() {
    for (w, h) in [(320, 200), (1, 1), (2, 3), (3, 3), (1, 80), (0, 0)] {
        let detector = RegionDetector::new(&canvas(w, h, 40));
        assert_eq!(detector.region_at(1.0, 1.0), RectPx::new(0, 0, w, h));
    }
}

#[test]
fn nested_controls_keep_exact_boundaries_and_parent_chain() {
    let mut image = canvas(360, 240, 20);
    let panel = RectPx::new(30, 20, 300, 200);
    let button = RectPx::new(100, 80, 120, 48);
    fill(&mut image, panel, [45, 45, 45]);
    fill(&mut image, button, [75, 75, 75]);
    let detector = RegionDetector::new(&image);
    assert_bounds(detector.region_at(0.4, 0.4), button, 1);
    let chain = detector.regions_at(0.4, 0.4);
    assert!(chain.contains(&panel), "missing panel in {chain:?}");
    assert_eq!(chain.last(), Some(&RectPx::new(0, 0, 360, 240)));
}

#[test]
fn detects_weak_dark_panel_and_equal_luminance_color_boundaries() {
    for (outside, inside) in [
        ([28, 28, 28], [36, 36, 36]),
        ([200, 200, 200], [208, 208, 208]),
        ([160, 50, 50], [50, 106, 50]),
    ] {
        let mut image = canvas(360, 240, 0);
        fill(&mut image, RectPx::new(0, 0, 360, 240), outside);
        let rect = RectPx::new(60, 45, 230, 140);
        fill(&mut image, rect, inside);
        let detector = RegionDetector::new(&image);
        assert_bounds(detector.region_at(0.5, 0.5), rect, 1);
    }
}

#[test]
fn weak_panel_borders_survive_bright_text_in_the_same_image() {
    let mut image = canvas(640, 360, 28);
    let panel = RectPx::new(35, 30, 560, 290);
    fill(&mut image, panel, [36, 36, 36]);
    for row in 0..9 {
        for column in 0..28 {
            let x = 55 + column * 18;
            let y = 48 + row * 28;
            fill(&mut image, RectPx::new(x, y, 2, 17), [220, 220, 220]);
            fill(&mut image, RectPx::new(x, y, 10, 2), [220, 220, 220]);
            fill(&mut image, RectPx::new(x, y + 8, 8, 2), [220, 220, 220]);
        }
    }
    let detector = RegionDetector::new(&image);
    assert_bounds(detector.region_at(0.8, 0.7), panel, 1);
}

#[test]
fn adjacent_panels_are_not_merged_and_screen_edges_are_clipped() {
    let mut image = canvas(360, 240, 22);
    let left = RectPx::new(0, 30, 150, 210);
    let right = RectPx::new(162, 30, 198, 210);
    fill(&mut image, left, [45, 45, 45]);
    fill(&mut image, right, [50, 50, 50]);
    let detector = RegionDetector::new(&image);
    assert_bounds(detector.region_at(0.2, 0.5), left, 1);
    assert_bounds(detector.region_at(0.7, 0.5), right, 1);
}

#[test]
fn hollow_and_rounded_controls_survive_boundary_validation() {
    let mut image = canvas(400, 250, 30);
    draw_hollow_rect_mut(
        &mut image,
        Rect::at(25, 30).of_size(160, 150),
        Rgba([210, 210, 210, 255]),
    );
    let rounded = RectPx::new(225, 65, 130, 80);
    for y in rounded.y..rounded.bottom() {
        for x in rounded.x..rounded.right() {
            let dx = (rounded.x + 12 - x).max(x - (rounded.right() - 13)).max(0);
            let dy = (rounded.y + 12 - y).max(y - (rounded.bottom() - 13)).max(0);
            if dx * dx + dy * dy <= 144 {
                image[(x as u32, y as u32)] = Rgba([50, 50, 50, 255]);
            }
        }
    }
    let detector = RegionDetector::new(&image);
    assert_bounds(
        detector.region_at(0.2, 0.4),
        RectPx::new(25, 30, 160, 150),
        1,
    );
    assert_bounds(detector.region_at(0.7, 0.4), rounded, 1);
}

#[test]
fn original_resolution_refinement_preserves_small_controls_on_4k() {
    let mut image = canvas(3840, 2160, 24);
    let panel = RectPx::new(421, 213, 1500, 1100);
    let button = RectPx::new(631, 451, 30, 30);
    fill(&mut image, panel, [42, 42, 42]);
    fill(&mut image, button, [100, 100, 100]);
    let detector = RegionDetector::new(&image);
    assert_bounds(
        detector.region_at(645.0 / 3840.0, 465.0 / 2160.0),
        button,
        2,
    );
    assert_bounds(
        detector.region_at(1200.0 / 3840.0, 900.0 / 2160.0),
        panel,
        2,
    );
}

#[test]
fn text_inside_a_button_does_not_steal_the_selection() {
    let mut image = canvas(640, 360, 25);
    let panel = RectPx::new(30, 30, 580, 300);
    let button = RectPx::new(180, 140, 250, 60);
    fill(&mut image, panel, [42, 42, 42]);
    fill(&mut image, button, [65, 65, 65]);
    // 确定性的位图字形，不依赖测试机器安装的字体。
    for (index, glyph) in [
        [14, 17, 17, 17, 17, 17, 14], // O
        [30, 17, 17, 30, 16, 16, 16], // P
        [31, 16, 16, 30, 16, 16, 31], // E
        [17, 25, 21, 19, 17, 17, 17], // N
    ]
    .iter()
    .enumerate()
    {
        for (y, row) in glyph.iter().enumerate() {
            for x in 0..5 {
                if row & (1 << (4 - x)) != 0 {
                    fill(
                        &mut image,
                        RectPx::new(202 + index as i32 * 30 + x * 3, 157 + y as i32 * 3, 3, 3),
                        [225, 225, 225],
                    );
                }
            }
        }
    }
    let detector = RegionDetector::new(&image);
    for x in [205, 215, 240, 285, 310] {
        assert_bounds(
            detector.region_at(x as f32 / 640.0, 168.0 / 360.0),
            button,
            1,
        );
    }
}

#[test]
fn compact_buttons_with_icons_remain_selectable() {
    let mut image = canvas(360, 240, 20);
    let panel = RectPx::new(30, 20, 300, 200);
    let button = RectPx::new(100, 80, 24, 24);
    fill(&mut image, panel, [45, 45, 45]);
    fill(&mut image, button, [85, 85, 85]);
    fill(&mut image, RectPx::new(110, 84, 4, 16), [220, 220, 220]);
    fill(&mut image, RectPx::new(104, 90, 16, 4), [220, 220, 220]);
    let detector = RegionDetector::new(&image);
    assert_bounds(detector.region_at(111.0 / 360.0, 91.0 / 240.0), button, 1);
}

#[test]
fn a_smooth_gradient_is_not_a_panel() {
    let image = RgbaImage::from_fn(480, 320, |x, y| {
        Rgba([(20 + x / 8) as u8, (30 + y / 6) as u8, 80, 255])
    });
    let detector = RegionDetector::new(&image);
    assert_eq!(detector.region_at(0.5, 0.5), RectPx::new(0, 0, 480, 320));
}

#[test]
fn wheel_choice_survives_jitter_and_click_refresh() {
    let mut image = canvas(360, 240, 20);
    let panel = RectPx::new(30, 20, 300, 200);
    let button = RectPx::new(100, 80, 120, 48);
    fill(&mut image, panel, [45, 45, 45]);
    fill(&mut image, button, [75, 75, 75]);
    let detector = RegionDetector::new(&image);
    let mut cursor = SelectionCursor::default();
    assert_bounds(cursor.hover(&detector, 0.4, 0.4), button, 1);
    assert_bounds(cursor.step(&detector, 0.4, 0.4, 1), panel, 1);
    assert_bounds(cursor.hover(&detector, 0.401, 0.401), panel, 1);
    assert_bounds(cursor.step(&detector, 0.4, 0.4, -1), button, 1);
    cursor.step(&detector, 0.4, 0.4, 1);
    assert_eq!(
        cursor.step(&detector, 0.4, 0.4, 1),
        RectPx::new(0, 0, 360, 240)
    );
    assert_eq!(
        cursor.step(&detector, 0.4, 0.4, 1),
        RectPx::new(0, 0, 360, 240)
    );
}

#[test]
fn quality_can_outweigh_a_small_noisy_candidate() {
    let small = RegionCandidate {
        rect: RectPx::new(50, 50, 20, 20),
        quality: 0.60,
        sides: [0.6; 4],
        interior_edges: 0.3,
        sources: vec!["contour"],
    };
    let large = RegionCandidate {
        rect: RectPx::new(20, 20, 240, 100),
        quality: 0.95,
        sides: [0.95; 4],
        interior_edges: 0.1,
        sources: vec!["background"],
    };
    let detector = RegionDetector {
        image_width: 320,
        image_height: 200,
        candidates: vec![small, large],
    };
    assert_eq!(
        detector.region_at(0.18, 0.28),
        RectPx::new(20, 20, 240, 100)
    );
}

#[test]
fn real_toolbar_selection_prefers_the_toolbar_over_label_fragments() {
    let image = image::load_from_memory(include_bytes!("../../docs/images/toolbar-context.png"))
        .unwrap()
        .to_rgba8();
    let detector = RegionDetector::new(&image);
    // 实际运行截图中的文字工具名称；应选择完整工具栏，而非“文字”两字。
    assert_bounds(
        detector.region_at(1310.0 / 1600.0, 181.0 / 205.0),
        RectPx::new(100, 70, 1350, 119),
        3,
    );
    // 选中的笔按钮仍可单独命中，避免去噪把真正的小控件一并删除。
    assert_bounds(
        detector.region_at(195.0 / 1600.0, 115.0 / 205.0),
        RectPx::new(176, 93, 44, 44),
        2,
    );
}

#[test]
fn dense_text_rows_do_not_create_competing_controls() {
    let mut image = canvas(640, 360, 20);
    let panel = RectPx::new(30, 30, 580, 300);
    fill(&mut image, panel, [45, 45, 45]);
    for row in 0..8 {
        for column in 0..18 {
            let x = 60 + column * 28;
            let y = 55 + row * 30;
            // 类似空心字形的密集文本，最容易被误判为小矩形控件。
            draw_hollow_rect_mut(
                &mut image,
                Rect::at(x, y).of_size(11, 18),
                Rgba([200, 200, 200, 255]),
            );
        }
    }
    let detector = RegionDetector::new(&image);
    for column in [0, 4, 8, 12, 16] {
        assert_bounds(
            detector.region_at((65 + column * 28) as f32 / 640.0, 65.0 / 360.0),
            panel,
            1,
        );
    }
}

#[test]
fn broken_outline_and_content_separators_preserve_the_outer_window() {
    let mut image = canvas(600, 400, 20);
    let window = RectPx::new(70, 40, 450, 320);
    fill(&mut image, window, [45, 45, 45]);
    for y in [100, 240] {
        fill(&mut image, RectPx::new(70, y, 450, 2), [80, 80, 80]);
    }
    // 轻微断边不能使窗口候选消失。
    fill(&mut image, RectPx::new(70, 180, 2, 3), [20, 20, 20]);
    let detector = RegionDetector::new(&image);
    let chain = detector.regions_at(0.5, 0.4);
    assert!(
        chain.iter().any(|rect| (rect.x - window.x).abs() <= 2
            && (rect.y - window.y).abs() <= 2
            && (rect.right() - window.right()).abs() <= 2
            && (rect.bottom() - window.bottom()).abs() <= 2),
        "outer window missing: {chain:?}"
    );
}

#[test]
fn crossing_candidates_do_not_hide_a_valid_parent() {
    let make = |rect, quality| RegionCandidate {
        rect,
        quality,
        sides: [quality; 4],
        interior_edges: 0.1,
        sources: vec!["contour"],
    };
    let control = RectPx::new(80, 80, 100, 60);
    let parent = RectPx::new(40, 40, 220, 160);
    let detector = RegionDetector {
        image_width: 320,
        image_height: 240,
        candidates: vec![
            make(RectPx::new(120, 60, 30, 40), 0.6),
            make(control, 0.95),
            make(parent, 0.95),
        ],
    };
    let mut cursor = SelectionCursor::default();
    assert_eq!(cursor.hover(&detector, 0.4, 0.4), control);
    assert_eq!(cursor.step(&detector, 0.4, 0.4, 1), parent);
    let chain = detector.regions_at(0.4, 0.4);
    assert!(chain.contains(&control));
    assert!(chain.contains(&parent));
}

#[test]
fn diagnostic_files_preserve_image_geometry_and_candidate_ids() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("source.png");
    let mut image = canvas(180, 120, 20);
    fill(&mut image, RectPx::new(30, 20, 100, 60), [65, 65, 65]);
    image.save(&source).unwrap();
    let output = directory.path().join("report");
    write_diagnostics(&source, &output).unwrap();
    let overlay = image::open(output.join("candidates.png")).unwrap();
    assert_eq!((overlay.width(), overlay.height()), (180, 120));
    let json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(output.join("candidates.json")).unwrap()).unwrap();
    assert_eq!(json["image_width"], 180);
    assert_eq!(json["image_height"], 120);
    let candidates = json["candidates"].as_array().unwrap();
    assert!(!candidates.is_empty());
    for (id, candidate) in candidates.iter().enumerate() {
        assert_eq!(candidate["id"], id);
        assert!(candidate["quality"].as_f64().unwrap().is_finite());
        assert!(candidate["rect"]["width"].as_u64().unwrap() > 0);
        assert!(!candidate["sources"].as_array().unwrap().is_empty());
    }
}

#[test]
fn scales_consistently_without_changing_the_region_hierarchy() {
    let mut image = canvas(360, 240, 20);
    let expected = RectPx::new(70, 60, 180, 100);
    fill(&mut image, expected, [50, 50, 50]);
    let scaled = imageops::resize(&image, 1440, 960, imageops::FilterType::Nearest);
    let detector = RegionDetector::new(&scaled);
    assert_bounds(
        detector.region_at(0.4, 0.4),
        RectPx::new(280, 240, 720, 400),
        2,
    );
}
