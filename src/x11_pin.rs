//! X11 贴图：无边框的 override-redirect 置顶窗口。拖动移动、滚轮缩放、
//! 右键菜单（复制、另存为、关闭），Esc 关闭。
use crate::x11_editor::{Surface, create_window};
use babry::{
    compositor::{self, OutputInfo},
    core::RectPx,
    font::Fonts,
    native_pin::{MENU_ITEMS, notify, save_as},
    native_ui, x11,
};
use std::{os::fd::AsRawFd, path::Path};
use tiny_skia::{FilterQuality, Pixmap, PixmapPaint, Transform};
use x11rb::{
    connection::Connection,
    protocol::{
        Event,
        xproto::{self, ConnectionExt as _, EventMask},
    },
};

fn events() -> EventMask {
    EventMask::KEY_PRESS
        | EventMask::BUTTON_PRESS
        | EventMask::BUTTON_RELEASE
        | EventMask::POINTER_MOTION
        | EventMask::LEAVE_WINDOW
        | EventMask::EXPOSURE
}

fn load(path: &Path) -> Result<Pixmap, String> {
    let image = babry::shm_image::ShmImage::read_png(path)?;
    let mut pixmap = Pixmap::new(image.width(), image.height()).ok_or("贴图尺寸无效")?;
    let row = image.width() as usize * 4;
    for (y, line) in pixmap.data_mut().chunks_exact_mut(row).enumerate() {
        image.read_row(0, y as i32, line);
    }
    Ok(pixmap)
}

struct Menu {
    rect: RectPx,
    surface: Surface,
    hover: Option<usize>,
}

