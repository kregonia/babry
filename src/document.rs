use crate::{
    core::{History, RectPx},
    editor::{self, Mark},
};
use image::{RgbaImage, imageops};
use std::sync::Arc;

/// 编辑状态与界面解耦；选区确认或取消空操作时不分配可变画布。
pub struct EditorDocument {
    source: Arc<RgbaImage>,
    region: RectPx,
    history: History<Mark>,
    committed: Option<RgbaImage>,
    pending: Option<Mark>,
}

pub struct PreviewPatch {
    pub region: RectPx,
    pub image: RgbaImage,
}

impl EditorDocument {
    pub fn new(source: Arc<RgbaImage>) -> Self {
        let region = RectPx::new(0, 0, source.width(), source.height());
        Self {
            source,
            region,
            history: History::default(),
            committed: None,
            pending: None,
        }
    }

    pub fn select(&mut self, selection: (f32, f32, f32, f32)) {
        let region = editor::selection_rect(&self.source, selection);
        if region != self.region {
            self.region = region;
            self.pending = None;
            self.rebuild();
        }
    }

    pub fn region(&self) -> RectPx {
        self.region
    }
    pub fn source(&self) -> &RgbaImage {
        &self.source
    }
    pub fn marks(&self) -> &[Mark] {
        self.history.entries()
    }
    pub fn next_number(&self) -> u32 {
        self.marks()
            .iter()
            .filter(|mark| mark.kind == 11)
            .map(|mark| mark.number)
            .max()
            .unwrap_or(0)
            .saturating_add(1)
    }
    pub fn committed(&self) -> Option<&RgbaImage> {
        self.committed.as_ref()
    }
    pub fn pending_mut(&mut self) -> Option<&mut Mark> {
        self.pending.as_mut()
    }
    pub fn begin(&mut self, mark: Mark) {
        self.pending = Some(mark);
    }
    pub fn cancel(&mut self) {
        self.pending = None;
    }

    pub fn commit(&mut self) -> bool {
        let Some(mark) = self.pending.take() else {
            return false;
        };
        let distance = (mark.x2 - mark.x1).hypot(mark.y2 - mark.y1);
        let valid = match mark.kind {
            10 => !mark.text.trim().is_empty(),
            1 | 7 | 8 | 9 | 11 | 12 => true,
            _ => distance > 0.003,
        };
        if !valid {
            return false;
        }
        self.ensure_committed();
        editor::render_mark_in_region(
            self.committed.as_mut().unwrap(),
            &mark,
            &self.source,
            (self.region.x, self.region.y),
        );
        self.history.push(mark);
        true
    }

    pub fn undo(&mut self) -> bool {
        self.pending = None;
        if !self.history.undo() {
            return false;
        }
        self.rebuild();
        true
    }

    pub fn redo(&mut self) -> bool {
        self.pending = None;
        if !self.history.redo() {
            return false;
        }
        let mark = self.history.entries().last().expect("redo pushed an entry");
        if let Some(canvas) = &mut self.committed {
            editor::render_mark_in_region(
                canvas,
                mark,
                &self.source,
                (self.region.x, self.region.y),
            );
        } else {
            self.ensure_committed();
        }
        true
    }

    /// 待提交标注只生成局部预览。遮罩/滤镜与导出共用渲染函数和坐标。
    pub fn preview(&self) -> Option<PreviewPatch> {
        let mark = self.pending.as_ref()?;
        let region = editor::mark_bounds(mark, self.source.dimensions(), self.region)?;
        let (base, x, y) = if let Some(committed) = &self.committed {
            (
                committed,
                (region.x - self.region.x) as u32,
                (region.y - self.region.y) as u32,
            )
        } else {
            (&*self.source, region.x as u32, region.y as u32)
        };
        let mut image = imageops::crop_imm(base, x, y, region.width, region.height).to_image();
        editor::render_mark_in_region(&mut image, mark, &self.source, (region.x, region.y));
        Some(PreviewPatch { region, image })
    }

    /// 已提交画布可直接编码；没有标注时只裁剪原图，不重新解码或重放。
    pub fn export(&self, output: &std::path::Path) -> Result<(), String> {
        if self.source.width() == 0 || self.source.height() == 0 {
            return Err("截图尺寸为空".into());
        }
        editor::validate_fonts(self.marks())?;
        if let Some(canvas) = &self.committed {
            canvas.save(output).map_err(|error| error.to_string())
        } else {
            editor::render_region(&self.source, self.region, self.marks())
                .save(output)
                .map_err(|error| error.to_string())
        }
    }

    fn ensure_committed(&mut self) {
        if self.committed.is_none() {
            self.committed = Some(editor::render_region(
                &self.source,
                self.region,
                self.history.entries(),
            ));
        }
    }

    fn rebuild(&mut self) {
        // 撤销到空历史直接显示底图，释放选区画布。
        self.committed = if self.history.is_empty() {
            None
        } else {
            Some(editor::render_region(
                &self.source,
                self.region,
                self.history.entries(),
            ))
        };
    }
}
