//! 原生截图编辑器：SCTK layer-shell 主表面 + 子表面场景（见 `scene.rs`）。
//!
//! 内存模型：截图 memfd 是唯一的整屏像素副本，同时用于显示与导出；界面控件和
//! 标注瓦片只占用各自的小缓冲区，标注历史只保存矢量命令。
use crate::scene::{Scene, SceneInput, TileSlot};
use babry::{
    config,
    core::RectPx,
    export,
    font::Fonts,
    marks::{self, Mark},
    native_ui::{self, Action, BoxF},
    shm_image::ShmImage,
    smart_select,
};
use smithay_client_toolkit::reexports::client::{
    Connection, Dispatch, EventQueue, QueueHandle, delegate_noop,
    globals::{GlobalList, registry_queue_init},
    protocol::{
        wl_buffer::{self, WlBuffer},
        wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm_pool, wl_surface,
    },
};
use smithay_client_toolkit::reexports::protocols::wp::{
    cursor_shape::v1::client::wp_cursor_shape_device_v1::{Shape, WpCursorShapeDeviceV1},
    text_input::zv3::client::{
        zwp_text_input_manager_v3::ZwpTextInputManagerV3,
        zwp_text_input_v3::{self, ZwpTextInputV3},
    },
    viewporter::client::{wp_viewport, wp_viewporter},
};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_keyboard, delegate_layer, delegate_output, delegate_pointer,
    delegate_registry, delegate_seat, delegate_shm, delegate_subcompositor,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        Capability, SeatHandler, SeatState,
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers, RawModifiers},
        pointer::{
            PointerEvent, PointerEventKind, PointerHandler, cursor_shape::CursorShapeManager,
        },
    },
    shell::{
        WaylandSurface,
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
    },
    shm::{Shm, ShmHandler},
    subcompositor::SubcompositorState,
};
use std::{
    path::PathBuf,
    process::{Command, Stdio},
    sync::{
        Arc,
        mpsc::{Receiver, TryRecvError},
    },
    time::{Duration, Instant},
};

const LEFT: u32 = 0x110;
const RIGHT: u32 = 0x111;

#[derive(Clone, Copy, Debug)]
enum PointerDrag {
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
    Toolbar {
        offset: (f64, f64),
    },
    Slider {
        strength: bool,
    },
}

pub struct Session {
    pub source: Arc<ShmImage>,
    pub direct_edit: bool,
    pub save_dir: PathBuf,
    pub output: Option<String>,
    pub logical: (u32, u32),
    pub selection: RectPx,
    /// 后台线程完成分析后送达；`None` 表示直接编辑全屏。
    pub candidates: Option<Receiver<Vec<smart_select::Candidate>>>,
}

pub struct NativeEditor {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    shm: Shm,
    layer: LayerSurface,
    viewport: wp_viewport::WpViewport,
    parts: Option<(
        CompositorState,
        SubcompositorState,
        wp_viewporter::WpViewporter,
    )>,
    pub scene: Option<Scene>,
    source: Arc<ShmImage>,
    physical: (u32, u32),
    logical: (u32, u32),
    selection: RectPx,
    candidates: Vec<smart_select::Candidate>,
    candidates_pending: Option<Receiver<Vec<smart_select::Candidate>>>,
    /// 用户尚未悬停或拖动时，候选到达后可用吸附后的窗口种子替换初始选区。
    hovered: bool,
    hover: smart_select::Cursor,
    ui: native_ui::EditorUi,
    fonts: Fonts,
    marks: Vec<Mark>,
    pending: Option<Mark>,
    redo: Vec<Mark>,
    text_editing: bool,
    pointer: Option<wl_pointer::WlPointer>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    cursor_manager: Option<CursorShapeManager>,
    cursor_device: Option<WpCursorShapeDeviceV1>,
    cursor_serial: u32,
    cursor_shape: Option<Shape>,
    pointer_drag: Option<PointerDrag>,
    /// text-input-v3：中文等输入法的预编辑与提交。
    text_input_manager: Option<ZwpTextInputManagerV3>,
    text_input: Option<ZwpTextInputV3>,
    ime_focus: bool,
    ime_enabled: bool,
    ime_cursor: Option<(i32, i32, i32, i32)>,
    preedit: String,
    ime_pending: (Option<String>, Option<String>),
    modifiers: Modifiers,
    /// 部分虚拟键盘只发送修饰键按键而不更新修饰状态，单独跟踪按下状态。
    held: (bool, bool),
    save_dir: PathBuf,
    output_name: Option<String>,
    configured: bool,
    frame_pending: bool,
    frame_requested: Instant,
    dirty: bool,
    exit: bool,
    started: Instant,
    presented: bool,
    _probe: Option<(EventQueue<OutputProbe>, OutputProbe, wl_output::WlOutput)>,
}

