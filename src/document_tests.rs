use crate::{
    document::EditorDocument,
    editor::{self, Mark},
};
use image::{Rgba, RgbaImage, imageops};
use std::sync::Arc;

fn source() -> Arc<RgbaImage> {
    Arc::new(RgbaImage::from_fn(480, 320, |x, y| {
        Rgba([(x * 7) as u8, (y * 5) as u8, (x + y) as u8, 255])
    }))
}

fn overlay(document: &EditorDocument) -> RgbaImage {
    let region = document.region();
    let mut canvas = document
        .committed()
        .cloned()
        .unwrap_or_else(|| editor::render_region(document.source(), region, document.marks()));
    if let Some(patch) = document.preview() {
        imageops::replace(
            &mut canvas,
            &patch.image,
            i64::from(patch.region.x - region.x),
            i64::from(patch.region.y - region.y),
        );
    }
    canvas
}

#[test]
fn empty_selection_and_cancel_allocate_no_edit_canvas() {
    let source = Arc::new(RgbaImage::new(3840, 2160));
    let mut document = EditorDocument::new(source.clone());
    document.select((0.25, 0.25, 0.2, 0.2));
    assert!(document.committed().is_none());
    assert!(document.preview().is_none());
    assert!(std::ptr::eq(source.as_ref(), document.source()));
    document.begin(Mark::new(1, 0.3, 0.3, 0.3, 0.3, Rgba([255, 0, 0, 255]), 4));
    let patch = document.preview().unwrap();
    assert!(
        patch.image.len() < 4096,
        "single click copied the selected canvas"
    );
    document.cancel();
    assert!(document.committed().is_none());
    assert!(document.preview().is_none());
}

#[test]
fn cropped_preview_and_incremental_commit_match_full_replay_for_all_tools() {
    let source = source();
    let mut document = EditorDocument::new(source);
    document.select((0.125, 0.125, 0.7, 0.7));
    for kind in 1..=14 {
        for (x, y, ex, ey) in [
            (0.2, 0.2, 0.5, 0.6),
            (0.125, 0.125, 0.19, 0.18),
            (0.81, 0.81, 0.6, 0.6),
            (0.3, 0.3, 0.3, 0.3),
        ] {
            let mut mark = Mark::new(kind, x, y, ex, ey, Rgba([255, 30, 70, 255]), 7);
            if matches!(kind, 1 | 7 | 8 | 9 | 12) {
                mark.update_endpoint(x + 0.01, y + 0.01);
                mark.update_endpoint(ex, ey);
            }
            if kind == 10 {
                mark.set_text("Hello\nBabry".to_string());
                mark.set_text_style(mark.color, 24.0, 70.0);
            }
            mark.set_effect_strength(18);
            document.begin(mark.clone());
            let actual = overlay(&document);
            let mut marks = document.marks().to_vec();
            marks.push(mark);
            let expected = editor::render_region(document.source(), document.region(), &marks);
            let mismatch = actual
                .enumerate_pixels()
                .find(|(x, y, p)| expected[(*x, *y)] != **p)
                .map(|(x, y, p)| (x, y, *p, expected[(x, y)]));
            assert!(
                mismatch.is_none(),
                "preview differs for tool {kind} at {x}, {y}: {mismatch:?}"
            );
            if document.commit() {
                assert_eq!(
                    document.committed().unwrap(),
                    &expected,
                    "incremental commit differs for tool {kind}"
                );
            }
        }
    }
    while document.undo() {
        assert_eq!(
            overlay(&document),
            editor::render_region(document.source(), document.region(), document.marks())
        );
    }
    assert!(
        document.committed().is_none(),
        "undo to empty retained the canvas"
    );
    while document.redo() {
        assert_eq!(
            overlay(&document),
            editor::render_region(document.source(), document.region(), document.marks())
        );
    }
}

#[test]
fn fractional_strokes_and_filter_padding_match_at_selection_edges() {
    let mut document = EditorDocument::new(source());
    document.select((0.131, 0.127, 0.673, 0.701));
    let mut seed = 19u32;
    for kind in [1, 2, 3, 4, 5, 6, 7, 8, 9, 11, 12, 13, 14] {
        for index in 0..12 {
            let mut sample = || {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                0.12 + (seed % 7000) as f32 / 10000.0
            };
            let (x, y, ex, ey) = (sample(), sample(), sample(), sample());
            let mut mark = Mark::new(
                kind,
                x,
                y,
                ex,
                ey,
                Rgba([100, 200, 30, 255]),
                1 + index % 20,
            );
            mark.set_effect_strength(1 + index % 20);
            mark.update_endpoint(ex, ey);
            document.begin(mark.clone());
            let actual = overlay(&document);
            let expected = editor::render_region(document.source(), document.region(), &[mark]);
            let mismatch = actual
                .enumerate_pixels()
                .find(|(x, y, p)| expected[(*x, *y)] != **p)
                .map(|(x, y, _)| (x, y));
            assert!(
                mismatch.is_none(),
                "tool {kind}, fractional preview {index}: {mismatch:?}"
            );
            document.cancel();
        }
    }
}

