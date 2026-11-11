//! X11 会话支持：显示器信息、截图、按键表与剪贴板服务。
//!
//! 截图优先走 MIT-SHM：服务器把根窗口像素直接写进与 `ShmImage` 共用的 memfd，
//! 与 Wayland screencopy 一样不经过额外复制；没有 MIT-SHM 时退回 GetImage。
use crate::{compositor::OutputInfo, core::RectPx, shm_image::ShmImage};
use smithay_client_toolkit::seat::keyboard::Keysym;
use std::{os::fd::AsRawFd, path::Path};
use x11rb::{
    connection::{Connection, RequestConnection},
    protocol::{
        randr::ConnectionExt as _,
        shm::ConnectionExt as _,
        xproto::{self, ConnectionExt as _, ImageFormat},
    },
    rust_connection::RustConnection,
    wrapper::ConnectionExt as _,
};

pub fn connect() -> Result<(RustConnection, usize), String> {
    x11rb::connect(None).map_err(|error| format!("无法连接 X11 显示: {error}"))
}

/// 用 RandR 1.5 的显示器列表；不可用时把整个根窗口当作一个显示器。
/// X11 没有逐显示器缩放，逻辑尺寸等于物理尺寸。指针所在的显示器标为焦点。
pub fn outputs() -> Result<Vec<OutputInfo>, String> {
    let (conn, screen) = connect()?;
    let root = conn.setup().roots[screen].clone();
    let pointer = conn
        .query_pointer(root.root)
        .ok()
        .and_then(|c| c.reply().ok())
        .map(|p| (i32::from(p.root_x), i32::from(p.root_y)));
    let mut outputs = Vec::new();
    if let Ok(reply) = conn
        .randr_get_monitors(root.root, true)
        .map_err(|e| e.to_string())
        .and_then(|c| c.reply().map_err(|e| e.to_string()))
    {
        for monitor in reply.monitors {
            let name = conn
                .get_atom_name(monitor.name)
                .ok()
                .and_then(|c| c.reply().ok())
                .map(|r| String::from_utf8_lossy(&r.name).into_owned())
                .unwrap_or_else(|| format!("X11-{}", outputs.len()));
            outputs.push(OutputInfo {
                name,
                x: i32::from(monitor.x),
                y: i32::from(monitor.y),
                width: u32::from(monitor.width),
                height: u32::from(monitor.height),
                scale: 1.0,
                focused: false,
            });
        }
    }
    if outputs.is_empty() {
        outputs.push(OutputInfo {
            name: "X11".into(),
            x: 0,
            y: 0,
            width: u32::from(root.width_in_pixels),
            height: u32::from(root.height_in_pixels),
            scale: 1.0,
            focused: false,
        });
    }
    let focused = pointer
        .and_then(|(x, y)| {
            outputs.iter().position(|o| {
                x >= o.x && y >= o.y && x < o.x + o.width as i32 && y < o.y + o.height as i32
            })
        })
        .unwrap_or(0);
    outputs[focused].focused = true;
    Ok(outputs)
}

