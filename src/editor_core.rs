//! 编辑器核心：选区、标注、撤销栈、工具栏状态与输入处理，与显示后端无关。
//!
//! Wayland（SCTK 子表面场景）、长图画布和 X11 前端都只负责把窗口事件翻译成这里
//! 的调用，并按 `damaged`/`dirty` 重绘。所有坐标：标注与选区使用图像物理像素，
//! 指针使用输出逻辑坐标；`view` 是屏幕物理原点对应的图像坐标（长图滚动时变化）。
use crate::{
    clipboard, config,
    core::RectPx,
    export,
    font::Fonts,
    marks::{self, Mark},
    native_ui::{self, Action, BoxF},
    shm_image::ShmImage,
    smart_select,
};
use smithay_client_toolkit::seat::keyboard::Keysym;
use std::{
    path::PathBuf,
    process::{Command, Stdio},
    sync::{
        Arc,
        mpsc::{Receiver, TryRecvError},
    },
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LongshotRequest {
    /// 以该选区（图像物理像素）开始。
    Start(RectPx),
    Stop,
}

/// 前端无关的指针形状。`Resize` 的参数与 `selection_edge` 的边号一致。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cursor {
    Default,
    Crosshair,
    Move,
    Grab,
    Grabbing,
    Text,
    Resize(u8),
}

/// 撤销栈中的一步编辑；标注本身保存在 `marks` 中，移动只记录位移。
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Edit {
    Add,
    Move { index: usize, dx: f32, dy: f32 },
}

/// 重做栈：撤销“新增”时保留被移除的标注，撤销“移动”只需位移。
#[derive(Clone, Debug)]
pub enum Redo {
    Add(Mark),
    Move { index: usize, dx: f32, dy: f32 },
}

#[derive(Clone, Copy, Debug)]
pub enum PointerDrag {
    Select {
        start: (f64, f64),
    },
    Move {
        start: (f64, f64),
        origin: RectPx,
    },
    Resize {
        start: (f64, f64),
        origin: RectPx,
        edge: u8,
    },
    Mark,
    /// 无工具时拖动已有标注；`moved` 为累计位移（物理像素）。
    MoveMark {
        index: usize,
        last: (f32, f32),
        moved: (f32, f32),
    },
    Toolbar {
        offset: (f64, f64),
    },
    Slider {
        strength: bool,
    },
}

pub struct Core {
    pub source: Arc<ShmImage>,
    /// 屏幕物理尺寸（截图模式下等于图像尺寸）。
    pub physical: (u32, u32),
    pub logical: (u32, u32),
    /// 屏幕物理 (0, 0) 对应的图像坐标；只有长图画布会滚动。
    pub view: (i32, i32),
    /// 长图画布：选区固定为整张图，滚轮滚动视图。
    pub canvas: bool,
    /// 长图画布在屏幕上的窗口（屏幕物理像素），即开始长截图时的选区。
    pub window: RectPx,
    /// 长图画布窗口外显示的原截图。
    pub backdrop: Option<Arc<ShmImage>>,
    pub selection: RectPx,
    pub candidates: Vec<smart_select::Candidate>,
    pub candidates_pending: Option<Receiver<Vec<smart_select::Candidate>>>,
    /// 用户尚未悬停或拖动时，候选到达后可用吸附后的窗口种子替换初始选区。
    pub hovered: bool,
    pub hover: smart_select::Cursor,
    pub ui: native_ui::EditorUi,
    pub fonts: Fonts,
    pub marks: Vec<Mark>,
    pub pending: Option<Mark>,
    pub edits: Vec<Edit>,
    pub redo: Vec<Redo>,
    pub text_editing: bool,
    pub preedit: String,
    pub pointer_drag: Option<PointerDrag>,
    pub ctrl: bool,
    pub shift: bool,
    pub save_dir: PathBuf,
    pub output_name: Option<String>,
    pub exit: bool,
    /// 工具栏长截图按钮发出的请求，由前端处理。
    pub longshot: Option<LongshotRequest>,
    /// 正在滚动长截图：只显示边框和工具栏，选区内的输入交给下方窗口。
    pub capturing: bool,
    /// 长截图期间显示在选区上方的进度文字。
    pub capture_status: String,
    pub dirty: bool,
    /// 需要重绘的图像区域（标注变化），由前端取走。
    pub damaged: Vec<RectPx>,
}

/// 构造核心状态所需的会话参数。
pub struct CoreInit {
    pub source: Arc<ShmImage>,
    pub physical: (u32, u32),
    pub logical: (u32, u32),
    pub selection: RectPx,
    pub direct_edit: bool,
    pub canvas: bool,
    pub save_dir: PathBuf,
    pub output: Option<String>,
    pub candidates: Option<Receiver<Vec<smart_select::Candidate>>>,
}

