//! 原生截图编辑器：SCTK layer-shell 主表面 + 子表面场景（见 `scene.rs`）。
//!
//! 内存模型：截图 memfd 是唯一的整屏像素副本，同时用于显示与导出；界面控件和
//! 标注瓦片只占用各自的小缓冲区，标注历史只保存矢量命令。
use crate::{
    longshot::Capture,
    scene::{RetiredSlot, Scene, SceneInput, TileSlot},
};
use babry::{
    compositor::{self, Backend, BoundOutput},
    core::RectPx,
    editor_core::{Core, CoreInit, Cursor, LongshotRequest},
    screencopy,
    shm_image::ShmImage,
    smart_select,
    soft_render::{self, SoftCache},
};
use smithay_client_toolkit::reexports::client::{
    Connection, Dispatch, QueueHandle, delegate_noop,
    globals::registry_queue_init,
    protocol::{
        wl_buffer::{self, WlBuffer},
        wl_callback, wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm, wl_shm_pool, wl_surface,
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
    shm::{
        Shm, ShmHandler,
        slot::{Buffer, SlotPool},
    },
    subcompositor::SubcompositorState,
};
use std::{
    path::PathBuf,
    sync::{Arc, mpsc::Receiver},
    time::{Duration, Instant},
};

const LEFT: u32 = 0x110;
const RIGHT: u32 = 0x111;

pub struct Session {
    pub source: Arc<ShmImage>,
    pub direct_edit: bool,
    pub save_dir: PathBuf,
    pub output: Option<String>,
    pub logical: (u32, u32),
    pub selection: RectPx,
    /// 后台线程完成分析后送达；`None` 表示直接编辑全屏。
    pub candidates: Option<Receiver<Vec<smart_select::Candidate>>>,
    /// 长截图需要输出的逻辑尺寸、位置和缩放。
    pub output_info: Option<compositor::OutputInfo>,
}

/// 长图画布的整帧缓冲区：两块屏幕大小的 wl_shm 轮换，每次重绘整帧。
struct SoftSurface {
    pool: SlotPool,
    buffers: Vec<Buffer>,
    cache: SoftCache,
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
    soft: Option<SoftSurface>,
    /// 场景销毁后保留，用于长截图期间新建主表面和设置输入区域。
    compositor: Option<CompositorState>,
    viewporter: Option<wp_viewporter::WpViewporter>,
    shell: LayerShell,
    output_info: Option<compositor::OutputInfo>,
    capture: Option<Capture>,
    core: Core,
    pointer: Option<wl_pointer::WlPointer>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    cursor_manager: Option<CursorShapeManager>,
    cursor_device: Option<WpCursorShapeDeviceV1>,
    cursor_serial: u32,
    cursor_shape: Option<Shape>,
    /// 画笔范围光标：自绘圆环挂在独立的光标表面上，跟随指针无需重绘编辑器。
    brush: BrushCursor,
    /// text-input-v3：中文等输入法的预编辑与提交。
    text_input_manager: Option<ZwpTextInputManagerV3>,
    text_input: Option<ZwpTextInputV3>,
    ime_focus: bool,
    ime_enabled: bool,
    ime_cursor: Option<(i32, i32, i32, i32)>,
    /// KWin 的输入法窗口位于 Overlay 层，输入时将编辑器临时降到 Top 层。
    kde: bool,
    ime_layer: bool,
    ime_pending: (Option<String>, Option<String>),
    modifiers: Modifiers,
    /// 部分虚拟键盘只发送修饰键按键而不更新修饰状态，单独跟踪按下状态。
    held: (bool, bool),
    configured: bool,
    frame_pending: bool,
    frame_requested: Instant,
    started: Instant,
    presented: bool,
    _output: Option<BoundOutput>,
}

struct BrushCursor {
    surface: wl_surface::WlSurface,
    pool: Option<SlotPool>,
    buffer: Option<Buffer>,
    /// 当前缓冲区对应的（物理直径，缓冲区整数缩放）；`None` 表示没有挂在指针上。
    key: Option<(u32, i32)>,
}

fn shape(cursor: Cursor) -> Shape {
    match cursor {
        Cursor::Default => Shape::Default,
        Cursor::Crosshair => Shape::Crosshair,
        Cursor::Move => Shape::Move,
        Cursor::Grab => Shape::Grab,
        Cursor::Grabbing => Shape::Grabbing,
        Cursor::Text => Shape::Text,
        Cursor::Resize(5) => Shape::NwResize,
        Cursor::Resize(6) => Shape::NeResize,
        Cursor::Resize(7) => Shape::SeResize,
        Cursor::Resize(8) => Shape::SwResize,
        Cursor::Resize(1) => Shape::WResize,
        Cursor::Resize(2) => Shape::NResize,
        Cursor::Resize(3) => Shape::EResize,
        Cursor::Resize(_) => Shape::SResize,
        Cursor::Brush(_) => Shape::Crosshair,
    }
}

impl NativeEditor {
    /// 运行编辑器；返回 `Some(选区)` 表示用户选择对该选区做长截图。
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
        let output = session
            .output
            .as_deref()
            .map(|name| compositor::bind_output(&connection, name))
            .transpose()?;
        let brush = BrushCursor {
            surface: compositor.create_surface(&qh),
            pool: None,
            buffer: None,
            key: None,
        };
        let surface = compositor.create_surface(&qh);
        let viewport = viewporter.get_viewport(&surface, &qh, ());
        let layer = shell.create_layer_surface(
            &qh,
            surface,
            Layer::Overlay,
            Some("babry"),
            output.as_ref().map(|binding| &binding.output),
        );
        layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
        layer.set_size(0, 0);
        layer.commit();
        let physical = (session.source.width(), session.source.height());
        let core = Core::new(CoreInit {
            source: session.source,
            physical,
            logical: session.logical,
            selection: session.selection,
            direct_edit: session.direct_edit,
            canvas: false,
            save_dir: session.save_dir,
            output: session.output,
            candidates: session.candidates,
        });
        let mut editor = Self {
            registry_state,
            seat_state,
            output_state,
            shm,
            layer,
            viewport,
            parts: Some((compositor, subcompositor, viewporter)),
            scene: None,
            soft: None,
            compositor: None,
            viewporter: None,
            shell,
            output_info: session.output_info,
            capture: None,
            core,
            pointer: None,
            keyboard: None,
            cursor_manager,
            cursor_device: None,
            cursor_serial: 0,
            cursor_shape: None,
            brush,
            text_input_manager,
            text_input: None,
            ime_focus: false,
            ime_enabled: false,
            ime_cursor: None,
            kde: Backend::detect().ok() == Some(Backend::Kde),
            ime_layer: false,
            ime_pending: (None, None),
            modifiers: Modifiers::default(),
            held: (false, false),
            configured: false,
            frame_pending: false,
            frame_requested: Instant::now(),
            started,
            presented: false,
            _output: output,
        };
        while !editor.core.exit {
            if editor.capture.is_some() {
                // 长截图期间定时醒来，刷新进度文字。
                let deadline = Instant::now() + Duration::from_millis(100);
                screencopy::dispatch_until(&mut queue, &mut editor, deadline)
                    .map_err(|error| format!("Wayland 事件循环失败: {error}"))?;
            } else {
                queue
                    .blocking_dispatch(&mut editor)
                    .map_err(|error| format!("Wayland 事件循环失败: {error}"))?;
            }
            editor.poll_longshot(&qh);
            editor.draw(&qh)?;
        }
        Ok(())
    }
}

