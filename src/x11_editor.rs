//! X11 编辑器前端：覆盖整个显示器的 override-redirect 窗口，整帧软件合成后用
//! MIT-SHM（不可用时用 PutImage）显示。编辑逻辑与 Wayland 共用 `editor_core`。
//!
//! X11 没有逐窗口透明合成可依赖，长截图期间隐藏全屏窗口，只把工具栏、进度和
//! 边框放在几个小窗口里，选区内的输入自然落到下方窗口。
use crate::longshot::Capture;
use babry::{
    compositor::OutputInfo,
    core::RectPx,
    editor_core::{Core, CoreInit, Cursor, LongshotRequest},
    native_ui::{self, SHADOW_MARGIN},
    shm_image::ShmImage,
    smart_select,
    soft_render::{self, SoftCache},
    x11::{self, Keymap},
};
use std::{
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    path::PathBuf,
    sync::{Arc, mpsc::Receiver},
    time::Duration,
};
use x11rb::{
    connection::{Connection, RequestConnection},
    protocol::{
        Event,
        shm::ConnectionExt as _,
        xproto::{
            self, ConnectionExt as _, CreateGCAux, CreateWindowAux, EventMask, GrabMode,
            GrabStatus, ImageFormat, WindowClass,
        },
    },
    rust_connection::RustConnection,
};

pub struct Session {
    pub source: Arc<ShmImage>,
    pub output: OutputInfo,
    pub selection: RectPx,
    pub direct_edit: bool,
    pub save_dir: PathBuf,
    pub candidates: Option<Receiver<Vec<smart_select::Candidate>>>,
}

/// 光标字体中的字形编号（X11 cursorfont.h）。
fn glyph(cursor: Cursor) -> u16 {
    match cursor {
        Cursor::Default => 68,
        Cursor::Crosshair => 34,
        Cursor::Move => 52,
        Cursor::Grab | Cursor::Grabbing => 58,
        Cursor::Text => 152,
        Cursor::Resize(5) => 134,
        Cursor::Resize(6) => 136,
        Cursor::Resize(7) => 14,
        Cursor::Resize(8) => 12,
        Cursor::Resize(1) => 70,
        Cursor::Resize(2) => 138,
        Cursor::Resize(3) => 96,
        Cursor::Resize(_) => 16,
    }
}

/// 一块与 X 服务器共享的帧缓冲区；没有 MIT-SHM 时 `seg` 为 `None`。
pub struct Surface {
    pub window: xproto::Window,
    gc: xproto::Gcontext,
    pub image: ShmImage,
    seg: Option<u32>,
    size: (u32, u32),
}

impl Surface {
    pub fn new(
        conn: &RustConnection,
        window: xproto::Window,
        size: (u32, u32),
    ) -> Result<Self, String> {
        let gc = conn.generate_id().map_err(|e| e.to_string())?;
        conn.create_gc(gc, window, &CreateGCAux::new().graphics_exposures(0))
            .map_err(|e| e.to_string())?;
        let image = ShmImage::new(size.0.max(1), size.1.max(1), size.0.max(1) * 4)?;
        let shm = conn
            .shm_query_version()
            .ok()
            .and_then(|c| c.reply().ok())
            .is_some_and(|v| (v.major_version, v.minor_version) >= (1, 2));
        let mut seg = None;
        if shm {
            let id = conn.generate_id().map_err(|e| e.to_string())?;
            // SAFETY: dup 的新 fd 只交给 OwnedFd 并随请求发给服务器。
            let fd = unsafe { libc::dup(image.fd().as_raw_fd()) };
            if fd >= 0 {
                let fd = unsafe { OwnedFd::from_raw_fd(fd) };
                if conn
                    .shm_attach_fd(id, fd, true)
                    .ok()
                    .and_then(|c| c.check().ok())
                    .is_some()
                {
                    seg = Some(id);
                }
            }
        }
        Ok(Self {
            window,
            gc,
            image,
            seg,
            size,
        })
    }