impl Core {
    pub fn new(init: CoreInit) -> Self {
        let mut core = Self {
            physical: init.physical,
            logical: init.logical,
            view: (0, 0),
            canvas: init.canvas,
            window: RectPx::new(0, 0, init.physical.0, init.physical.1),
            backdrop: None,
            selection: init.selection,
            candidates: Vec::new(),
            candidates_pending: init.candidates,
            hovered: false,
            hover: smart_select::Cursor::default(),
            ui: native_ui::EditorUi {
                editing: init.direct_edit || init.canvas,
                ..native_ui::EditorUi::default()
            },
            fonts: Fonts::new(),
            marks: Vec::new(),
            pending: None,
            edits: Vec::new(),
            redo: Vec::new(),
            text_editing: false,
            preedit: String::new(),
            pointer_drag: None,
            ctrl: false,
            shift: false,
            save_dir: init.save_dir,
            output_name: init.output,
            exit: false,
            longshot: None,
            capturing: false,
            capture_status: String::new(),
            dirty: true,
            damaged: Vec::new(),
            source: init.source,
        };
        if core.canvas {
            // 窄图水平居中；矮图垂直居中，高图从顶部开始。
            let (iw, ih) = (core.source.width() as i32, core.source.height() as i32);
            let (sw, sh) = (core.physical.0 as i32, core.physical.1 as i32);
            core.view = (
                if iw <= sw { -(sw - iw) / 2 } else { 0 },
                if ih <= sh { -(sh - ih) / 2 } else { 0 },
            );
        }
        core
    }

    /// 长截图结束：拼接结果放进原选区的位置和大小里，作为可滚动的页面继续编辑；
    /// 选区外仍显示原截图的遮罩，与普通截图编辑页一致。已有标注按选区原点平移。
    pub fn enter_canvas(&mut self, image: ShmImage) {
        self.commit_pending();
        let window = self.selection;
        for mark in &mut self.marks {
            mark.translate(-window.x as f32, -window.y as f32);
        }
        self.backdrop = Some(std::mem::replace(&mut self.source, Arc::new(image)));
        self.canvas = true;
        self.capturing = false;
        self.ui.longshot = false;
        self.capture_status.clear();
        self.selection = RectPx::new(0, 0, self.source.width(), self.source.height());
        // 页面宽度可能因逻辑坐标取整比原选区少一两个像素，窗口取二者较小值。
        self.window = RectPx::new(
            window.x,
            window.y,
            window.width.min(self.source.width()),
            window.height.min(self.source.height()),
        );
        self.view = (-self.window.x, -self.window.y);
        self.ui.editing = true;
        self.damage(Some(self.selection));
    }

    /// 指针进入表面。
    pub fn pointer_enter(&mut self, position: (f64, f64)) {
        self.ui.pointer = position;
        if !self.ui.editing {
            self.hover_selection(position);
        }
        self.dirty = true;
    }

    pub fn pointer_leave(&mut self) {
        self.ui.pointer = (-1.0, -1.0);
        self.dirty = true;
    }

    pub fn pointer_motion(&mut self, position: (f64, f64)) {
        self.ui.pointer = position;
        if self.pointer_drag.is_some() {
            self.update_pointer_drag(position);
        } else if !self.ui.editing {
            self.hover_selection(position);
        } else {
            // 悬停只影响工具栏/菜单，状态键不变时场景不会重绘。
            self.dirty = true;
        }
    }

    /// 右键：框选阶段退出；编辑时依次提交文字、收起菜单、取消工具。
    pub fn right_press(&mut self) {
        if self.capturing {
            return;
        }
        if !self.ui.editing {
            self.exit = true;
        } else if self.text_editing {
            self.commit_pending();
        } else if self.ui.menu.is_some() {
            self.ui.menu = None;
        } else {
            self.ui.tool = 0;
        }
        self.dirty = true;
    }

    /// 滚轮：`steps` 为正表示向下。框选阶段在候选父子层级间切换；长图画布滚动视图。
    pub fn axis(&mut self, position: (f64, f64), steps: i32, logical_dy: f64) {
        if self.canvas {
            if self.scroll_view(logical_dy) {
                // 视图移动后指针下的图像坐标变了，拖动中的标注跟着更新。
                if self.pointer_drag.is_some() {
                    self.update_pointer_drag(position);
                }
            }
            return;
        }
        if self.ui.editing {
            return;
        }
        self.poll_candidates();
        if steps != 0 {
            let point = self.to_physical(position);
            // 向上滚动扩大到父区域，向下缩小到子区域。
            let next = self.hover.step(
                &self.candidates,
                point,
                self.selection,
                self.bounds(),
                -steps,
            );
            if next != self.selection {
                self.selection = next;
                self.dirty = true;
            }
        }
    }