/// 截取根窗口中的一个矩形（根窗口坐标），结果为不透明 BGRA。
pub fn capture(
    conn: &RustConnection,
    root: xproto::Window,
    rect: RectPx,
) -> Result<ShmImage, String> {
    let (w, h) = (rect.width as u16, rect.height as u16);
    let mut image = ShmImage::new(rect.width, rect.height, rect.width * 4)?;
    let shm = conn
        .shm_query_version()
        .ok()
        .and_then(|c| c.reply().ok())
        .is_some_and(|v| (v.major_version, v.minor_version) >= (1, 2));
    let mut done = false;
    if shm {
        // memfd 复制一份 fd 交给服务器；服务器写入后立即分离。
        let seg = conn.generate_id().map_err(|e| e.to_string())?;
        // SAFETY: dup 返回的新 fd 由 OwnedFd 接管并随请求发送给服务器。
        let fd = unsafe { libc::dup(image.fd().as_raw_fd()) };
        if fd >= 0 {
            // SAFETY: fd 刚由 dup 返回，此处唯一持有。
            let owned =
                unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(fd) };
            if conn
                .shm_attach_fd(seg, owned, false)
                .ok()
                .and_then(|c| c.check().ok())
                .is_some()
            {
                done = conn
                    .shm_get_image(
                        root,
                        rect.x as i16,
                        rect.y as i16,
                        w,
                        h,
                        !0,
                        ImageFormat::Z_PIXMAP.into(),
                        seg,
                        0,
                    )
                    .ok()
                    .and_then(|c| c.reply().ok())
                    .is_some();
                let _ = conn.shm_detach(seg);
            }
        }
    }
    if !done {
        // GetImage 单次请求有大小上限，按行分块读取。
        let max = (conn.maximum_request_bytes() / 4).max(4096) as u32;
        let rows = (max / rect.width.max(1)).clamp(1, rect.height);
        let mut y = 0;
        while y < rect.height {
            let n = rows.min(rect.height - y);
            let reply = conn
                .get_image(
                    ImageFormat::Z_PIXMAP,
                    root,
                    rect.x as i16,
                    (rect.y + y as i32) as i16,
                    w,
                    n as u16,
                    !0,
                )
                .map_err(|e| e.to_string())?
                .reply()
                .map_err(|e| format!("X11 截图失败: {e}"))?;
            let len = (rect.width * 4 * n) as usize;
            let start = (y * rect.width * 4) as usize;
            let data = &reply.data;
            if data.len() < len {
                return Err("X11 截图数据不完整（仅支持 24/32 位色深）".into());
            }
            image.bytes_mut()[start..start + len].copy_from_slice(&data[..len]);
            y += n;
        }
    }
    // 24 位色深的 X 像素第四字节无定义，统一设为不透明。
    for pixel in image.bytes_mut().chunks_exact_mut(4) {
        pixel[3] = 255;
    }
    Ok(image)
}

/// 服务器的按键表：键码 → 各层 keysym。
pub struct Keymap {
    min: u8,
    per: usize,
    syms: Vec<u32>,
}

impl Keymap {
    pub fn load(conn: &RustConnection) -> Result<Self, String> {
        let setup = conn.setup();
        let (min, max) = (setup.min_keycode, setup.max_keycode);
        let reply = conn
            .get_keyboard_mapping(min, max - min + 1)
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|e| e.to_string())?;
        Ok(Self {
            min,
            per: usize::from(reply.keysyms_per_keycode).max(1),
            syms: reply.keysyms,
        })
    }

    /// 按 Shift/CapsLock 选层，返回 keysym 与对应的文字。
    pub fn lookup(&self, keycode: u8, state: u16) -> (Keysym, Option<String>) {
        let index = usize::from(keycode.saturating_sub(self.min)) * self.per;
        let base = self.syms.get(index).copied().unwrap_or(0);
        let shifted = self
            .syms
            .get(index + 1)
            .copied()
            .filter(|&s| s != 0)
            .unwrap_or(base);
        let shift = state & u16::from(xproto::KeyButMask::SHIFT) != 0;
        let caps = state & u16::from(xproto::KeyButMask::LOCK) != 0;
        let letter = Keysym::new(base)
            .key_char()
            .is_some_and(char::is_alphabetic);
        let raw = if shift ^ (caps && letter) {
            shifted
        } else {
            base
        };
        let keysym = Keysym::new(raw);
        let text = keysym
            .key_char()
            .filter(|c| !c.is_control())
            .map(String::from);
        (keysym, text)
    }
}