impl NativeEditor {
    pub fn run(session: Session) -> Result<(), String> {
        let started = Instant::now();
        let connection =
            Connection::connect_to_env().map_err(|error| format!("无法连接 Wayland: {error}"))?;
        let (globals, mut queue) = registry_queue_init::<NativeEditor>(&connection)
            .map_err(|error| format!("读取 Wayland 全局对象失败: {error}"))?;
        let qh = queue.handle();
        let compositor = CompositorState::bind(&globals, &qh)
            .map_err(|error| format!("wl_compositor 不可用: {error}"))?;
        let subcompositor =
            SubcompositorState::bind(compositor.wl_compositor().clone(), &globals, &qh)
                .map_err(|error| format!("wl_subcompositor 不可用: {error}"))?;
        let shell = LayerShell::bind(&globals, &qh)
            .map_err(|error| format!("wlr-layer-shell 不可用: {error}"))?;
        let shm = Shm::bind(&globals, &qh).map_err(|error| format!("wl_shm 不可用: {error}"))?;
        let viewporter: wp_viewporter::WpViewporter = globals
            .bind(&qh, 1..=1, ())
            .map_err(|error| format!("合成器缺少 wp_viewporter: {error}"))?;
        let cursor_manager = CursorShapeManager::bind(&globals, &qh).ok();
        let text_input_manager: Option<ZwpTextInputManagerV3> = globals.bind(&qh, 1..=1, ()).ok();
        let output_state = OutputState::new(&globals, &qh);
        let registry_state = RegistryState::new(&globals);
        let seat_state = SeatState::new(&globals, &qh);
        // 把覆盖层绑定到截图所在的输出，多显示器时不会出现在错误屏幕。
        let probe = session
            .output
            .as_deref()
            .and_then(|name| OutputProbe::find(&connection, &globals, name));
        let output = probe.as_ref().map(|(_, _, output)| output.clone());
        let surface = compositor.create_surface(&qh);
        let viewport = viewporter.get_viewport(&surface, &qh, ());
        let layer = shell.create_layer_surface(
            &qh,
            surface,
            Layer::Overlay,
            Some("babry"),
            output.as_ref(),
        );
        layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
        layer.set_size(0, 0);
        layer.commit();
        let physical = (session.source.width(), session.source.height());
        let mut editor = Self {
            registry_state,
            seat_state,
            output_state,
            shm,
            layer,
            viewport,
            parts: Some((compositor, subcompositor, viewporter)),
            scene: None,
            source: session.source,
            physical,
            logical: session.logical,
            selection: session.selection,
            candidates: Vec::new(),
            candidates_pending: session.candidates,
            hovered: false,
            hover: smart_select::Cursor::default(),
            ui: native_ui::EditorUi {
                editing: session.direct_edit,
                ..native_ui::EditorUi::default()
            },
            fonts: Fonts::new(),
            marks: Vec::new(),
            pending: None,
            redo: Vec::new(),
            text_editing: false,
            pointer: None,
            keyboard: None,
            cursor_manager,
            cursor_device: None,
            cursor_serial: 0,
            cursor_shape: None,
            pointer_drag: None,
            text_input_manager,
            text_input: None,
            ime_focus: false,
            ime_enabled: false,
            ime_cursor: None,
            preedit: String::new(),
            ime_pending: (None, None),
            modifiers: Modifiers::default(),
            held: (false, false),
            save_dir: session.save_dir,
            output_name: session.output,
            configured: false,
            frame_pending: false,
            frame_requested: Instant::now(),
            dirty: true,
            exit: false,
            started,
            presented: false,
            _probe: probe,
        };
        while !editor.exit {
            queue
                .blocking_dispatch(&mut editor)
                .map_err(|error| format!("Wayland 事件循环失败: {error}"))?;
            editor.draw(&qh)?;
        }
        Ok(())
    }
}