impl NativeEditor {
    fn draw(&mut self, qh: &QueueHandle<Self>) -> Result<(), String> {
        self.core.poll_candidates();
        if !self.configured || self.core.exit || !self.core.dirty {
            return Ok(());
        }
        // 帧回调节流；合成器长时间不回调时也不让界面卡死。
        if self.frame_pending && self.frame_requested.elapsed() < Duration::from_millis(100) {
            return Ok(());
        }
        // 预编辑文字只用于显示，不进入标注历史。
        let committed_len = match self.core.pending.as_mut() {
            Some(mark) if self.core.text_editing && !self.core.preedit.is_empty() => {
                let len = mark.text.len();
                mark.text.push_str(&self.core.preedit);
                Some(len)
            }
            _ => None,
        };
        let caret = self.core.caret_rect();
        // 输入法光标使用屏幕坐标；长图画布需要减去视图偏移。
        let view = self.core.view;
        self.sync_ime(caret.map(|c| RectPx::new(c.x - view.0, c.y - view.1, c.width, c.height)));
        if self.soft.is_some() {
            let result = self.draw_soft(caret, qh);
            if let (Some(len), Some(mark)) = (committed_len, self.core.pending.as_mut()) {
                mark.text.truncate(len);
            }
            return result;
        }
        let Some(scene) = self.scene.as_mut() else {
            return Ok(());
        };
        for rect in self.core.damaged.drain(..) {
            scene.damage(rect);
        }
        let core = &mut self.core;
        let overlay = core.push_overlay();
        let has_pending = overlay.0;
        let result = scene.update(
            SceneInput {
                selection: core.selection,
                ui: &core.ui,
                marks: &core.marks,
                source: &core.source,
                fonts: &mut core.fonts,
                preview: has_pending,
                caret,
            },
            qh,
        );
        core.pop_overlay(overlay);
        if let (Some(len), Some(mark)) = (committed_len, self.core.pending.as_mut()) {
            mark.text.truncate(len);
        }
        let (changed, complete) = result?;
        self.core.dirty = !complete;
        if changed || !self.presented {
            let surface = self.layer.wl_surface();
            surface.frame(qh, surface.clone());
            self.layer.commit();
            self.frame_pending = true;
            self.frame_requested = Instant::now();
        }
        Ok(())
    }