#[test]
fn eraser_restores_source_pixels_and_survives_cancel_undo_redo_and_export() {
    let mut document = EditorDocument::new(source());
    let selection = (0.125, 0.125, 0.75, 0.75);
    document.select(selection);
    let ink = Rgba([255, 0, 0, 255]);
    document.begin(Mark::new(1, 0.2, 0.4, 0.8, 0.4, ink, 20));
    assert!(document.commit());
    let painted = document.committed().unwrap().clone();
    let mut eraser = Mark::new(12, 0.4, 0.35, 0.4, 0.35, ink, 32);
    eraser.update_endpoint(0.4, 0.5);
    eraser.update_endpoint(0.6, 0.5);
    eraser.update_endpoint(0.6, 0.35);
    document.begin(eraser.clone());
    let preview = overlay(&document);
    assert_eq!(document.committed().unwrap(), &painted);
    let region = document.region();
    let local = |x, y| (x - region.x as u32, y - region.y as u32);
    assert_eq!(preview[local(192, 128)], document.source()[(192, 128)]);
    assert_eq!(preview[local(288, 128)], document.source()[(288, 128)]);
    assert_eq!(
        preview[local(144, 128)],
        ink,
        "eraser touched an unrelated stroke pixel"
    );
    document.cancel();
    assert_eq!(overlay(&document), painted);
    document.begin(eraser);
    assert!(document.commit());
    assert_eq!(document.committed().unwrap(), &preview);
    assert!(document.undo());
    assert_eq!(document.committed().unwrap(), &painted);
    assert!(document.redo());
    assert_eq!(document.committed().unwrap(), &preview);
    let output = tempfile::tempdir().unwrap();
    let path = output.path().join("erased.png");
    editor::export(document.source(), &path, selection, document.marks()).unwrap();
    assert_eq!(image::open(path).unwrap().to_rgba8(), preview);
}

#[test]
fn number_counter_follows_committed_history_and_cancel_does_not_consume_it() {
    let mut document = EditorDocument::new(source());
    document.select((0.125, 0.125, 0.75, 0.75));
    for expected in 1..=3 {
        assert_eq!(document.next_number(), expected);
        let mut mark = Mark::new(
            11,
            0.2 + expected as f32 * 0.1,
            0.4,
            0.2,
            0.4,
            Rgba([240, 30, 30, 255]),
            4,
        );
        mark.number = document.next_number();
        document.begin(mark);
        assert!(document.commit());
    }
    let committed = document.committed().unwrap().clone();
    assert!(document.undo());
    assert_eq!(document.next_number(), 3);
    assert!(document.redo());
    assert_eq!(document.next_number(), 4);
    assert_eq!(document.committed().unwrap(), &committed);
    document.begin(Mark::new(
        11,
        0.7,
        0.7,
        0.7,
        0.7,
        Rgba([240, 30, 30, 255]),
        4,
    ));
    document.cancel();
    assert_eq!(document.next_number(), 4);
}

#[test]
fn replacing_or_cancelling_preview_leaves_committed_pixels_untouched() {
    let mut document = EditorDocument::new(source());
    document.select((0.125, 0.125, 0.7, 0.7));
    document.begin(Mark::new(2, 0.2, 0.2, 0.7, 0.65, Rgba([0, 255, 0, 255]), 3));
    assert!(document.commit());
    let committed = document.committed().unwrap().clone();
    for (x, y) in [(0.7, 0.6), (0.4, 0.4), (0.3, 0.25)] {
        document.begin(Mark::new(5, 0.2, 0.2, x, y, Rgba([255, 0, 0, 255]), 10));
        let _ = document.preview().unwrap();
        assert_eq!(document.committed().unwrap(), &committed);
    }
    document.cancel();
    assert_eq!(overlay(&document), committed);
}

#[test]
fn in_memory_export_matches_selection_canvas_after_undo_and_redo() {
    let mut document = EditorDocument::new(source());
    let selection = (0.131, 0.127, 0.7, 0.7);
    document.select(selection);
    for kind in [2, 7, 9, 8] {
        document.begin(Mark::new(
            kind,
            0.15,
            0.15,
            0.65,
            0.6,
            Rgba([255, 0, 0, 255]),
            8,
        ));
        document.commit();
    }
    document.undo();
    document.redo();
    let directory = tempfile::tempdir().unwrap();
    let output = directory.path().join("export.png");
    editor::export(document.source(), &output, selection, document.marks()).unwrap();
    assert_eq!(
        image::open(&output).unwrap().to_rgba8(),
        *document.committed().unwrap()
    );
    document.export(&output).unwrap();
    assert_eq!(
        image::open(&output).unwrap().to_rgba8(),
        *document.committed().unwrap()
    );
    document.begin(Mark::new(7, 0.2, 0.2, 0.7, 0.7, Rgba([0, 255, 0, 255]), 10));
    document.export(&output).unwrap();
    assert_eq!(
        image::open(output).unwrap().to_rgba8(),
        *document.committed().unwrap(),
        "export included a pending operation"
    );
}