/// 独立事件队列上的输出探测，只用于在创建覆盖层前按名称找到 wl_output。
pub struct OutputProbe {
    registry: RegistryState,
    outputs: OutputState,
}

impl OutputProbe {
    fn find(
        connection: &Connection,
        globals: &GlobalList,
        name: &str,
    ) -> Option<(EventQueue<Self>, Self, wl_output::WlOutput)> {
        let mut queue = connection.new_event_queue::<Self>();
        let qh = queue.handle();
        let mut probe = Self {
            registry: RegistryState::new(globals),
            outputs: OutputState::new(globals, &qh),
        };
        queue.roundtrip(&mut probe).ok()?;
        queue.roundtrip(&mut probe).ok()?;
        let output = probe.outputs.outputs().find(|output| {
            probe
                .outputs
                .info(output)
                .and_then(|info| info.name)
                .as_deref()
                == Some(name)
        })?;
        Some((queue, probe, output))
    }
}

impl OutputHandler for OutputProbe {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.outputs
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}
delegate_output!(OutputProbe);
delegate_registry!(OutputProbe);
impl ProvidesRegistryState for OutputProbe {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState];
}

impl NativeEditor {
    fn draw(&mut self, qh: &QueueHandle<Self>) -> Result<(), String> {
        self.poll_candidates();
        if !self.configured || self.exit || !self.dirty {
            return Ok(());
        }
        // 帧回调节流；合成器长时间不回调时也不让界面卡死。
        if self.frame_pending && self.frame_requested.elapsed() < Duration::from_millis(100) {
            return Ok(());
        }
        // 预编辑文字只用于显示，不进入标注历史。
        let committed_len = match self.pending.as_mut() {
            Some(mark) if self.text_editing && !self.preedit.is_empty() => {
                let len = mark.text.len();
                mark.text.push_str(&self.preedit);
                Some(len)
            }
            _ => None,
        };
        let caret = self.caret_rect();
        self.sync_ime(caret);
        let Some(scene) = self.scene.as_mut() else {
            return Ok(());
        };
        let pending = self.pending.take();
        let has_pending = pending.is_some();
        self.marks.extend(pending);
        let result = scene.update(
            SceneInput {
                selection: self.selection,
                ui: &self.ui,
                marks: &self.marks,
                source: &self.source,
                fonts: &mut self.fonts,
                caret,
            },
            qh,
        );
        if has_pending {
            self.pending = self.marks.pop();
        }
        if let (Some(len), Some(mark)) = (committed_len, self.pending.as_mut()) {
            mark.text.truncate(len);
        }
        let (changed, complete) = result?;
        self.dirty = !complete;
        if changed || !self.presented {
            let surface = self.layer.wl_surface();
            surface.frame(qh, surface.clone());
            self.layer.commit();
            self.frame_pending = true;
            self.frame_requested = Instant::now();
        }
        Ok(())
    }

    fn scale(&self) -> f32 {
        self.physical.0 as f32 / self.logical.0.max(1) as f32
    }

    fn bounds(&self) -> RectPx {
        RectPx::new(0, 0, self.physical.0, self.physical.1)
    }

    fn to_physical(&self, logical: (f64, f64)) -> (f32, f32) {
        (
            (logical.0 * self.physical.0 as f64 / self.logical.0.max(1) as f64) as f32,
            (logical.1 * self.physical.1 as f64 / self.logical.1.max(1) as f64) as f32,
        )
    }

    fn ui_selection(&self) -> BoxF {
        native_ui::logical_selection(self.selection, self.physical, self.logical)
    }

    fn damage(&mut self, rect: Option<RectPx>) {
        if let (Some(scene), Some(rect)) = (self.scene.as_mut(), rect) {
            scene.damage(rect);
        }
        self.dirty = true;
    }