    pub fn scale(&self) -> f32 {
        self.physical.0 as f32 / self.logical.0.max(1) as f32
    }

    /// 整张图像的范围（图像物理像素）。
    pub fn bounds(&self) -> RectPx {
        RectPx::new(0, 0, self.source.width(), self.source.height())
    }

    /// 输出逻辑坐标 → 图像物理坐标。
    pub fn to_physical(&self, logical: (f64, f64)) -> (f32, f32) {
        (
            (logical.0 * self.physical.0 as f64 / self.logical.0.max(1) as f64) as f32
                + self.view.0 as f32,
            (logical.1 * self.physical.1 as f64 / self.logical.1.max(1) as f64) as f32
                + self.view.1 as f32,
        )
    }

    /// 选区在屏幕上的逻辑位置。长图画布中裁剪到屏幕内，工具栏因此始终可见。
    pub fn ui_selection(&self) -> BoxF {
        let mut rect = RectPx::new(
            self.selection.x - self.view.0,
            self.selection.y - self.view.1,
            self.selection.width,
            self.selection.height,
        );
        if self.canvas {
            rect = self.window;
        }
        native_ui::logical_selection(rect, self.physical, self.logical)
    }

    /// 记录需要重绘的图像区域；前端在下一帧取走。
    pub fn damage(&mut self, rect: Option<RectPx>) {
        if let Some(rect) = rect {
            self.damaged.push(rect);
        }
        self.dirty = true;
    }

    /// 长图画布在窗口内滚动页面；返回是否真的移动了。
    pub fn scroll_view(&mut self, logical_dy: f64) -> bool {
        if !self.canvas {
            return false;
        }
        let scale = f64::from(self.scale());
        let limit = (self.source.height() as i32 - self.window.height as i32).max(0);
        let scroll =
            (self.view.1 + self.window.y + (logical_dy * scale).round() as i32).clamp(0, limit);
        let next = scroll - self.window.y;
        if next == self.view.1 {
            return false;
        }
        self.view.1 = next;
        self.dirty = true;
        true
    }

    /// 当前可见的页面范围（图像坐标）。
    pub fn visible(&self) -> RectPx {
        if !self.canvas {
            return self.selection;
        }
        RectPx::new(
            self.window.x + self.view.0,
            self.window.y + self.view.1,
            self.window.width,
            self.window.height,
        )
    }

    pub fn caret_rect(&mut self) -> Option<RectPx> {
        if !self.text_editing {
            return None;
        }
        let mark = self.pending.as_ref()?;
        let (_, _, x, y) = self
            .fonts
            .measure(&mark.text, mark.font_size, mark.text_width);
        Some(RectPx::new(
            (mark.x1 + x).round() as i32,
            (mark.y1 + y).round() as i32,
            2,
            (mark.font_size * 1.25).ceil() as u32,
        ))
    }

    pub fn update_next_number(&mut self) {
        self.ui.next_number = self
            .marks
            .iter()
            .filter(|m| m.kind == marks::NUMBER)
            .count() as u32
            + 1;
    }

    /// 提交正在编辑的文字或尚未松开的标注；空文字和零尺寸图形直接丢弃。
    pub fn commit_pending(&mut self) {
        self.text_editing = false;
        self.preedit.clear();
        let Some(mark) = self.pending.take() else {
            return;
        };
        let empty = match mark.kind {
            marks::TEXT => mark.text.trim().is_empty(),
            marks::RECTANGLE | marks::SQUARE | marks::CIRCLE | marks::LINE | marks::ARROW => {
                (mark.x2 - mark.x1).abs() < 2.0 && (mark.y2 - mark.y1).abs() < 2.0
            }
            _ => false,
        };
        if empty {
            self.damage(mark.bounds());
            return;
        }
        self.marks.push(mark);
        self.edits.push(Edit::Add);
        self.redo.clear();
        self.update_next_number();
        self.dirty = true;
    }

    /// 平移一条已提交的标注并重绘新旧范围。
    pub fn shift_mark(&mut self, index: usize, dx: f32, dy: f32) {
        let Some(mark) = self.marks.get_mut(index) else {
            return;
        };
        let before = mark.bounds();
        mark.translate(dx, dy);
        let after = mark.bounds();
        // 马赛克/模糊读取下方标注，移动后其上方的效果也要重新计算。
        let damage = match (before, after) {
            (Some(a), Some(b)) => Some(marks::union(Some(a), b)),
            (a, b) => a.or(b),
        };
        let effects: Vec<RectPx> = self.marks[index + 1..]
            .iter()
            .filter(|m| matches!(m.kind, marks::MOSAIC | marks::BLUR))
            .filter_map(Mark::bounds)
            .filter(|b| damage.and_then(|d| marks::intersect(*b, d)).is_some())
            .collect();
        self.damage(damage);
        for rect in effects {
            self.damage(Some(rect));
        }
    }