    /// 处理工具栏长截图按钮的开始/停止请求，并刷新进度。
    fn poll_longshot(&mut self, qh: &QueueHandle<Self>) {
        match self.core.longshot.take() {
            Some(LongshotRequest::Start(selection)) => {
                let result = self.start_capture(selection, qh);
                self.core.report(result);
            }
            Some(LongshotRequest::Stop) => {
                let result = self.stop_capture();
                self.core.report(result);
            }
            None => {}
        }
        let Some(capture) = self.capture.as_mut() else {
            return;
        };
        if let Some(status) = capture.poll() {
            self.core.capture_status = status;
            self.core.dirty = true;
        }
        // 取帧线程出错退出：结束长截图并显示错误，编辑器保持原状。
        if capture.failed() {
            let result = self.stop_capture();
            self.core.report(result);
        }
    }

    fn start_capture(&mut self, selection: RectPx, qh: &QueueHandle<Self>) -> Result<(), String> {
        let info = self.output_info.as_ref().ok_or("长截图需要显示器信息")?;
        let capture = Capture::start(info, selection, self.core.physical)?;
        // 子表面场景显示的是冻结的截图；换成透明的整帧合成，露出下方实时窗口。
        if let Some(scene) = self.scene.take() {
            let (compositor, viewporter) = scene.destroy();
            self.compositor = Some(compositor);
            self.viewporter = Some(viewporter);
        }
        // 原主表面挂着不透明的截图缓冲区，合成器会据此跳过下方窗口的绘制；
        // 换一个新的透明主表面，旧表面随之销毁。
        let (Some(compositor), Some(viewporter)) = (&self.compositor, &self.viewporter) else {
            return Err("长截图需要重新创建表面".into());
        };
        let surface = compositor.create_surface(qh);
        let viewport = viewporter.get_viewport(&surface, qh, ());
        let layer = self.shell.create_layer_surface(
            qh,
            surface,
            Layer::Overlay,
            Some("babry"),
            self._output.as_ref().map(|binding| &binding.output),
        );
        layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer.set_size(0, 0);
        layer.commit();
        self.viewport.destroy();
        self.viewport = viewport;
        self.layer = layer;
        // 等新表面 configure 后再画。
        self.configured = false;
        self.frame_pending = false;
        let size = self.core.physical;
        let pool = SlotPool::new(size.0 as usize * size.1 as usize * 8, &self.shm)
            .map_err(|e| format!("无法创建画布共享内存: {e}"))?;
        self.soft = Some(SoftSurface {
            pool,
            buffers: Vec::new(),
            cache: SoftCache::default(),
        });
        self.core.capturing = true;
        self.core.ui.longshot = true;
        self.core.capture_status = capture.status();
        self.capture = Some(capture);
        // 不再抢键盘，用户也可以用 PageDown/空格滚动页面。
        self.layer
            .set_keyboard_interactivity(KeyboardInteractivity::None);
        self.core.dirty = true;
        Ok(())
    }