    fn caret_rect(&mut self) -> Option<RectPx> {
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

    fn update_next_number(&mut self) {
        self.ui.next_number = self
            .marks
            .iter()
            .filter(|m| m.kind == marks::NUMBER)
            .count() as u32
            + 1;
    }

    /// 提交正在编辑的文字或尚未松开的标注；空文字和零尺寸图形直接丢弃。
    fn commit_pending(&mut self) {
        self.text_editing = false;
        self.preedit.clear();
        let Some(mark) = self.pending.take() else {
            return;
        };
        let empty = match mark.kind {
            marks::TEXT => mark.text.trim().is_empty(),
            marks::RECTANGLE | marks::SQUARE | marks::CIRCLE => {
                (mark.x2 - mark.x1).abs() < 2.0 && (mark.y2 - mark.y1).abs() < 2.0
            }
            _ => false,
        };
        if empty {
            self.damage(mark.bounds());
            return;
        }
        self.marks.push(mark);
        self.redo.clear();
        self.update_next_number();
        self.dirty = true;
    }

    fn undo(&mut self) {
        self.commit_pending();
        if let Some(mark) = self.marks.pop() {
            self.damage(mark.bounds());
            self.redo.push(mark);
            self.update_next_number();
        }
    }

    fn redo(&mut self) {
        self.commit_pending();
        if let Some(mark) = self.redo.pop() {
            self.damage(mark.bounds());
            self.marks.push(mark);
            self.update_next_number();
        }
    }

    fn begin_mark(&mut self, position: (f64, f64)) {
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

    fn update_mark(&mut self, position: (f64, f64)) {
        let (x, y) = self.to_physical(position);
        let s = self.selection;
        let x = x.clamp(s.x as f32, (s.right() - 1) as f32);
        let y = y.clamp(s.y as f32, (s.bottom() - 1) as f32);
        let Some(mark) = self.pending.as_mut() else {
            return;
        };
        let before = mark.bounds();
        let last = mark.points.last().copied();
        if mark.kind == marks::NUMBER {
            mark.x1 = x;
            mark.y1 = y;
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

    fn begin_text(&mut self, x: f32, y: f32) {
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
    fn refresh_text(&mut self, edit: impl FnOnce(&mut Mark)) {
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

    fn export_to(&mut self, path: &std::path::Path) -> Result<(), String> {
        self.commit_pending();
        export::save_png(
            path,
            &self.source,
            self.selection,
            &self.marks,
            &mut self.fonts,
        )
    }

    fn save(&mut self) -> Result<(), String> {
        let path = config::output_path(&self.save_dir)?;
        self.export_to(&path)?;
        eprintln!("已保存: {}", path.display());
        self.exit = true;
        Ok(())
    }

    fn copy(&mut self) -> Result<(), String> {
        let path = config::output_path(&self.save_dir)?;
        self.export_to(&path)?;
        let file = std::fs::File::open(&path).map_err(|error| error.to_string())?;
        let status = Command::new("wl-copy")
            .args(["--type", "image/png"])
            .stdin(file)
            .status()
            .map_err(|error| format!("无法启动 wl-copy: {error}"))?;
        if !status.success() {
            return Err(format!("wl-copy 失败: {status}"));
        }
        self.exit = true;
        Ok(())
    }

    /// 贴图在独立进程中显示，编辑器退出后释放整张截图。
    fn pin(&mut self) -> Result<(), String> {
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

    /// 文字编辑期间启用输入法，并把光标矩形（表面逻辑坐标）告诉输入法定位候选窗。
    fn sync_ime(&mut self, caret: Option<RectPx>) {
        let Some(text_input) = &self.text_input else {
            return;
        };
        let want = self.text_editing && self.ime_focus;
        if !want {
            if self.ime_enabled {
                text_input.disable();
                text_input.commit();
                self.ime_enabled = false;
                self.ime_cursor = None;
            }
            return;
        }
        if !self.ime_enabled {
            text_input.enable();
            text_input.set_content_type(
                zwp_text_input_v3::ContentHint::None,
                zwp_text_input_v3::ContentPurpose::Normal,
            );
            self.ime_enabled = true;
            self.ime_cursor = None;
        }
        let scale = self.scale();
        let cursor = caret.map(|c| {
            (
                (c.x as f32 / scale).round() as i32,
                (c.y as f32 / scale).round() as i32,
                2,
                ((c.height as f32 / scale).round() as i32).max(1),
            )
        });
        if cursor != self.ime_cursor {
            if let Some((x, y, w, h)) = cursor {
                text_input.set_cursor_rectangle(x, y, w, h);
            }
            text_input.commit();
            self.ime_cursor = cursor;
        }
    }

    fn report(&mut self, result: Result<(), String>) {
        if let Err(error) = result {
            self.ui.error = Some(error);
            self.dirty = true;
        }
    }
}

impl NativeEditor {
    fn update_pointer_drag(&mut self, position: (f64, f64)) {
        let Some(drag) = self.pointer_drag else {
            return;
        };
        let physical = self.to_physical(position);
        let (w, h) = (self.physical.0 as i32, self.physical.1 as i32);
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
    fn apply_text_style(&mut self) {
        let size = self.ui.font_size as f32 * self.scale();
        let color = self.ui.color;
        self.refresh_text(|mark| {
            mark.font_size = size;
            mark.color = color;
        });
    }

    fn finish_number_input(&mut self) {
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

    fn set_ui_from_action(&mut self, action: Action) {
        let tool_before = self.ui.tool;
        match action {
            Action::Tool(tool) => {
                self.ui.menu = None;
                self.ui.tool = match tool {
                    2 if matches!(self.ui.tool, 2 | 13 | 14) => 0,
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
            Action::Dismiss | Action::More | Action::Blank => self.ui.menu = None,
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

    fn cursor_for(&self, position: (f64, f64)) -> Shape {
        if !self.ui.editing && !matches!(self.pointer_drag, Some(PointerDrag::Resize { .. })) {
            let edge = selection_edge(self.ui_selection(), position);
            if edge == 0 || matches!(self.pointer_drag, Some(PointerDrag::Select { .. })) {
                return Shape::Crosshair;
            }
        }
        if let Some(PointerDrag::Move { .. }) = self.pointer_drag {
            return Shape::Grabbing;
        }
        let selected = self.ui_selection();
        if self.ui.hit(self.logical, selected, position).is_some()
            && self.ui.hit(self.logical, selected, position) != Some(Action::Dismiss)
        {
            return Shape::Default;
        }
        match selection_edge(selected, position) {
            5 => Shape::NwResize,
            6 => Shape::NeResize,
            7 => Shape::SeResize,
            8 => Shape::SwResize,
            1 => Shape::WResize,
            2 => Shape::NResize,
            3 => Shape::EResize,
            4 => Shape::SResize,
            _ if selected.contains(position) => match self.ui.tool {
                0 => Shape::Move,
                10 => Shape::Text,
                _ => Shape::Crosshair,
            },
            _ => Shape::Default,
        }
    }

    fn update_cursor(&mut self, position: (f64, f64)) {
        let shape = self.cursor_for(position);
        if self.cursor_shape != Some(shape)
            && let Some(device) = &self.cursor_device
        {
            device.set_shape(self.cursor_serial, shape);
            self.cursor_shape = Some(shape);
        }
    }

    /// 非阻塞地接收后台分析结果。
    fn poll_candidates(&mut self) {
        let Some(receiver) = &self.candidates_pending else {
            return;
        };
        match receiver.try_recv() {
            Ok(candidates) => {
                if !self.hovered
                    && !self.ui.editing
                    && let Some(seed) = candidates
                        .iter()
                        .filter(|candidate| candidate.seed)
                        .max_by_key(|c| u64::from(c.rect.width) * u64::from(c.rect.height))
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

    fn hover_selection(&mut self, position: (f64, f64)) {
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

    fn handle_press(&mut self, position: (f64, f64)) {
        self.ui.error = None;
        if self.ui.number_input.is_some() {
            self.finish_number_input();
        }
        let screen = self.logical;
        let selected = self.ui_selection();
        if let Some(action) = self.ui.hit(screen, selected, position) {
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
        let edge = selection_edge(selected, position);
        if !self.ui.editing {
            self.hovered = true;
            // 选区阶段也可以直接拖动当前候选的边或角微调。
            self.pointer_drag = Some(if edge != 0 {
                PointerDrag::Resize {
                    start: position,
                    origin: self.selection,
                    edge,
                }
            } else {
                PointerDrag::Select { start: position }
            });
            return;
        }
        if edge != 0 {
            self.commit_pending();
            self.pointer_drag = Some(PointerDrag::Resize {
                start: position,
                origin: self.selection,
                edge,
            });
        } else if selected.contains(position) && self.ui.tool == 0 {
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

    fn handle_release(&mut self, position: (f64, f64)) {
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
}

fn selection_edge(selection: BoxF, position: (f64, f64)) -> u8 {
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

impl CompositorHandler for NativeEditor {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: i32,
    ) {
    }
    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }
    fn frame(&mut self, _: &Connection, qh: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {
        self.frame_pending = false;
        if !self.presented {
            self.presented = true;
            if std::env::var_os("BABRY_TRACE").is_some() {
                eprintln!(
                    "babry: 首帧已呈现 {:.1}ms",
                    self.started.elapsed().as_secs_f64() * 1000.0
                );
            }
        }
        if let Err(error) = self.draw(qh) {
            self.ui.error = Some(error);
        }
    }
    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for NativeEditor {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl SeatHandler for NativeEditor {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
    fn new_capability(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer && self.pointer.is_none() {
            self.pointer = self.seat_state.get_pointer(qh, &seat).ok();
            if let (Some(manager), Some(pointer)) = (&self.cursor_manager, &self.pointer) {
                self.cursor_device = Some(manager.get_shape_device(pointer, qh));
            }
        }
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            self.keyboard = self.seat_state.get_keyboard(qh, &seat, None).ok();
            if let Some(manager) = &self.text_input_manager {
                self.text_input = Some(manager.get_text_input(&seat, qh, ()));
            }
        }
    }
    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer {
            self.pointer = None;
            self.cursor_device = None;
        }
        if capability == Capability::Keyboard {
            self.keyboard = None;
        }
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl KeyboardHandler for NativeEditor {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: &wl_surface::WlSurface,
        _: u32,
        _: &[u32],
        _: &[Keysym],
    ) {
    }
    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: &wl_surface::WlSurface,
        _: u32,
    ) {
        self.held = (false, false);
    }
    fn press_key(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        self.key(event);
        let _ = self.draw(qh);
    }
    fn repeat_key(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        self.key(event);
        let _ = self.draw(qh);
    }
    fn release_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        match event.keysym {
            Keysym::Control_L | Keysym::Control_R => self.held.0 = false,
            Keysym::Shift_L | Keysym::Shift_R => self.held.1 = false,
            _ => {}
        }
    }
    fn update_modifiers(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        modifiers: Modifiers,
        _: RawModifiers,
        _: u32,
    ) {
        self.modifiers = modifiers;
    }
}

impl NativeEditor {
    fn key(&mut self, event: KeyEvent) {
        let key = event.keysym;
        match key {
            Keysym::Control_L | Keysym::Control_R => self.held.0 = true,
            Keysym::Shift_L | Keysym::Shift_R => self.held.1 = true,
            _ => {}
        }
        let ctrl = self.modifiers.ctrl || self.held.0;
        let shift = self.modifiers.shift || self.held.1;
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
                    if let (Some(text), Some((_, value))) =
                        (event.utf8.as_deref(), &mut self.ui.number_input)
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
                    if let Some(text) = event.utf8.as_deref()
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

impl PointerHandler for NativeEditor {
    fn pointer_frame(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        _: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for event in events {
            if event.surface != *self.layer.wl_surface() {
                continue;
            }
            match event.kind {
                PointerEventKind::Enter { serial } => {
                    self.cursor_serial = serial;
                    self.cursor_shape = None;
                    self.ui.pointer = event.position;
                    if !self.ui.editing {
                        self.hover_selection(event.position);
                    }
                }
                PointerEventKind::Leave { .. } => {
                    self.ui.pointer = (-1.0, -1.0);
                    self.dirty = true;
                }
                PointerEventKind::Press { button: LEFT, .. } => self.handle_press(event.position),
                PointerEventKind::Press { button: RIGHT, .. } => {
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
                PointerEventKind::Release { button: LEFT, .. } => {
                    self.handle_release(event.position)
                }
                PointerEventKind::Motion { .. } => {
                    self.ui.pointer = event.position;
                    if self.pointer_drag.is_some() {
                        self.update_pointer_drag(event.position);
                    } else if !self.ui.editing {
                        self.hover_selection(event.position);
                    } else {
                        // 悬停只影响工具栏/菜单，状态键不变时场景不会重绘。
                        self.dirty = true;
                    }
                }
                PointerEventKind::Axis { vertical, .. } if !self.ui.editing => {
                    self.poll_candidates();
                    let steps = if vertical.value120 != 0 {
                        vertical.value120.signum()
                    } else if vertical.discrete != 0 {
                        vertical.discrete.signum()
                    } else {
                        (vertical.absolute.signum()) as i32
                    };
                    if steps != 0 {
                        let point = self.to_physical(event.position);
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
                _ => {}
            }
            self.update_cursor(event.position);
        }
        if let Err(error) = self.draw(qh) {
            self.ui.error = Some(error);
        }
    }
}

impl LayerShellHandler for NativeEditor {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        self.exit = true;
    }
    fn configure(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        _: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _: u32,
    ) {
        if self.configured {
            return;
        }
        let (w, h) = configure.new_size;
        if w > 0 && h > 0 {
            self.logical = (w, h);
        }
        self.viewport
            .set_destination(self.logical.0 as i32, self.logical.1 as i32);
        let Some((compositor, subcompositor, viewporter)) = self.parts.take() else {
            return;
        };
        match Scene::new(
            compositor,
            subcompositor,
            viewporter,
            &self.shm,
            self.layer.wl_surface(),
            &self.source,
            self.logical,
            qh,
        ) {
            Ok(scene) => {
                self.ui.grid = scene.grid();
                self.scene = Some(scene);
                self.configured = true;
                self.dirty = true;
                if let Err(error) = self.draw(qh) {
                    eprintln!("babry: {error}");
                    self.exit = true;
                }
            }
            Err(error) => {
                eprintln!("babry: {error}");
                self.exit = true;
            }
        }
    }
}

impl ShmHandler for NativeEditor {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl Dispatch<WlBuffer, TileSlot> for NativeEditor {
    fn event(
        state: &mut Self,
        _: &WlBuffer,
        event: wl_buffer::Event,
        slot: &TileSlot,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event
            && let Some(scene) = state.scene.as_mut()
        {
            scene.tile_released(slot.0);
            // 之前因槽位占用而推迟的瓦片在 release 后重试。
            state.dirty = true;
        }
    }
}

impl Dispatch<ZwpTextInputV3, ()> for NativeEditor {
    fn event(
        state: &mut Self,
        _: &ZwpTextInputV3,
        event: zwp_text_input_v3::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwp_text_input_v3::Event::Enter { surface } => {
                state.ime_focus = surface == *state.layer.wl_surface();
                state.ime_enabled = false;
                state.dirty = true;
            }
            zwp_text_input_v3::Event::Leave { .. } => {
                state.ime_focus = false;
                state.ime_enabled = false;
            }
            zwp_text_input_v3::Event::PreeditString { text, .. } => {
                state.ime_pending.1 = Some(text.unwrap_or_default());
            }
            zwp_text_input_v3::Event::CommitString { text } => {
                state.ime_pending.0 = text;
            }
            zwp_text_input_v3::Event::Done { .. } => {
                let (commit, preedit) = std::mem::take(&mut state.ime_pending);
                state.preedit = preedit.unwrap_or_default();
                let commit = commit.unwrap_or_default();
                state.refresh_text(|mark| mark.text.push_str(&commit));
            }
            _ => {}
        }
    }
}

delegate_noop!(NativeEditor: ignore ZwpTextInputManagerV3);
delegate_compositor!(NativeEditor);
delegate_subcompositor!(NativeEditor);
delegate_output!(NativeEditor);
delegate_shm!(NativeEditor);
delegate_noop!(NativeEditor: ignore wp_viewporter::WpViewporter);
delegate_noop!(NativeEditor: ignore wp_viewport::WpViewport);
delegate_noop!(NativeEditor: ignore wl_shm_pool::WlShmPool);
delegate_noop!(NativeEditor: ignore WlBuffer);
delegate_seat!(NativeEditor);
delegate_keyboard!(NativeEditor);
delegate_pointer!(NativeEditor);
delegate_layer!(NativeEditor);
delegate_registry!(NativeEditor);
impl ProvidesRegistryState for NativeEditor {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}
