//! 选区导出：按条带合成截图与标注并流式编码 PNG。
//!
//! 峰值内存只与选区宽度乘以条带高度有关，不再为导出复制整张选区图像。
use crate::{
    core::RectPx,
    font::Fonts,
    marks::{self, Mark},
    shm_image::ShmImage,
};
use std::{
    fs::File,
    io::{BufWriter, Write},
    path::Path,
};
use tiny_skia::{Pixmap, PixmapPaint, Transform};

const BAND_ROWS: u32 = 64;

/// 把选区编码为 RGBA PNG 写入任意输出流。
pub fn encode_png<W: Write>(
    output: W,
    source: &ShmImage,
    selection: RectPx,
    marks: &[Mark],
    fonts: &mut Fonts,
) -> Result<(), String> {
    let image = RectPx::new(0, 0, source.width(), source.height());
    let rect = marks::intersect(selection, image).ok_or("选区为空，无法导出")?;
    let mut encoder = png::Encoder::new(output, rect.width, rect.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut header = encoder
        .write_header()
        .map_err(|error| format!("写入 PNG 头部失败: {error}"))?;
    let mut writer = header
        .stream_writer()
        .map_err(|error| format!("创建 PNG 流失败: {error}"))?;
    let row = rect.width as usize * 4;
    let mut band_y = 0;
    while band_y < rect.height {
        let rows = BAND_ROWS.min(rect.height - band_y);
        let mut base = Pixmap::new(rect.width, rows).ok_or("导出条带尺寸无效")?;
        let top = rect.y + band_y as i32;
        for (y, line) in base.data_mut().chunks_exact_mut(row).enumerate() {
            source.read_row(rect.x, top + y as i32, line);
        }
        let band = RectPx::new(rect.x, top, rect.width, rows);
        if marks.iter().any(|mark| {
            mark.bounds()
                .and_then(|b| marks::intersect(b, band))
                .is_some()
        }) {
            let mut layer = Pixmap::new(rect.width, rows).ok_or("导出图层尺寸无效")?;
            marks::render_layer(&mut layer.as_mut(), (rect.x, top), marks, source, fonts);
            base.draw_pixmap(
                0,
                0,
                layer.as_ref(),
                &PixmapPaint::default(),
                Transform::identity(),
                None,
            );
        }
        // 截图不透明，叠加后仍为 alpha=255，预乘值即直通值。
        writer
            .write_all(base.data())
            .map_err(|error| format!("写入 PNG 像素失败: {error}"))?;
        band_y += rows;
    }
    writer
        .finish()
        .map_err(|error| format!("完成 PNG 写入失败: {error}"))?;
    header
        .finish()
        .map_err(|error| format!("完成 PNG 写入失败: {error}"))
}

/// 写入 PNG 文件；失败时删除不完整的文件，避免留下损坏图片。
pub fn save_png(
    path: &Path,
    source: &ShmImage,
    selection: RectPx,
    marks: &[Mark],
    fonts: &mut Fonts,
) -> Result<(), String> {
    let file = File::create(path).map_err(|error| format!("无法创建 PNG: {error}"))?;
    let result = encode_png(BufWriter::new(file), source, selection, marks, fonts);
    if result.is_err() {
        let _ = std::fs::remove_file(path);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pixels::{PixelSize, RgbaPixels};

    #[test]
    fn streams_marked_selection_across_bands() {
        let size = PixelSize::new(40, 150).unwrap();
        let pixels = RgbaPixels::from_rgba(size, vec![200; 40 * 150 * 4]).unwrap();
        let source = ShmImage::from_rgba(&pixels).unwrap();
        let mut pen = Mark::new(marks::PEN, 20.0, 5.0, [255, 0, 0, 255], 4);
        pen.update(20.0, 145.0);
        let mut bytes = Vec::new();
        encode_png(
            &mut bytes,
            &source,
            RectPx::new(10, 0, 20, 150),
            &[pen],
            &mut Fonts::new(),
        )
        .unwrap();
        let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
        let mut reader = decoder.read_info().unwrap();
        let mut out = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut out).unwrap();
        assert_eq!((info.width, info.height), (20, 150));
        // 跨过 64 行条带边界的笔迹连续存在。
        for y in [10usize, 63, 64, 128, 140] {
            let i = (y * 20 + 10) * 4;
            assert_eq!(&out[i..i + 4], &[255, 0, 0, 255], "row {y}");
        }
        assert_eq!(&out[0..4], &[200, 200, 200, 255]);
    }
}
