//! wlroots screencopy：合成器直接写入编辑器随后显示和导出的同一块 memfd。
//!
//! 截图完成后只在原地规范通道顺序，不复制成第二份 RGBA 图像，也不经过 PNG。
use crate::shm_image::{ChannelOrder, ShmImage};
use smithay_client_toolkit::reexports::{
    client::{
        Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum, delegate_noop,
        globals::registry_queue_init,
        protocol::{
            wl_buffer::WlBuffer,
            wl_output::{self, WlOutput},
            wl_shm,
            wl_shm_pool::WlShmPool,
        },
    },
    protocols_wlr::screencopy::v1::client::{
        zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
        zwlr_screencopy_manager_v1::{self, ZwlrScreencopyManagerV1},
    },
};
use smithay_client_toolkit::{
    delegate_output, delegate_registry,
    globals::GlobalData,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
};
use std::{
    os::fd::AsRawFd,
    time::{Duration, Instant},
};

const CAPTURE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug)]
struct BufferInfo {
    format: wl_shm::Format,
    width: u32,
    height: u32,
    stride: u32,
}

struct CaptureState {
    registry_state: RegistryState,
    output_state: OutputState,
    shm: wl_shm::WlShm,
    version: u32,
    formats: Vec<BufferInfo>,
    chosen: Option<BufferInfo>,
    image: Option<ShmImage>,
    pool: Option<WlShmPool>,
    buffer: Option<WlBuffer>,
    y_invert: bool,
    transform: wl_output::Transform,
    error: Option<String>,
    done: bool,
}

fn order(format: wl_shm::Format) -> Option<(ChannelOrder, bool)> {
    match format {
        wl_shm::Format::Xrgb8888 => Some((ChannelOrder::Bgra, true)),
        wl_shm::Format::Argb8888 => Some((ChannelOrder::Bgra, true)),
        wl_shm::Format::Xbgr8888 => Some((ChannelOrder::Rgba, true)),
        wl_shm::Format::Abgr8888 => Some((ChannelOrder::Rgba, true)),
        _ => None,
    }
}

impl CaptureState {
    fn output(&self, name: Option<&str>) -> Result<WlOutput, String> {
        let mut outputs = self.output_state.outputs();
        let found = match name {
            Some(name) => outputs.find(|output| {
                self.output_state
                    .info(output)
                    .and_then(|info| info.name)
                    .as_deref()
                    == Some(name)
            }),
            None => outputs.next(),
        };
        found.ok_or_else(|| {
            name.map_or_else(
                || "没有可用的 Wayland 输出".to_string(),
                |name| format!("显示器 {name} 不存在或未启用"),
            )
        })
    }

    fn fail(&mut self, message: impl Into<String>) {
        self.error = Some(message.into());
        // 连续取帧时失败的帧也要释放缓冲区，避免长截图期间累积 wl_shm 对象。
        if let Some(buffer) = self.buffer.take() {
            buffer.destroy();
        }
        if let Some(pool) = self.pool.take() {
            pool.destroy();
        }
        self.image = None;
        self.done = true;
    }

    fn allocate_and_copy(&mut self, frame: &ZwlrScreencopyFrameV1, qh: &QueueHandle<Self>) {
        if self.image.is_some() {
            return;
        }
        // 优先使用合成器原生的 XRGB/ARGB，避免额外交换通道。
        let Some(info) = self
            .formats
            .iter()
            .copied()
            .find(|info| order(info.format).is_some_and(|(o, _)| o == ChannelOrder::Bgra))
            .or_else(|| {
                self.formats
                    .iter()
                    .copied()
                    .find(|info| order(info.format).is_some())
            })
        else {
            return self.fail("screencopy 未提供可用的 32 位 wl_shm 格式");
        };
        let image = match ShmImage::new(info.width, info.height, info.stride) {
            Ok(image) => image,
            Err(error) => return self.fail(error),
        };
        let pool = self.shm.create_pool(image.fd(), image.len() as i32, qh, ());
        let buffer = pool.create_buffer(
            0,
            info.width as i32,
            info.height as i32,
            info.stride as i32,
            info.format,
            qh,
            (),
        );
        frame.copy(&buffer);
        self.chosen = Some(info);
        self.image = Some(image);
        self.pool = Some(pool);
        self.buffer = Some(buffer);
    }