    /// 最上层被 `position`（逻辑坐标）命中的可移动标注。
    pub fn mark_at(&self, position: (f64, f64)) -> Option<usize> {
        let (x, y) = self.to_physical(position);
        let tolerance = 6.0 * self.scale();
        self.marks
            .iter()
            .rposition(|mark| mark.hit(x, y, tolerance))
    }

    pub fn undo(&mut self) {
        self.commit_pending();
        match self.edits.pop() {
            Some(Edit::Add) => {
                if let Some(mark) = self.marks.pop() {
                    self.damage(mark.bounds());
                    self.redo.push(Redo::Add(mark));
                }
            }
            Some(Edit::Move { index, dx, dy }) => {
                self.shift_mark(index, -dx, -dy);
                self.redo.push(Redo::Move { index, dx, dy });
            }
            None => {}
        }
        self.update_next_number();
    }

    pub fn redo(&mut self) {
        self.commit_pending();
        match self.redo.pop() {
            Some(Redo::Add(mark)) => {
                self.damage(mark.bounds());
                self.marks.push(mark);
                self.edits.push(Edit::Add);
            }
            Some(Redo::Move { index, dx, dy }) => {
                self.shift_mark(index, dx, dy);
                self.edits.push(Edit::Move { index, dx, dy });
            }
            None => {}
        }
        self.update_next_number();
    }

    pub fn begin_mark(&mut self, position: (f64, f64)) {
        let (x, y) = self.to_physical(position);
        let scale = self.scale();
        let kind = match self.ui.tool {
            8 if self.ui.effect == 1 => marks::BLUR,
            tool => tool,
        };
        if kind == marks::TEXT {
            self.begin_text(x, y);
            return;
        }
        let width = ((self.ui.width as f32 * scale).round() as u32).max(1);
        let mut mark = Mark::new(kind, x, y, self.ui.color, width);
        mark.strength = self.ui.strength;
        if kind == marks::NUMBER {
            mark.number = self.ui.next_number;
            mark.font_size = self.ui.font_size as f32 * scale;
        }
        let bounds = mark.bounds();
        self.pending = Some(mark);
        self.pointer_drag = Some(PointerDrag::Mark);
        self.damage(bounds);
    }

    pub fn update_mark(&mut self, position: (f64, f64)) {
        let (x, y) = self.to_physical(position);
        // 长图画布只能画在当前可见的那一段里。
        let s = self.visible();
        let x = x.clamp(s.x as f32, (s.right() - 1) as f32);
        let y = y.clamp(s.y as f32, (s.bottom() - 1) as f32);
        let shift = self.shift;
        let Some(mark) = self.pending.as_mut() else {
            return;
        };
        let before = mark.bounds();
        let last = mark.points.last().copied();
        if mark.kind == marks::NUMBER {
            mark.x1 = x;
            mark.y1 = y;
        } else if matches!(mark.kind, marks::LINE | marks::ARROW) && shift {
            // 按住 Shift 时直线和箭头吸附到 45° 的整数倍。
            let (dx, dy) = (x - mark.x1, y - mark.y1);
            let angle =
                (dy.atan2(dx) / std::f32::consts::FRAC_PI_4).round() * std::f32::consts::FRAC_PI_4;
            let length = dx.hypot(dy);
            mark.update(
                mark.x1 + angle.cos() * length,
                mark.y1 + angle.sin() * length,
            );
        } else {
            mark.update(x, y);
        }
        // 画笔类只重绘新线段经过的瓦片；图形需要同时清除旧轮廓。
        let damage = if marks::is_brush(mark.kind) {
            let (lx, ly) = last.unwrap_or((x, y));
            let pad = (mark.width.saturating_mul(3).min(192) / 2 + 4) as f32;
            let left = (lx.min(x) - pad).floor() as i32;
            let top = (ly.min(y) - pad).floor() as i32;
            let right = (lx.max(x) + pad).ceil() as i32;
            let bottom = (ly.max(y) + pad).ceil() as i32;
            Some(RectPx::new(
                left,
                top,
                (right - left) as u32,
                (bottom - top) as u32,
            ))
        } else {
            match (before, mark.bounds()) {
                (Some(a), Some(b)) => Some(marks::union(Some(a), b)),
                (a, b) => a.or(b),
            }
        };
        self.damage(damage);
    }

    pub fn begin_text(&mut self, x: f32, y: f32) {
        self.commit_pending();
        let scale = self.scale();
        let mut mark = Mark::new(marks::TEXT, x, y, self.ui.color, 1);
        mark.font_size = self.ui.font_size as f32 * scale;
        mark.text_width = (self.selection.right() as f32 - x).max(mark.font_size);
        self.pending = Some(mark);
        self.text_editing = true;
        self.refresh_text(|_| {});
    }