pub fn run(path: &Path, output: Option<String>) -> Result<(), String> {
    let pixmap = load(path)?;
    let outputs = compositor::outputs()?;
    let output: OutputInfo = compositor::select_output(&outputs, output.as_deref())?;
    let (conn, screen_index) = x11::connect()?;
    let screen = conn.setup().roots[screen_index].clone();
    // 1:1 显示，超出显示器时等比缩小，居中。
    let fit = (f64::from(output.width) / f64::from(pixmap.width()))
        .min(f64::from(output.height) / f64::from(pixmap.height()))
        .min(1.0);
    let base = (pixmap.width(), pixmap.height());
    let mut size = (
        ((f64::from(base.0) * fit).round() as u32).max(1),
        ((f64::from(base.1) * fit).round() as u32).max(1),
    );
    let mut origin = (
        output.x + (output.width.saturating_sub(size.0) / 2) as i32,
        output.y + (output.height.saturating_sub(size.1) / 2) as i32,
    );
    let window = create_window(
        &conn,
        &screen,
        RectPx::new(origin.0, origin.1, size.0, size.1),
        events(),
    )?;
    conn.map_window(window).map_err(|e| e.to_string())?;
    let mut surface = Surface::new(&conn, window, size)?;
    let mut fonts = Fonts::new();
    let mut menu: Option<Menu> = None;
    let mut drag: Option<((i16, i16), (i32, i32))> = None;
    let mut dirty = true;
    let fd = conn.stream().as_raw_fd();
    loop {
        while let Some(event) = conn.poll_for_event().map_err(|e| e.to_string())? {
            let in_menu = |w: xproto::Window, menu: &Option<Menu>| {
                menu.as_ref().is_some_and(|m| m.surface.window == w)
            };
            match event {
                Event::Expose(_) => dirty = true,
                Event::KeyPress(key) if key.detail == 9 => {
                    // 键码 9 为 Esc：先收起菜单，没有菜单时关闭贴图。
                    if let Some(m) = menu.take() {
                        let w = m.surface.window;
                        m.surface.destroy(&conn);
                        let _ = conn.destroy_window(w);
                    } else {
                        return Ok(());
                    }
                }
                Event::ButtonPress(b) if in_menu(b.event, &menu) => {
                    let index = native_ui::context_hit(
                        MENU_ITEMS.len(),
                        (f64::from(b.event_x), f64::from(b.event_y)),
                    );
                    if let Some(m) = menu.take() {
                        let w = m.surface.window;
                        m.surface.destroy(&conn);
                        let _ = conn.destroy_window(w);
                    }
                    let target = path.to_path_buf();
                    match index {
                        Some(0) => {
                            std::thread::spawn(move || {
                                if let Err(error) = babry::clipboard::copy_png(&target) {
                                    notify(&error);
                                }
                            });
                        }
                        Some(1) => {
                            std::thread::spawn(move || match save_as(&target) {
                                Ok(Some(saved)) => eprintln!("已另存为: {}", saved.display()),
                                Ok(None) => {}
                                Err(error) => notify(&error),
                            });
                        }
                        Some(_) => return Ok(()),
                        None => {}
                    }
                }
                Event::MotionNotify(m) if in_menu(m.event, &menu) => {
                    if let Some(open) = menu.as_mut() {
                        let hover = native_ui::context_hit(
                            MENU_ITEMS.len(),
                            (f64::from(m.event_x), f64::from(m.event_y)),
                        );
                        if hover != open.hover {
                            open.hover = hover;
                            draw_menu(&conn, open, &mut fonts)?;
                        }
                    }
                }
                // 只处理真正的移出；抓取开始/结束产生的 Leave 不算。
                Event::LeaveNotify(l)
                    if in_menu(l.event, &menu) && l.mode == xproto::NotifyMode::NORMAL =>
                {
                    if let Some(m) = menu.take() {
                        let w = m.surface.window;
                        m.surface.destroy(&conn);
                        let _ = conn.destroy_window(w);
                    }
                }
                Event::ButtonPress(b) => match b.detail {
                    1 => {
                        let _ = conn.set_input_focus(
                            xproto::InputFocus::PARENT,
                            window,
                            x11rb::CURRENT_TIME,
                        );
                        drag = Some(((b.root_x, b.root_y), origin));
                    }
                    3 => {
                        // 右键打开菜单，避免一次误触就关闭贴图。
                        let (w, h) = native_ui::context_menu_size(MENU_ITEMS.len());
                        let rect = RectPx::new(
                            i32::from(b.root_x).min(output.x + output.width as i32 - w as i32),
                            i32::from(b.root_y).min(output.y + output.height as i32 - h as i32),
                            w as u32,
                            h as u32,
                        );
                        let menu_window = create_window(&conn, &screen, rect, events())?;
                        conn.map_window(menu_window).map_err(|e| e.to_string())?;
                        let mut open = Menu {
                            rect,
                            surface: Surface::new(&conn, menu_window, (rect.width, rect.height))?,
                            hover: None,
                        };
                        draw_menu(&conn, &mut open, &mut fonts)?;
                        menu = Some(open);
                    }
                    4 | 5 => {
                        let factor = if b.detail == 4 { 1.1 } else { 1.0 / 1.1 };
                        let next = (
                            ((f64::from(size.0) * factor).round() as u32).clamp(32, output.width),
                            ((f64::from(size.1) * factor).round() as u32).clamp(32, output.height),
                        );
                        let center = (origin.0 + size.0 as i32 / 2, origin.1 + size.1 as i32 / 2);
                        size = next;
                        origin = (center.0 - size.0 as i32 / 2, center.1 - size.1 as i32 / 2);
                        conn.configure_window(
                            window,
                            &xproto::ConfigureWindowAux::new()
                                .x(origin.0)
                                .y(origin.1)
                                .width(size.0)
                                .height(size.1),
                        )
                        .map_err(|e| e.to_string())?;
                        let old =
                            std::mem::replace(&mut surface, Surface::new(&conn, window, size)?);
                        old.destroy(&conn);
                        dirty = true;
                    }
                    _ => {}
                },
                Event::MotionNotify(m) => {
                    if let Some((start, from)) = drag {
                        origin = (
                            from.0 + i32::from(m.root_x - start.0),
                            from.1 + i32::from(m.root_y - start.1),
                        );
                        conn.configure_window(
                            window,
                            &xproto::ConfigureWindowAux::new().x(origin.0).y(origin.1),
                        )
                        .map_err(|e| e.to_string())?;
                    }
                }
                Event::ButtonRelease(b) if b.detail == 1 => drag = None,
                _ => {}
            }
        }
        if dirty {
            let bytes = surface.image.bytes_mut();
            bytes.fill(0);
            let mut target =
                tiny_skia::PixmapMut::from_bytes(bytes, size.0, size.1).ok_or("贴图尺寸无效")?;
            target.draw_pixmap(
                0,
                0,
                pixmap.as_ref(),
                &PixmapPaint {
                    quality: FilterQuality::Bilinear,
                    ..PixmapPaint::default()
                },
                Transform::from_scale(size.0 as f32 / base.0 as f32, size.1 as f32 / base.1 as f32),
                None,
            );
            surface.present(&conn)?;
            dirty = false;
        }
        conn.flush().map_err(|e| e.to_string())?;
        let mut pollfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: pollfd 指向栈上的有效对象。
        unsafe { libc::poll(&mut pollfd, 1, 200) };
    }
}

fn draw_menu(
    conn: &x11rb::rust_connection::RustConnection,
    menu: &mut Menu,
    fonts: &mut Fonts,
) -> Result<(), String> {
    let size = (menu.rect.width, menu.rect.height);
    let bytes = menu.surface.image.bytes_mut();
    // 没有透明窗口：先铺面板底色，圆角外也是同色。
    for pixel in bytes.chunks_exact_mut(4) {
        pixel.copy_from_slice(&[24, 31, 44, 255]);
    }
    native_ui::draw_context_menu(
        bytes,
        size,
        Transform::identity(),
        (0.0, 0.0),
        &MENU_ITEMS,
        menu.hover,
        Some(MENU_ITEMS.len() - 1),
        fonts,
    )?;
    menu.surface.present(conn)
}