    fn finish(&mut self) {
        let (Some(info), Some(image)) = (self.chosen, self.image.as_mut()) else {
            return self.fail("截图缓冲区尚未创建");
        };
        let (channels, opaque) = order(info.format).unwrap_or((ChannelOrder::Bgra, true));
        image.normalize(channels, opaque, self.y_invert);
        image.orient(self.transform as u32 % 4, self.transform as u32 >= 4);
        if let Some(buffer) = self.buffer.take() {
            buffer.destroy();
        }
        if let Some(pool) = self.pool.take() {
            pool.destroy();
        }
        self.done = true;
    }
}

impl OutputHandler for CaptureState {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlOutput) {}
}

impl Dispatch<ZwlrScreencopyManagerV1, GlobalData> for CaptureState {
    fn event(
        _: &mut Self,
        _: &ZwlrScreencopyManagerV1,
        _: zwlr_screencopy_manager_v1::Event,
        _: &GlobalData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, ()> for CaptureState {
    fn event(
        state: &mut Self,
        frame: &ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_screencopy_frame_v1::Event::Buffer {
                format: WEnum::Value(format),
                width,
                height,
                stride,
            } => {
                state.formats.push(BufferInfo {
                    format,
                    width,
                    height,
                    stride,
                });
                // v1/v2 没有 buffer_done，收到唯一的 buffer 事件后即可复制。
                if state.version < 3 {
                    state.allocate_and_copy(frame, qh);
                }
            }
            zwlr_screencopy_frame_v1::Event::BufferDone => state.allocate_and_copy(frame, qh),
            zwlr_screencopy_frame_v1::Event::Flags { flags } => {
                state.y_invert = matches!(flags, WEnum::Value(f)
                    if f.contains(zwlr_screencopy_frame_v1::Flags::YInvert));
            }
            zwlr_screencopy_frame_v1::Event::Ready { .. } => {
                state.finish();
                frame.destroy();
            }
            zwlr_screencopy_frame_v1::Event::Failed => {
                state.fail("合成器拒绝了 screencopy 请求");
                frame.destroy();
            }
            _ => {}
        }
    }
}

delegate_noop!(CaptureState: ignore wl_shm::WlShm);
delegate_noop!(CaptureState: ignore WlShmPool);
delegate_noop!(CaptureState: ignore WlBuffer);
delegate_output!(CaptureState);
delegate_registry!(CaptureState);
impl ProvidesRegistryState for CaptureState {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState];
}