    /// 修改正在编辑的文字并重新测量，重绘新旧排版范围。
    pub fn refresh_text(&mut self, edit: impl FnOnce(&mut Mark)) {
        if !self.text_editing {
            return;
        }
        let Some(mark) = self.pending.as_mut() else {
            return;
        };
        let before = mark.bounds();
        edit(mark);
        let shown = format!("{}{}", mark.text, self.preedit);
        let (w, h, _, _) = self.fonts.measure(&shown, mark.font_size, mark.text_width);
        mark.extent = (w, h);
        let after = mark.bounds();
        let damage = match (before, after) {
            (Some(a), Some(b)) => Some(marks::union(Some(a), b)),
            (a, b) => a.or(b),
        };
        self.damage(damage);
    }

    pub fn export_to(&mut self, path: &std::path::Path) -> Result<(), String> {
        self.commit_pending();
        export::save_png(
            path,
            &self.source,
            self.selection,
            &self.marks,
            &mut self.fonts,
        )
    }

    pub fn save(&mut self) -> Result<(), String> {
        let path = config::output_path(&self.save_dir)?;
        self.export_to(&path)?;
        eprintln!("已保存: {}", path.display());
        self.exit = true;
        Ok(())
    }

    pub fn copy(&mut self) -> Result<(), String> {
        let path = config::output_path(&self.save_dir)?;
        self.export_to(&path)?;
        clipboard::copy_png(&path)?;
        self.exit = true;
        Ok(())
    }

    /// 贴图在独立进程中显示，编辑器退出后释放整张截图。
    pub fn pin(&mut self) -> Result<(), String> {
        let path = config::output_path(&self.save_dir)?;
        self.export_to(&path)?;
        let exe = std::env::current_exe().map_err(|error| error.to_string())?;
        let mut command = Command::new(exe);
        if let Some(output) = &self.output_name {
            command.args(["--output", output]);
        }
        command
            .arg("--pin")
            .arg(&path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|error| format!("无法启动贴图: {error}"))?;
        self.exit = true;
        Ok(())
    }

    pub fn report(&mut self, result: Result<(), String>) {
        if let Err(error) = result {
            self.ui.error = Some(error);
            self.dirty = true;
        }
    }

    pub fn update_pointer_drag(&mut self, position: (f64, f64)) {
        let Some(drag) = self.pointer_drag else {
            return;
        };
        let physical = self.to_physical(position);
        let (w, h) = (self.source.width() as i32, self.source.height() as i32);
        match drag {
            PointerDrag::Select { start } => {
                if (start.0 - position.0)
                    .abs()
                    .max((start.1 - position.1).abs())
                    < 3.0
                {
                    return;
                }
                let a = self.to_physical(start);
                let left = a.0.min(physical.0).round().clamp(0.0, w as f32) as i32;
                let top = a.1.min(physical.1).round().clamp(0.0, h as f32) as i32;
                let right = a.0.max(physical.0).round().clamp(left as f32, w as f32) as i32;
                let bottom = a.1.max(physical.1).round().clamp(top as f32, h as f32) as i32;
                self.selection =
                    RectPx::new(left, top, (right - left) as u32, (bottom - top) as u32);
            }
            PointerDrag::Move { start, origin } => {
                let start = self.to_physical(start);
                let x = (origin.x as f32 + physical.0 - start.0).round() as i32;
                let y = (origin.y as f32 + physical.1 - start.1).round() as i32;
                self.selection = RectPx::new(
                    x.clamp(0, (w - origin.width as i32).max(0)),
                    y.clamp(0, (h - origin.height as i32).max(0)),
                    origin.width,
                    origin.height,
                );
            }
            PointerDrag::Resize {
                start,
                origin,
                edge,
            } => {
                let start = self.to_physical(start);
                let dx = (physical.0 - start.0).round() as i32;
                let dy = (physical.1 - start.1).round() as i32;
                let (mut left, mut top) = (origin.x, origin.y);
                let (mut right, mut bottom) = (origin.right(), origin.bottom());
                // 拖过对边时翻转，而不是卡在 3 像素。
                if matches!(edge, 1 | 5 | 8) {
                    left = (left + dx).clamp(0, w);
                }
                if matches!(edge, 2 | 5 | 6) {
                    top = (top + dy).clamp(0, h);
                }
                if matches!(edge, 3 | 6 | 7) {
                    right = (right + dx).clamp(0, w);
                }
                if matches!(edge, 4 | 7 | 8) {
                    bottom = (bottom + dy).clamp(0, h);
                }
                let (l, r) = (left.min(right), left.max(right).max(left.min(right) + 1));
                let (t, b) = (top.min(bottom), top.max(bottom).max(top.min(bottom) + 1));
                self.selection = RectPx::new(l, t, (r - l) as u32, (b - t) as u32);
            }
            PointerDrag::Mark => self.update_mark(position),
            PointerDrag::MoveMark { index, last, moved } => {
                let (dx, dy) = (physical.0 - last.0, physical.1 - last.1);
                if dx != 0.0 || dy != 0.0 {
                    self.shift_mark(index, dx, dy);
                    self.pointer_drag = Some(PointerDrag::MoveMark {
                        index,
                        last: physical,
                        moved: (moved.0 + dx, moved.1 + dy),
                    });
                }
            }
            PointerDrag::Toolbar { offset } => {
                let screen = self.logical;
                let b = self.ui.toolbar(screen, self.ui_selection());
                self.ui.toolbar_position = Some((
                    (position.0 - offset.0)
                        .clamp(0.0, (screen.0 as f64 - f64::from(b.width)).max(0.0))
                        as f32,
                    (position.1 - offset.1)
                        .clamp(0.0, (screen.1 as f64 - f64::from(b.height) - 8.0).max(0.0))
                        as f32,
                ));
            }
            PointerDrag::Slider { strength } => {
                let before = self.ui.font_size;
                self.ui
                    .slider(self.logical, self.ui_selection(), position, strength);
                if before != self.ui.font_size {
                    self.apply_text_style();
                }
            }
        }
        self.dirty = true;
    }