    fn stop_capture(&mut self) -> Result<(), String> {
        let Some(capture) = self.capture.take() else {
            return Ok(());
        };
        let result = capture
            .finish()
            .and_then(crate::longshot::into_image)
            .map(|image| self.core.enter_canvas(image));
        // 无论成功与否都恢复正常输入；失败时退回原截图的冻结画面。
        self.core.capturing = false;
        self.core.ui.longshot = false;
        self.layer
            .set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
        self.layer.wl_surface().set_input_region(None);
        if let Some(soft) = self.soft.as_mut() {
            soft.cache.invalidate();
        }
        self.core.dirty = true;
        result
    }

    /// 长截图期间只有工具栏接收指针，其余区域的滚轮和点击落到下方窗口。
    fn update_input_region(&mut self) {
        let (Some(compositor), true) = (self.compositor.as_ref(), self.core.capturing) else {
            return;
        };
        let Ok(region) = smithay_client_toolkit::compositor::Region::new(compositor) else {
            return;
        };
        let b = self
            .core
            .ui
            .toolbar(self.core.logical, self.core.ui_selection());
        region.add(
            b.x.floor() as i32,
            b.y.floor() as i32,
            b.width.ceil() as i32 + 1,
            b.height.ceil() as i32 + 1,
        );
        self.layer
            .wl_surface()
            .set_input_region(Some(region.wl_region()));
    }

    /// 长图画布：整帧软件合成到空闲缓冲区并提交到主表面。
    fn draw_soft(&mut self, caret: Option<RectPx>, qh: &QueueHandle<Self>) -> Result<(), String> {
        let size = self.core.physical;
        let Some(soft) = self.soft.as_mut() else {
            return Ok(());
        };
        let index = match soft
            .buffers
            .iter()
            .position(|b| b.canvas(&mut soft.pool).is_some())
        {
            Some(index) => index,
            None if soft.buffers.len() < 2 => {
                let (buffer, _) = soft
                    .pool
                    .create_buffer(
                        size.0 as i32,
                        size.1 as i32,
                        size.0 as i32 * 4,
                        wl_shm::Format::Argb8888,
                    )
                    .map_err(|e| format!("无法创建画布缓冲区: {e}"))?;
                soft.buffers.push(buffer);
                soft.buffers.len() - 1
            }
            // 两块都在合成器手里：等 release 或下一帧回调再画。
            None => return Ok(()),
        };
        let canvas = soft.buffers[index]
            .canvas(&mut soft.pool)
            .ok_or("画布缓冲区仍在使用")?;
        let core = &mut self.core;
        let overlay = core.push_overlay();
        let marks = std::mem::take(&mut core.marks);
        let result = soft_render::render(canvas, size, core, &marks, caret, &mut soft.cache);
        core.marks = marks;
        core.pop_overlay(overlay);
        result?;
        for pixel in canvas.chunks_exact_mut(4) {
            pixel.swap(0, 2);
        }
        self.update_input_region();
        let Some(soft) = self.soft.as_mut() else {
            return Ok(());
        };
        let surface = self.layer.wl_surface();
        soft.buffers[index]
            .attach_to(surface)
            .map_err(|e| format!("附加画布缓冲区失败: {e:?}"))?;
        surface.damage_buffer(0, 0, size.0 as i32, size.1 as i32);
        surface.frame(qh, surface.clone());
        self.layer.commit();
        self.frame_pending = true;
        self.frame_requested = Instant::now();
        self.core.dirty = false;
        Ok(())
    }

