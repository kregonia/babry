//! 字体按需映射；只读取字形轮廓，不复制整个字体到堆中。
use memmap2::Mmap;
use std::{fs::File, path::PathBuf, process::Command};
use tiny_skia::{PathBuilder, Transform};

pub struct Fonts {
    faces: Vec<(Mmap, u32)>,
}
impl Default for Fonts {
    fn default() -> Self {
        Self::new()
    }
}
impl Fonts {
    pub fn new() -> Self {
        Self { faces: Vec::new() }
    }
    fn load(&mut self, pattern: &str) {
        let explicit = std::env::var_os("BABRY_FONT").map(PathBuf::from);
        let path = if let Some(path) = explicit {
            Some(path)
        } else {
            Command::new("fc-match")
                .args(["-f", "%{file}", pattern])
                .output()
                .ok()
                .filter(|r| r.status.success())
                .map(|r| PathBuf::from(String::from_utf8_lossy(&r.stdout).trim()))
        };
        if let Some(path) = path
            && let Ok(file) = File::open(path)
        {
            // SAFETY: 字体只读映射；运行期间不修改字体文件。
            if let Ok(map) = unsafe { Mmap::map(&file) } {
                let index = std::env::var("BABRY_FONT_INDEX")
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                if ttf_parser::Face::parse(&map, index).is_ok() {
                    self.faces.push((map, index));
                }
            }
        }
    }
    /// 绘制文字，返回排版宽高。
    pub fn text(
        &mut self,
        text: &str,
        x: f32,
        y: f32,
        size: f32,
        wrap: f32,
        mut draw: impl FnMut(&tiny_skia::Path),
    ) -> (f32, f32) {
        let layout = self.layout(text, x, y, size, wrap, Some(&mut draw));
        (layout.0, layout.1)
    }

    /// 只测量文字：返回 (宽, 高, 光标 x, 光标 y)，坐标相对文字起点。
    pub fn measure(&mut self, text: &str, size: f32, wrap: f32) -> (f32, f32, f32, f32) {
        self.layout(text, 0.0, 0.0, size, wrap, None)
    }

    fn face_for(&mut self, ch: char) -> Option<usize> {
        if self.faces.is_empty() {
            self.load("sans-serif");
        }
        let has = |faces: &[(Mmap, u32)]| {
            faces.iter().position(|(m, i)| {
                ttf_parser::Face::parse(m, *i)
                    .ok()
                    .is_some_and(|f| f.glyph_index(ch).is_some())
            })
        };
        let mut index = has(&self.faces);
        if index.is_none() && self.faces.len() < 4 && !ch.is_whitespace() {
            self.load(&format!("sans-serif:charset={:x}", ch as u32));
            index = has(&self.faces);
        }
        index
    }

    fn layout(
        &mut self,
        text: &str,
        x: f32,
        y: f32,
        size: f32,
        wrap: f32,
        mut draw: Option<&mut dyn FnMut(&tiny_skia::Path)>,
    ) -> (f32, f32, f32, f32) {
        let line_height = size * 1.25;
        let (mut cx, mut cy, mut max) = (x, y, x);
        for ch in text.chars() {
            if ch == '\n' {
                max = max.max(cx);
                cx = x;
                cy += line_height;
                continue;
            }
            let Some(index) = self.face_for(ch) else {
                cx += size * 0.6;
                continue;
            };
            let (map, index) = &self.faces[index];
            let Ok(face) = ttf_parser::Face::parse(map, *index) else {
                continue;
            };
            let Some(id) = face.glyph_index(ch) else {
                continue;
            };
            let scale = size / f32::from(face.units_per_em());
            let advance = f32::from(face.glyph_hor_advance(id).unwrap_or(0)) * scale;
            if cx > x && cx + advance > x + wrap {
                max = max.max(cx);
                cx = x;
                cy += line_height;
            }
            if let Some(draw) = draw.as_mut() {
                let mut outline = Outline(PathBuilder::new());
                if face.outline_glyph(id, &mut outline).is_some()
                    && let Some(path) = outline.0.finish()
                {
                    let baseline = cy + f32::from(face.ascender()) * scale;
                    if let Some(path) =
                        path.transform(Transform::from_row(scale, 0.0, 0.0, -scale, cx, baseline))
                    {
                        draw(&path);
                    }
                }
            }
            cx += advance;
        }
        (max.max(cx) - x, cy - y + line_height, cx - x, cy - y)
    }
}
struct Outline(PathBuilder);
impl ttf_parser::OutlineBuilder for Outline {
    fn move_to(&mut self, x: f32, y: f32) {
        self.0.move_to(x, y)
    }
    fn line_to(&mut self, x: f32, y: f32) {
        self.0.line_to(x, y)
    }
    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        self.0.quad_to(x1, y1, x, y)
    }
    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        self.0.cubic_to(x1, y1, x2, y2, x, y)
    }
    fn close(&mut self) {
        self.0.close()
    }
}