    /// 字号和颜色变化实时作用于正在编辑的文字。
    pub fn apply_text_style(&mut self) {
        let size = self.ui.font_size as f32 * self.scale();
        let color = self.ui.color;
        self.refresh_text(|mark| {
            mark.font_size = size;
            mark.color = color;
        });
    }

    pub fn finish_number_input(&mut self) {
        if let Some((strength, value)) = self.ui.number_input.take()
            && let Ok(value) = value.parse::<u32>()
        {
            if strength {
                self.ui.strength = value.clamp(1, 20);
            } else {
                self.ui.set_value(value);
                self.apply_text_style();
            }
        }
        self.dirty = true;
    }

    pub fn set_ui_from_action(&mut self, action: Action) {
        let tool_before = self.ui.tool;
        match action {
            Action::Tool(tool) => {
                self.ui.menu = None;
                self.ui.tool = match tool {
                    2 if matches!(self.ui.tool, 2 | 4 | 5 | 13 | 14) => 0,
                    2 => self.ui.shape,
                    tool if self.ui.tool == tool => 0,
                    tool => tool,
                };
            }
            Action::Open(menu) => {
                self.ui.menu = (self.ui.menu != Some(menu)).then_some(menu);
            }
            Action::Shape(shape) => {
                self.ui.shape = shape;
                self.ui.tool = shape;
                self.ui.menu = None;
            }
            Action::Effect(effect) => {
                self.ui.effect = effect;
                self.ui.tool = 8;
                self.ui.menu = None;
            }
            Action::Color(color) => {
                self.ui.color = color;
                self.ui.menu = None;
                self.apply_text_style();
            }
            Action::Undo => {
                self.ui.menu = None;
                self.undo();
            }
            Action::Redo => {
                self.ui.menu = None;
                self.redo();
            }
            Action::Save => {
                self.ui.menu = None;
                let result = self.save();
                self.report(result);
            }
            Action::Copy => {
                self.ui.menu = None;
                let result = self.copy();
                self.report(result);
            }
            Action::Pin => {
                self.ui.menu = None;
                let result = self.pin();
                self.report(result);
            }
            Action::Cancel => self.exit = true,
            // 第一次点击开始长截图，再次点击停止；长图画布中不再嵌套长截图。
            Action::Longshot if self.canvas => {}
            Action::Longshot => {
                self.ui.menu = None;
                self.commit_pending();
                self.longshot = Some(if self.capturing {
                    LongshotRequest::Stop
                } else {
                    LongshotRequest::Start(self.selection)
                });
            }
            Action::Dismiss | Action::Blank => self.ui.menu = None,
            Action::Grip
            | Action::Width
            | Action::Number
            | Action::Strength
            | Action::StrengthNumber => {}
        }
        // 切换工具时提交正在输入的文字，与旧版行为一致。
        if self.ui.tool != tool_before && self.text_editing {
            self.commit_pending();
        }
        self.dirty = true;
    }