    /// 文字编辑期间启用输入法，并把光标矩形（表面逻辑坐标）告诉输入法定位候选窗。
    fn sync_ime(&mut self, caret: Option<RectPx>) {
        let lower = self.kde && self.core.text_editing && self.ime_focus;
        if lower != self.ime_layer {
            self.layer
                .set_layer(if lower { Layer::Top } else { Layer::Overlay });
            self.ime_layer = lower;
            // 同步主表面状态，输入法候选窗和状态提示始终显示在编辑器上方。
            self.layer.commit();
        }
        let Some(text_input) = &self.text_input else {
            return;
        };
        let want = self.core.text_editing && self.ime_focus;
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
        let scale = self.core.scale();
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

    fn update_cursor(&mut self, position: (f64, f64)) {
        let cursor = self.core.cursor_for(position);
        if let Cursor::Brush(diameter) = cursor
            && self
                .set_brush(diameter)
                .inspect_err(|e| eprintln!("babry: 画笔光标失败: {e}"))
                .is_ok()
        {
            self.cursor_shape = None;
            return;
        }
        self.brush.key = None;
        let shape = shape(cursor);
        if self.cursor_shape != Some(shape)
            && let Some(device) = &self.cursor_device
        {
            device.set_shape(self.cursor_serial, shape);
            self.cursor_shape = Some(shape);
        }
    }
}

impl NativeEditor {
    /// 把直径 `diameter`（物理像素）的圆环设为指针图像。光标表面只支持整数
    /// `buffer_scale`，因此按向上取整的倍率绘制，由合成器缩到分数缩放。
    fn set_brush(&mut self, diameter: u32) -> Result<(), String> {
        let pointer = self.pointer.as_ref().ok_or("没有指针")?;
        let scale = self.core.scale();
        let factor = scale.ceil().max(1.0) as i32;
        let key = (diameter, factor);
        if self.brush.key == Some(key) {
            return Ok(());
        }
        let logical = diameter as f32 / scale;
        let side = ((logical.ceil() as u32 + 8) & !1) as i32;
        let (width, stride) = (side * factor, side * factor * 4);
        if self.brush.pool.is_none() {
            self.brush.pool = Some(
                SlotPool::new(stride as usize * width as usize, &self.shm)
                    .map_err(|e| e.to_string())?,
            );
        }
        let pool = self.brush.pool.as_mut().ok_or("光标内存不可用")?;
        let (buffer, canvas) = pool
            .create_buffer(width, width, stride, wl_shm::Format::Argb8888)
            .map_err(|e| e.to_string())?;
        let image =
            babry::native_ui::brush_cursor(logical * factor as f32, width as u32, factor as f32)
                .ok_or("光标尺寸无效")?;
        canvas.copy_from_slice(image.data());
        let surface = &self.brush.surface;
        surface.set_buffer_scale(factor);
        buffer.attach_to(surface).map_err(|e| e.to_string())?;
        surface.damage_buffer(0, 0, width, width);
        surface.commit();
        pointer.set_cursor(self.cursor_serial, Some(surface), side / 2, side / 2);
        // 旧缓冲区在新缓冲区挂上后才释放，SlotPool 会在合成器 release 后回收。
        self.brush.buffer = Some(buffer);
        self.brush.key = Some(key);
        Ok(())
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
            self.core.ui.error = Some(error);
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
        self.sync_modifiers();
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
        self.sync_modifiers();
    }
}

impl NativeEditor {
    fn key(&mut self, event: KeyEvent) {
        match event.keysym {
            Keysym::Control_L | Keysym::Control_R => self.held.0 = true,
            Keysym::Shift_L | Keysym::Shift_R => self.held.1 = true,
            _ => {}
        }
        self.sync_modifiers();
        self.core.key(event.keysym, event.utf8.as_deref());
    }