    /// 把 RGBA 帧（预乘）转换为 X 的 BGRX 并显示；等服务器读完再返回，
    /// 下一帧才能安全地覆盖共享内存。
    pub fn present(&mut self, conn: &RustConnection) -> Result<(), String> {
        for pixel in self.image.bytes_mut().chunks_exact_mut(4) {
            pixel.swap(0, 2);
        }
        let (w, h) = (self.size.0 as u16, self.size.1 as u16);
        let depth = conn.setup().roots[0].root_depth;
        if let Some(seg) = self.seg {
            conn.shm_put_image(
                self.window,
                self.gc,
                w,
                h,
                0,
                0,
                w,
                h,
                0,
                0,
                depth,
                ImageFormat::Z_PIXMAP.into(),
                false,
                seg,
                0,
            )
            .map_err(|e| e.to_string())?;
        } else {
            let row = self.size.0 as usize * 4;
            let rows = ((conn.maximum_request_bytes() - 64) / row).clamp(1, self.size.1 as usize);
            let bytes = &self.image.bytes_mut()[..];
            let mut y = 0;
            while y < self.size.1 as usize {
                let n = rows.min(self.size.1 as usize - y);
                conn.put_image(
                    ImageFormat::Z_PIXMAP,
                    self.window,
                    self.gc,
                    w,
                    n as u16,
                    0,
                    y as i16,
                    0,
                    depth,
                    &bytes[y * row..(y + n) * row],
                )
                .map_err(|e| e.to_string())?;
                y += n;
            }
        }
        conn.get_input_focus()
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn destroy(self, conn: &RustConnection) {
        if let Some(seg) = self.seg {
            let _ = conn.shm_detach(seg);
        }
        let _ = conn.free_gc(self.gc);
    }
}

pub fn create_window(
    conn: &RustConnection,
    root: &xproto::Screen,
    rect: RectPx,
    events: EventMask,
) -> Result<xproto::Window, String> {
    let window = conn.generate_id().map_err(|e| e.to_string())?;
    conn.create_window(
        root.root_depth,
        window,
        root.root,
        rect.x as i16,
        rect.y as i16,
        rect.width.max(1) as u16,
        rect.height.max(1) as u16,
        0,
        WindowClass::INPUT_OUTPUT,
        root.root_visual,
        &CreateWindowAux::new()
            .override_redirect(1)
            .background_pixel(root.black_pixel)
            .event_mask(events),
    )
    .map_err(|e| e.to_string())?;
    Ok(window)
}

/// 长截图期间代替全屏窗口显示的一小块界面（工具栏或进度标签）。
struct Patch {
    rect: RectPx,
    surface: Surface,
}

struct Editor {
    conn: RustConnection,
    screen: xproto::Screen,
    output: OutputInfo,
    window: xproto::Window,
    surface: Option<Surface>,
    core: Core,
    cache: SoftCache,
    keymap: Keymap,
    cursor_font: xproto::Font,
    cursors: Vec<(u16, xproto::Cursor)>,
    cursor: Option<u16>,
    capture: Option<Capture>,
    /// 长截图开始时截下的屏幕，用作小窗口圆角和阴影下的底色。
    under: Option<ShmImage>,
    patches: Vec<Patch>,
    borders: Vec<xproto::Window>,
    scratch: Vec<u8>,
}

fn events() -> EventMask {
    EventMask::KEY_PRESS
        | EventMask::KEY_RELEASE
        | EventMask::BUTTON_PRESS
        | EventMask::BUTTON_RELEASE
        | EventMask::POINTER_MOTION
        | EventMask::ENTER_WINDOW
        | EventMask::LEAVE_WINDOW
        | EventMask::EXPOSURE
}

pub fn run(session: Session) -> Result<(), String> {
    let (conn, screen_index) = x11::connect()?;
    let screen = conn.setup().roots[screen_index].clone();
    let output = session.output;
    let rect = RectPx::new(output.x, output.y, output.width, output.height);
    let window = create_window(&conn, &screen, rect, events())?;
    let cursor_font = conn.generate_id().map_err(|e| e.to_string())?;
    conn.open_font(cursor_font, b"cursor")
        .map_err(|e| e.to_string())?;
    let physical = (session.source.width(), session.source.height());
    let core = Core::new(CoreInit {
        source: session.source,
        physical,
        logical: (output.width, output.height),
        selection: session.selection,
        direct_edit: session.direct_edit,
        canvas: false,
        save_dir: session.save_dir,
        output: Some(output.name.clone()),
        candidates: session.candidates,
    });
    let keymap = Keymap::load(&conn)?;
    let mut editor = Editor {
        surface: Some(Surface::new(&conn, window, (output.width, output.height))?),
        conn,
        screen,
        output,
        window,
        core,
        cache: SoftCache::default(),
        keymap,
        cursor_font,
        cursors: Vec::new(),
        cursor: None,
        capture: None,
        under: None,
        patches: Vec::new(),
        borders: Vec::new(),
        scratch: Vec::new(),
    };
    editor.show_main()?;
    let result = editor.event_loop();
    editor.close();
    result
}

impl Editor {
    /// 显示全屏窗口并抢占键盘和指针；窗口管理器可能短暂占着抓取，重试一会儿。
    fn show_main(&mut self) -> Result<(), String> {
        let conn = &self.conn;
        conn.map_window(self.window).map_err(|e| e.to_string())?;
        conn.configure_window(
            self.window,
            &xproto::ConfigureWindowAux::new().stack_mode(xproto::StackMode::ABOVE),
        )
        .map_err(|e| e.to_string())?;
        conn.flush().map_err(|e| e.to_string())?;
        for _ in 0..50 {
            let keyboard = conn
                .grab_keyboard(
                    true,
                    self.window,
                    x11rb::CURRENT_TIME,
                    GrabMode::ASYNC,
                    GrabMode::ASYNC,
                )
                .ok()
                .and_then(|c| c.reply().ok())
                .is_some_and(|r| r.status == GrabStatus::SUCCESS);
            let mask = EventMask::BUTTON_PRESS
                | EventMask::BUTTON_RELEASE
                | EventMask::POINTER_MOTION
                | EventMask::ENTER_WINDOW
                | EventMask::LEAVE_WINDOW;
            let pointer = conn
                .grab_pointer(
                    true,
                    self.window,
                    mask,
                    GrabMode::ASYNC,
                    GrabMode::ASYNC,
                    x11rb::NONE,
                    x11rb::NONE,
                    x11rb::CURRENT_TIME,
                )
                .ok()
                .and_then(|c| c.reply().ok())
                .is_some_and(|r| r.status == GrabStatus::SUCCESS);
            if keyboard && pointer {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = conn.set_input_focus(
            xproto::InputFocus::POINTER_ROOT,
            self.window,
            x11rb::CURRENT_TIME,
        );
        self.core.dirty = true;
        self.cursor = None;
        Ok(())
    }

    fn close(&mut self) {
        let _ = self.conn.ungrab_keyboard(x11rb::CURRENT_TIME);
        let _ = self.conn.ungrab_pointer(x11rb::CURRENT_TIME);
        self.clear_capture_ui();
        if let Some(surface) = self.surface.take() {
            surface.destroy(&self.conn);
        }
        let _ = self.conn.destroy_window(self.window);
        let _ = self.conn.flush();
    }

    fn event_loop(&mut self) -> Result<(), String> {
        let fd = self.conn.stream().as_raw_fd();
        while !self.core.exit {
            while let Some(event) = self.conn.poll_for_event().map_err(|e| e.to_string())? {
                self.event(event);
            }
            self.core.poll_candidates();
            self.poll_longshot();
            if self.core.exit {
                break;
            }
            if self.core.dirty {
                let result = self.draw();
                if let Err(error) = result {
                    self.core.ui.error = Some(error);
                }
            }
            self.conn.flush().map_err(|e| e.to_string())?;
            let mut pollfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: pollfd 指向栈上的有效对象。
            unsafe { libc::poll(&mut pollfd, 1, 50) };
        }
        Ok(())
    }

    /// 事件坐标换算为输出内坐标：小窗口的事件加上窗口原点。
    fn position(&self, window: xproto::Window, x: i16, y: i16) -> (f64, f64) {
        let origin = self
            .patches
            .iter()
            .find(|p| p.surface.window == window)
            .map_or((0, 0), |p| (p.rect.x, p.rect.y));
        (
            f64::from(x) + f64::from(origin.0),
            f64::from(y) + f64::from(origin.1),
        )
    }

    fn modifiers(&mut self, state: u16) {
        self.core.ctrl = state & u16::from(xproto::KeyButMask::CONTROL) != 0;
        self.core.shift = state & u16::from(xproto::KeyButMask::SHIFT) != 0;
    }

    fn event(&mut self, event: Event) {
        match event {
            Event::Expose(_) => self.core.dirty = true,
            Event::KeyPress(key) => {
                self.modifiers(key.state.into());
                let (keysym, text) = self.keymap.lookup(key.detail, key.state.into());
                self.core.key(keysym, text.as_deref());
            }
            Event::KeyRelease(key) => self.modifiers(key.state.into()),
            Event::ButtonPress(button) => {
                self.modifiers(button.state.into());
                let p = self.position(button.event, button.event_x, button.event_y);
                match button.detail {
                    1 => self.core.handle_press(p),
                    3 => self.core.right_press(),
                    // X11 用 4/5 号按钮表示滚轮上/下。
                    4 | 5 => {
                        let steps = if button.detail == 5 { 1 } else { -1 };
                        self.core.axis(p, steps, f64::from(steps) * 60.0);
                    }
                    _ => {}
                }
            }
            Event::ButtonRelease(button) if button.detail == 1 => {
                let p = self.position(button.event, button.event_x, button.event_y);
                self.core.handle_release(p);
            }
            Event::MotionNotify(motion) => {
                self.modifiers(motion.state.into());
                let p = self.position(motion.event, motion.event_x, motion.event_y);
                self.core.pointer_motion(p);
                self.update_cursor(motion.event, p);
            }
            Event::EnterNotify(enter) => {
                let p = self.position(enter.event, enter.event_x, enter.event_y);
                self.core.pointer_enter(p);
            }
            Event::LeaveNotify(_) => self.core.pointer_leave(),
            _ => {}
        }
    }

    fn update_cursor(&mut self, window: xproto::Window, position: (f64, f64)) {
        let glyph = if self.core.capturing {
            68
        } else {
            glyph(self.core.cursor_for(position))
        };
        if self.cursor == Some(glyph) && window == self.window {
            return;
        }
        let cursor = match self.cursors.iter().find(|(g, _)| *g == glyph) {
            Some(&(_, cursor)) => cursor,
            None => {
                let Ok(id) = self.conn.generate_id() else {
                    return;
                };
                let _ = self.conn.create_glyph_cursor(
                    id,
                    self.cursor_font,
                    self.cursor_font,
                    glyph,
                    glyph + 1,
                    0,
                    0,
                    0,
                    0xffff,
                    0xffff,
                    0xffff,
                );
                self.cursors.push((glyph, id));
                id
            }
        };
        let _ = self.conn.change_window_attributes(
            window,
            &xproto::ChangeWindowAttributesAux::new().cursor(cursor),
        );
        if window == self.window {
            self.cursor = Some(glyph);
        }
    }
}

impl Editor {
    fn draw(&mut self) -> Result<(), String> {
        let size = (self.output.width, self.output.height);
        let caret = self.core.caret_rect();
        let core = &mut self.core;
        let pending = core.pending.take();
        let has_pending = pending.is_some();
        core.marks.extend(pending);
        let marks = std::mem::take(&mut core.marks);
        let result = if core.capturing {
            self.scratch
                .resize(size.0 as usize * size.1 as usize * 4, 0);
            soft_render::render(
                &mut self.scratch,
                size,
                core,
                &marks,
                caret,
                &mut self.cache,
            )
        } else if let Some(surface) = self.surface.as_mut() {
            soft_render::render(
                surface.image.bytes_mut(),
                size,
                core,
                &marks,
                caret,
                &mut self.cache,
            )
        } else {
            Ok(())
        };
        core.marks = marks;
        if has_pending {
            core.pending = core.marks.pop();
        }
        result?;
        self.core.dirty = false;
        if self.core.capturing {
            return self.draw_patches();
        }
        if let Some(surface) = self.surface.as_mut() {
            surface.present(&self.conn)?;
        }
        Ok(())
    }

    /// 长截图期间：把整帧中工具栏和进度标签所在的部分叠在原屏幕上，分别显示到小窗口。
    fn draw_patches(&mut self) -> Result<(), String> {
        let (w, h) = (self.output.width as i32, self.output.height as i32);
        let s = self.core.ui_selection();
        let toolbar = self.core.ui.toolbar(self.core.logical, s);
        let label = native_ui::label_box(s, self.core.ui.grid);
        let label_width = label.width.max(
            self.core
                .fonts
                .measure(&self.core.capture_status, 12.0, f32::INFINITY)
                .0
                + 20.0,
        );
        let rects: Vec<RectPx> = [
            (
                toolbar.x - SHADOW_MARGIN,
                toolbar.y - SHADOW_MARGIN,
                toolbar.width + SHADOW_MARGIN * 2.0,
                toolbar.height + SHADOW_MARGIN * 2.0,
            ),
            (label.x, label.y, label_width, label.height),
        ]
        .into_iter()
        .filter_map(|(x, y, rw, rh)| {
            let rect = RectPx::new(
                x.floor() as i32,
                y.floor() as i32,
                rw.ceil() as u32,
                rh.ceil() as u32,
            );
            babry::marks::intersect(rect, RectPx::new(0, 0, w as u32, h as u32))
        })
        .collect();
        if self
            .patches
            .iter()
            .map(|p| p.rect)
            .ne(rects.iter().copied())
        {
            for patch in self.patches.drain(..) {
                let window = patch.surface.window;
                patch.surface.destroy(&self.conn);
                let _ = self.conn.destroy_window(window);
            }
            for rect in &rects {
                let screen_rect = RectPx::new(
                    self.output.x + rect.x,
                    self.output.y + rect.y,
                    rect.width,
                    rect.height,
                );
                let window = create_window(&self.conn, &self.screen, screen_rect, events())?;
                self.conn.map_window(window).map_err(|e| e.to_string())?;
                self.patches.push(Patch {
                    rect: *rect,
                    surface: Surface::new(&self.conn, window, (rect.width, rect.height))?,
                });
            }
        }
        let row = self.output.width as usize * 4;
        let mut under = vec![0u8; row];
        for patch in &mut self.patches {
            let r = patch.rect;
            let out_row = r.width as usize * 4;
            let bytes = patch.surface.image.bytes_mut();
            for y in 0..r.height as usize {
                let sy = r.y as usize + y;
                if let Some(image) = &self.under {
                    image.read_row(r.x, sy as i32, &mut under[..out_row]);
                } else {
                    under[..out_row].fill(0);
                }
                let src = &self.scratch
                    [sy * row + r.x as usize * 4..sy * row + r.x as usize * 4 + out_row];
                let dst = &mut bytes[y * out_row..(y + 1) * out_row];
                // 预乘 alpha 叠加：out = src + under × (1 − α)。
                for ((d, s), u) in dst
                    .chunks_exact_mut(4)
                    .zip(src.chunks_exact(4))
                    .zip(under.chunks_exact(4))
                {
                    let inv = 255 - u32::from(s[3]);
                    for c in 0..3 {
                        d[c] = (u32::from(s[c]) + u32::from(u[c]) * inv / 255).min(255) as u8;
                    }
                    d[3] = 255;
                }
            }
            patch.surface.present(&self.conn)?;
        }
        Ok(())
    }

    fn poll_longshot(&mut self) {
        match self.core.longshot.take() {
            Some(LongshotRequest::Start(selection)) => {
                let result = self.start_capture(selection);
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
        if capture.failed() {
            let result = self.stop_capture();
            self.core.report(result);
        }
    }

    fn start_capture(&mut self, selection: RectPx) -> Result<(), String> {
        // 先隐藏全屏窗口，让根窗口露出真实内容，再开始取帧。
        let _ = self.conn.ungrab_keyboard(x11rb::CURRENT_TIME);
        let _ = self.conn.ungrab_pointer(x11rb::CURRENT_TIME);
        self.conn
            .unmap_window(self.window)
            .map_err(|e| e.to_string())?;
        self.conn.flush().map_err(|e| e.to_string())?;
        std::thread::sleep(Duration::from_millis(80));
        let full = RectPx::new(
            self.output.x,
            self.output.y,
            self.output.width,
            self.output.height,
        );
        self.under = x11::capture(&self.conn, self.screen.root, full).ok();
        let capture = match Capture::start(&self.output, selection, self.core.physical) {
            Ok(capture) => capture,
            Err(error) => {
                self.show_main()?;
                return Err(error);
            }
        };
        self.core.capturing = true;
        self.core.ui.longshot = true;
        self.core.capture_status = capture.status();
        self.capture = Some(capture);
        // 选区外侧的四条边框，留 1 像素间隙，不会被截进结果。
        let (gap, t) = (1, 2);
        let (x, y) = (self.output.x + selection.x, self.output.y + selection.y);
        let (sw, sh) = (selection.width as i32, selection.height as i32);
        for (bx, by, bw, bh) in [
            (x - gap - t, y - gap - t, sw + 2 * (gap + t), t),
            (x - gap - t, y + sh + gap, sw + 2 * (gap + t), t),
            (x - gap - t, y - gap, t, sh + 2 * gap),
            (x + sw + gap, y - gap, t, sh + 2 * gap),
        ] {
            let window = self.conn.generate_id().map_err(|e| e.to_string())?;
            self.conn
                .create_window(
                    self.screen.root_depth,
                    window,
                    self.screen.root,
                    bx as i16,
                    by as i16,
                    bw.max(1) as u16,
                    bh.max(1) as u16,
                    0,
                    WindowClass::INPUT_OUTPUT,
                    self.screen.root_visual,
                    &CreateWindowAux::new()
                        .override_redirect(1)
                        .background_pixel(0x0055_aaff),
                )
                .map_err(|e| e.to_string())?;
            self.conn.map_window(window).map_err(|e| e.to_string())?;
            self.borders.push(window);
        }
        self.core.dirty = true;
        Ok(())
    }

    fn clear_capture_ui(&mut self) {
        for patch in self.patches.drain(..) {
            let window = patch.surface.window;
            patch.surface.destroy(&self.conn);
            let _ = self.conn.destroy_window(window);
        }
        for window in self.borders.drain(..) {
            let _ = self.conn.destroy_window(window);
        }
        self.under = None;
    }

    fn stop_capture(&mut self) -> Result<(), String> {
        let Some(capture) = self.capture.take() else {
            return Ok(());
        };
        let result = capture
            .finish()
            .and_then(crate::longshot::into_image)
            .map(|image| self.core.enter_canvas(image));
        self.core.capturing = false;
        self.core.ui.longshot = false;
        self.clear_capture_ui();
        self.cache.invalidate();
        self.show_main()?;
        result
    }
}