/// 等待事件直到截止时间；返回 `Ok(false)` 表示超时。
pub fn dispatch_until<S>(
    queue: &mut EventQueue<S>,
    state: &mut S,
    deadline: Instant,
) -> Result<bool, String> {
    if queue
        .dispatch_pending(state)
        .map_err(|error| error.to_string())?
        > 0
    {
        return Ok(true);
    }
    queue.flush().map_err(|error| error.to_string())?;
    let Some(guard) = queue.prepare_read() else {
        queue
            .dispatch_pending(state)
            .map_err(|error| error.to_string())?;
        return Ok(true);
    };
    let remaining = deadline.saturating_duration_since(Instant::now());
    let mut fd = libc::pollfd {
        fd: guard.connection_fd().as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: pollfd 指向栈上有效对象，数量为 1。
    let ready = unsafe { libc::poll(&mut fd, 1, remaining.as_millis().min(60_000) as i32) };
    if ready < 0 {
        let error = std::io::Error::last_os_error();
        return if error.kind() == std::io::ErrorKind::Interrupted {
            Ok(true)
        } else {
            Err(error.to_string())
        };
    }
    if ready == 0 {
        return Ok(false);
    }
    match guard.read() {
        Ok(_) => {}
        Err(smithay_client_toolkit::reexports::client::backend::WaylandError::Io(error))
            if error.kind() == std::io::ErrorKind::WouldBlock => {}
        Err(error) => return Err(error.to_string()),
    }
    queue
        .dispatch_pending(state)
        .map_err(|error| error.to_string())?;
    Ok(true)
}

/// 截取一个 wlroots 兼容输出到 memfd 共享图像。
pub fn capture(output_name: Option<&str>) -> Result<ShmImage, String> {
    let mut grabber = Grabber::new(output_name, None)?;
    grabber.grab()
}

/// 可重复截图的 screencopy 连接；长截图在同一连接上按区域连续取帧。
pub struct Grabber {
    queue: EventQueue<CaptureState>,
    state: CaptureState,
    manager: ZwlrScreencopyManagerV1,
    output: WlOutput,
    /// 输出内的逻辑坐标区域；`None` 表示整个输出。
    region: Option<(i32, i32, i32, i32)>,
}

impl Grabber {
    pub fn new(
        output_name: Option<&str>,
        region: Option<(i32, i32, i32, i32)>,
    ) -> Result<Self, String> {
        let connection =
            Connection::connect_to_env().map_err(|error| format!("无法连接 Wayland: {error}"))?;
        let (globals, mut queue) = registry_queue_init::<CaptureState>(&connection)
            .map_err(|error| format!("读取 Wayland 全局对象失败: {error}"))?;
        let qh = queue.handle();
        let shm: wl_shm::WlShm = globals
            .bind(&qh, 1..=1, ())
            .map_err(|error| format!("wl_shm 不可用: {error}"))?;
        let manager: ZwlrScreencopyManagerV1 = globals
            .bind(&qh, 1..=3, GlobalData)
            .map_err(|error| format!("screencopy 协议不可用: {error}"))?;
        let mut state = CaptureState {
            registry_state: RegistryState::new(&globals),
            output_state: OutputState::new(&globals, &qh),
            shm,
            version: Proxy::version(&manager),
            formats: Vec::new(),
            chosen: None,
            image: None,
            pool: None,
            buffer: None,
            y_invert: false,
            transform: wl_output::Transform::Normal,
            error: None,
            done: false,
        };
        // 第一次往返得到 wl_output，第二次得到名称等输出信息。
        queue
            .roundtrip(&mut state)
            .map_err(|error| format!("Wayland 输出查询失败: {error}"))?;
        queue
            .roundtrip(&mut state)
            .map_err(|error| format!("Wayland 输出信息不完整: {error}"))?;
        let output = state.output(output_name)?;
        state.transform = state
            .output_state
            .info(&output)
            .map(|info| info.transform)
            .unwrap_or(wl_output::Transform::Normal);
        Ok(Self {
            queue,
            state,
            manager,
            output,
            region,
        })
    }

    /// 截取一帧；每帧使用新的 memfd，返回后调用方独占像素。
    pub fn grab(&mut self) -> Result<ShmImage, String> {
        let state = &mut self.state;
        state.formats.clear();
        state.chosen = None;
        state.image = None;
        state.y_invert = false;
        state.error = None;
        state.done = false;
        let qh = self.queue.handle();
        match self.region {
            Some((x, y, width, height)) => {
                self.manager
                    .capture_output_region(0, &self.output, x, y, width, height, &qh, ())
            }
            None => self.manager.capture_output(0, &self.output, &qh, ()),
        };
        let deadline = Instant::now() + CAPTURE_TIMEOUT;
        while !self.state.done {
            if !dispatch_until(&mut self.queue, &mut self.state, deadline)?
                && Instant::now() >= deadline
            {
                return Err("screencopy 超时：合成器 5 秒内没有返回截图".into());
            }
        }
        match self.state.error.take() {
            Some(error) => Err(error),
            None => self
                .state
                .image
                .take()
                .ok_or_else(|| "screencopy 没有返回像素".into()),
        }
    }
}

impl Drop for Grabber {
    fn drop(&mut self) {
        self.manager.destroy();
        let _ = self.queue.roundtrip(&mut self.state);
    }
}