    fn sync_modifiers(&mut self) {
        self.core.ctrl = self.modifiers.ctrl || self.held.0;
        self.core.shift = self.modifiers.shift || self.held.1;
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
            // 指针事件中的 Shift 状态影响直线吸附，先同步修饰键。
            self.sync_modifiers();
            match event.kind {
                PointerEventKind::Enter { serial } => {
                    self.cursor_serial = serial;
                    self.cursor_shape = None;
                    self.brush.key = None;
                    self.core.pointer_enter(event.position);
                }
                PointerEventKind::Leave { .. } => self.core.pointer_leave(),
                PointerEventKind::Press { button: LEFT, .. } => {
                    self.core.handle_press(event.position)
                }
                PointerEventKind::Press { button: RIGHT, .. } => self.core.right_press(),
                PointerEventKind::Release { button: LEFT, .. } => {
                    self.core.handle_release(event.position)
                }
                PointerEventKind::Motion { .. } => self.core.pointer_motion(event.position),
                PointerEventKind::Axis { vertical, .. } => {
                    let steps = if vertical.value120 != 0 {
                        vertical.value120.signum()
                    } else if vertical.discrete != 0 {
                        vertical.discrete.signum()
                    } else {
                        (vertical.absolute.signum()) as i32
                    };
                    self.core
                        .axis(event.position, steps, vertical.absolute * 4.0);
                }
                _ => {}
            }
            self.update_cursor(event.position);
        }
        if let Err(error) = self.draw(qh) {
            self.core.ui.error = Some(error);
        }
    }
}

impl LayerShellHandler for NativeEditor {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        self.core.exit = true;
    }
    fn configure(
        &mut self,
        connection: &Connection,
        qh: &QueueHandle<Self>,
        _: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _: u32,
    ) {
        if self.configured {
            return;
        }
        if self.soft.is_some() {
            self.viewport
                .set_destination(self.core.logical.0 as i32, self.core.logical.1 as i32);
            self.configured = true;
            self.core.dirty = true;
            let _ = connection;
            return;
        }
        let (w, h) = configure.new_size;
        if w > 0 && h > 0 {
            self.core.logical = (w, h);
        }
        self.viewport
            .set_destination(self.core.logical.0 as i32, self.core.logical.1 as i32);
        let Some((compositor, subcompositor, viewporter)) = self.parts.take() else {
            return;
        };
        match Scene::new(
            connection,
            compositor,
            subcompositor,
            viewporter,
            &self.shm,
            self.layer.wl_surface(),
            &self.core.source,
            self.core.logical,
            qh,
        ) {
            Ok(scene) => {
                self.core.ui.grid = scene.grid();
                self.scene = Some(scene);
                self.configured = true;
                self.core.dirty = true;
                if let Err(error) = self.draw(qh) {
                    eprintln!("babry: {error}");
                    self.core.exit = true;
                }
            }
            Err(error) => {
                eprintln!("babry: {error}");
                self.core.exit = true;
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
        qh: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event
            && let Some(scene) = state.scene.as_mut()
        {
            scene.tile_released(slot.0, qh);
            // 之前因槽位占用而推迟的瓦片在 release 后重试。
            state.core.dirty = true;
        }
    }
}

impl Dispatch<wl_callback::WlCallback, RetiredSlot> for NativeEditor {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        _: wl_callback::Event,
        slot: &RetiredSlot,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let Some(scene) = state.scene.as_mut() {
            scene.tile_retired(slot.0);
            state.core.dirty = true;
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
                state.core.dirty = true;
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
                state.core.preedit = preedit.unwrap_or_default();
                let commit = commit.unwrap_or_default();
                state.core.refresh_text(|mark| mark.text.push_str(&commit));
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