/// 剪贴板服务进程：持有 CLIPBOARD，按需提供 `image/png`，被其他程序取代后退出。
/// 大于单次请求上限的数据使用 INCR 分块传输。
pub fn serve_clipboard(path: &Path) -> Result<(), String> {
    use x11rb::protocol::{
        Event,
        xproto::{AtomEnum, EventMask, PropMode, SelectionNotifyEvent, WindowClass},
    };
    let data = std::fs::read(path).map_err(|e| format!("无法读取图片: {e}"))?;
    let (conn, screen) = connect()?;
    let root = conn.setup().roots[screen].root;
    let atom = |name: &str| -> Result<u32, String> {
        Ok(conn
            .intern_atom(false, name.as_bytes())
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|e| e.to_string())?
            .atom)
    };
    let (clipboard, targets, png, incr) = (
        atom("CLIPBOARD")?,
        atom("TARGETS")?,
        atom("image/png")?,
        atom("INCR")?,
    );
    let window = conn.generate_id().map_err(|e| e.to_string())?;
    conn.create_window(
        0,
        window,
        root,
        0,
        0,
        1,
        1,
        0,
        WindowClass::INPUT_ONLY,
        0,
        &xproto::CreateWindowAux::new(),
    )
    .map_err(|e| e.to_string())?;
    conn.set_selection_owner(window, clipboard, x11rb::CURRENT_TIME)
        .map_err(|e| e.to_string())?;
    let owner = conn
        .get_selection_owner(clipboard)
        .map_err(|e| e.to_string())?
        .reply()
        .map_err(|e| e.to_string())?
        .owner;
    if owner != window {
        return Err("无法取得 X11 剪贴板".into());
    }
    conn.flush().map_err(|e| e.to_string())?;
    println!("ready");
    use std::io::Write;
    let _ = std::io::stdout().flush();
    let chunk = (conn.maximum_request_bytes() / 2).max(65536);
    // 正在进行的 INCR 传输：(请求方窗口, 属性, 已发送字节)。
    let mut transfers: Vec<(u32, u32, usize)> = Vec::new();
    loop {
        let event = conn.wait_for_event().map_err(|e| e.to_string())?;
        match event {
            Event::SelectionClear(_) => return Ok(()),
            Event::SelectionRequest(request) => {
                let property = if request.property == x11rb::NONE {
                    request.target
                } else {
                    request.property
                };
                let ok = if request.target == targets {
                    let list = [targets, png];
                    conn.change_property32(
                        PropMode::REPLACE,
                        request.requestor,
                        property,
                        AtomEnum::ATOM,
                        &list,
                    )
                    .is_ok()
                } else if request.target == png {
                    if data.len() <= chunk {
                        conn.change_property8(
                            PropMode::REPLACE,
                            request.requestor,
                            property,
                            png,
                            &data,
                        )
                        .is_ok()
                    } else {
                        let _ = conn.change_window_attributes(
                            request.requestor,
                            &xproto::ChangeWindowAttributesAux::new()
                                .event_mask(EventMask::PROPERTY_CHANGE),
                        );
                        let ok = conn
                            .change_property32(
                                PropMode::REPLACE,
                                request.requestor,
                                property,
                                incr,
                                &[data.len() as u32],
                            )
                            .is_ok();
                        transfers.push((request.requestor, property, 0));
                        ok
                    }
                } else {
                    false
                };
                let notify = SelectionNotifyEvent {
                    response_type: xproto::SELECTION_NOTIFY_EVENT,
                    sequence: 0,
                    time: request.time,
                    requestor: request.requestor,
                    selection: request.selection,
                    target: request.target,
                    property: if ok { property } else { x11rb::NONE },
                };
                let _ = conn.send_event(false, request.requestor, EventMask::NO_EVENT, notify);
                let _ = conn.flush();
            }
            // INCR：请求方每删除一次属性，就写入下一块；最后写入空块表示结束。
            Event::PropertyNotify(event) if event.state == xproto::Property::DELETE => {
                if let Some(index) = transfers
                    .iter()
                    .position(|t| t.0 == event.window && t.1 == event.atom)
                {
                    let (requestor, property, sent) = transfers[index];
                    let end = (sent + chunk).min(data.len());
                    let _ = conn.change_property8(
                        PropMode::REPLACE,
                        requestor,
                        property,
                        png,
                        &data[sent..end],
                    );
                    if sent == end {
                        transfers.remove(index);
                    } else {
                        transfers[index].2 = end;
                    }
                    let _ = conn.flush();
                }
            }
            _ => {}
        }
    }
}