    pub fn cursor_for(&self, position: (f64, f64)) -> Cursor {
        if !self.ui.editing {
            return Cursor::Crosshair;
        }
        if let Some(PointerDrag::Move { .. } | PointerDrag::MoveMark { .. }) = self.pointer_drag {
            return Cursor::Grabbing;
        }
        let selected = self.ui_selection();
        if self.ui.hit(self.logical, selected, position).is_some()
            && self.ui.hit(self.logical, selected, position) != Some(Action::Dismiss)
        {
            return Cursor::Default;
        }
        let edge = if self.canvas {
            0
        } else {
            selection_edge(selected, position)
        };
        match edge {
            5 => Cursor::Resize(5),
            6 => Cursor::Resize(6),
            7 => Cursor::Resize(7),
            8 => Cursor::Resize(8),
            1 => Cursor::Resize(1),
            2 => Cursor::Resize(2),
            3 => Cursor::Resize(3),
            4 => Cursor::Resize(4),
            _ if selected.contains(position) => match self.ui.tool {
                0 if self.mark_at(position).is_some() => Cursor::Grab,
                0 => Cursor::Move,
                10 => Cursor::Text,
                _ => Cursor::Crosshair,
            },
            _ => Cursor::Default,
        }
    }

    /// 非阻塞地接收后台分析结果。
    pub fn poll_candidates(&mut self) {
        let Some(receiver) = &self.candidates_pending else {
            return;
        };
        match receiver.try_recv() {
            Ok(candidates) => {
                if !self.hovered
                    && !self.ui.editing
                    && let Some(seed) = candidates
                        .iter()
                        .find(|candidate| candidate.primary)
                        .or_else(|| {
                            candidates
                                .iter()
                                .filter(|candidate| candidate.seed)
                                .max_by_key(|candidate| {
                                    u64::from(candidate.rect.width)
                                        * u64::from(candidate.rect.height)
                                })
                        })
                {
                    self.selection = seed.rect;
                    self.dirty = true;
                }
                self.candidates = candidates;
                self.candidates_pending = None;
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => self.candidates_pending = None,
        }
    }

    pub fn hover_selection(&mut self, position: (f64, f64)) {
        self.poll_candidates();
        if self.candidates_pending.is_some() {
            return;
        }
        self.hovered = true;
        let point = self.to_physical(position);
        let next = self
            .hover
            .hover(&self.candidates, point, self.selection, self.bounds());
        if next != self.selection {
            self.selection = next;
            self.dirty = true;
        }
    }

    pub fn handle_press(&mut self, position: (f64, f64)) {
        self.ui.error = None;
        if self.ui.number_input.is_some() {
            self.finish_number_input();
        }
        let screen = self.logical;
        let selected = self.ui_selection();
        let hit = self.ui.hit(screen, selected, position);
        if self.capturing && !matches!(hit, Some(Action::Longshot | Action::Cancel | Action::Grip))
        {
            return;
        }
        if let Some(action) = hit {
            self.ui.pressed = Some(action);
            match action {
                Action::Grip => {
                    let toolbar = self.ui.toolbar(screen, selected);
                    self.pointer_drag = Some(PointerDrag::Toolbar {
                        offset: (
                            position.0 - f64::from(toolbar.x),
                            position.1 - f64::from(toolbar.y),
                        ),
                    });
                }
                Action::Width | Action::Strength => {
                    let strength = action == Action::Strength;
                    self.pointer_drag = Some(PointerDrag::Slider { strength });
                    self.update_pointer_drag(position);
                }
                Action::Number | Action::StrengthNumber => {
                    self.ui.number_input = Some((action == Action::StrengthNumber, String::new()));
                }
                _ => {}
            }
            self.dirty = true;
            return;
        }
        if !self.ui.editing {
            self.hovered = true;
            // 悬停候选边缘可能恰好经过指针；框选阶段拖动必须优先手动接管，
            // 不能因此变成拉伸候选框。确认后各工具状态仍可拖边/角微调。
            self.pointer_drag = Some(PointerDrag::Select { start: position });
            return;
        }
        // 长图画布的选区固定为整张图，不能拖边或移动。
        let edge = if self.canvas {
            0
        } else {
            selection_edge(selected, position)
        };
        if edge != 0 {
            self.commit_pending();
            self.pointer_drag = Some(PointerDrag::Resize {
                start: position,
                origin: self.selection,
                edge,
            });
        } else if selected.contains(position)
            && self.ui.tool == 0
            && let Some(index) = {
                self.commit_pending();
                self.mark_at(position)
            }
        {
            // 无工具时先命中已有标注：拖动标注；空白处仍然移动选区。
            self.pointer_drag = Some(PointerDrag::MoveMark {
                index,
                last: self.to_physical(position),
                moved: (0.0, 0.0),
            });
        } else if selected.contains(position) && self.ui.tool == 0 && !self.canvas {
            self.commit_pending();
            self.pointer_drag = Some(PointerDrag::Move {
                start: position,
                origin: self.selection,
            });
        } else if selected.contains(position) {
            if !(self.text_editing && self.ui.tool == marks::TEXT) {
                self.commit_pending();
            }
            self.begin_mark(position);
        } else {
            self.commit_pending();
        }
        self.dirty = true;
    }

    pub fn handle_release(&mut self, position: (f64, f64)) {
        self.update_pointer_drag(position);
        let drag = self.pointer_drag.take();
        if let Some(action) = self.ui.pressed.take() {
            if !matches!(
                drag,
                Some(PointerDrag::Toolbar { .. } | PointerDrag::Slider { .. })
            ) && self.ui.hit(self.logical, self.ui_selection(), position) == Some(action)
            {
                self.set_ui_from_action(action);
            }
        } else if matches!(drag, Some(PointerDrag::Mark)) {
            self.commit_pending();
        } else if let Some(PointerDrag::MoveMark { index, moved, .. }) = drag
            && (moved.0 != 0.0 || moved.1 != 0.0)
        {
            self.edits.push(Edit::Move {
                index,
                dx: moved.0,
                dy: moved.1,
            });
            self.redo.clear();
        } else if matches!(
            drag,
            Some(PointerDrag::Select { .. } | PointerDrag::Resize { .. })
        ) && self.selection.width > 2
            && self.selection.height > 2
        {
            self.ui.editing = true;
        }
        self.dirty = true;
    }

    pub fn key(&mut self, key: Keysym, utf8: Option<&str>) {
        let (ctrl, shift) = (self.ctrl, self.shift);
        self.dirty = true;
        if self.ui.number_input.is_some() && !ctrl {
            match key {
                Keysym::Escape => self.ui.number_input = None,
                Keysym::Return | Keysym::KP_Enter => self.finish_number_input(),
                Keysym::BackSpace => {
                    if let Some((_, value)) = &mut self.ui.number_input {
                        value.pop();
                    }
                }
                _ => {
                    if let (Some(text), Some((_, value))) = (utf8, &mut self.ui.number_input)
                        && text.chars().all(|ch| ch.is_ascii_digit())
                        && value.len() < 3
                    {
                        value.push_str(text);
                    }
                }
            }
            return;
        }
        if self.text_editing
            && !(ctrl
                && matches!(
                    key,
                    Keysym::z | Keysym::Z | Keysym::y | Keysym::s | Keysym::c
                ))
        {
            match key {
                // Esc 放弃正在输入的文字；Ctrl+Enter、切换工具或点击别处提交。
                Keysym::Escape => {
                    self.text_editing = false;
                    self.preedit.clear();
                    let bounds = self.pending.take().and_then(|mark| mark.bounds());
                    self.damage(bounds);
                }
                Keysym::Return | Keysym::KP_Enter if ctrl => self.commit_pending(),
                Keysym::Return | Keysym::KP_Enter => self.refresh_text(|mark| mark.text.push('\n')),
                Keysym::BackSpace => self.refresh_text(|mark| {
                    mark.text.pop();
                }),
                _ => {
                    if let Some(text) = utf8
                        && !ctrl
                        && !text.chars().any(char::is_control)
                    {
                        let text = text.to_owned();
                        self.refresh_text(|mark| mark.text.push_str(&text));
                    }
                }
            }
            return;
        }
        match key {
            Keysym::Escape => {
                if self.ui.menu.is_some() {
                    self.ui.menu = None;
                } else if self.pending.is_some() {
                    let bounds = self.pending.take().and_then(|mark| mark.bounds());
                    self.pointer_drag = None;
                    self.damage(bounds);
                } else if self.ui.tool != 0 {
                    self.ui.tool = 0;
                } else {
                    self.exit = true;
                }
            }
            Keysym::z | Keysym::Z if ctrl && shift => self.redo(),
            Keysym::z | Keysym::Z if ctrl => self.undo(),
            Keysym::y | Keysym::Y if ctrl => self.redo(),
            Keysym::s | Keysym::S if ctrl => {
                let result = self.save();
                self.report(result);
            }
            Keysym::c | Keysym::C if ctrl => {
                let result = self.copy();
                self.report(result);
            }
            Keysym::Return | Keysym::KP_Enter if !self.ui.editing => self.ui.editing = true,
            _ => {}
        }
    }
}

pub fn selection_edge(selection: BoxF, position: (f64, f64)) -> u8 {
    let (x, y) = (position.0 as f32, position.1 as f32);
    let (left, top) = (selection.x, selection.y);
    let (right, bottom) = (left + selection.width, top + selection.height);
    let near = |a: f32, b: f32| (a - b).abs() <= 8.0;
    let inside_x = x >= left - 8.0 && x <= right + 8.0;
    let inside_y = y >= top - 8.0 && y <= bottom + 8.0;
    if near(x, left) && near(y, top) {
        5
    } else if near(x, right) && near(y, top) {
        6
    } else if near(x, right) && near(y, bottom) {
        7
    } else if near(x, left) && near(y, bottom) {
        8
    } else if near(x, left) && inside_y {
        1
    } else if near(y, top) && inside_x {
        2
    } else if near(x, right) && inside_y {
        3
    } else if near(y, bottom) && inside_x {
        4
    } else {
        0
    }
}
