//! Native wlroots screencopy capture for niri/Hyprland.
//!
//! The compositor writes directly into a wl_shm buffer. We copy the released
//! pixels into the editor's owned source only after the frame is ready; no PNG
//! temporary file or image-decoder allocation is involved.

use crate::pixels::{PixelSize, RgbaPixels};
use smithay_client_toolkit::reexports::{
    client::{
        Connection, Dispatch, QueueHandle, WEnum,
        globals::registry_queue_init,
        protocol::{wl_output::WlOutput, wl_shm},
    },
    protocols_wlr::screencopy::v1::client::{
        zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
        zwlr_screencopy_manager_v1::{self, ZwlrScreencopyManagerV1},
    },
};
use smithay_client_toolkit::{
    delegate_output, delegate_registry, delegate_shm,
    globals::GlobalData,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    shm::{
        Shm, ShmHandler,
        slot::{Buffer, SlotPool},
    },
};

#[derive(Clone, Copy, Debug)]
struct BufferInfo {
    format: wl_shm::Format,
    width: u32,
    height: u32,
    stride: u32,
}

pub struct CapturedFrame {
    pub pixels: RgbaPixels,
    pub name: Option<String>,
    pub logical_width: u32,
    pub logical_height: u32,
    pub scale: f32,
}

struct CaptureState {
    registry_state: RegistryState,
    output_state: OutputState,
    shm: Shm,
    manager: ZwlrScreencopyManagerV1,
    frame: Option<ZwlrScreencopyFrameV1>,
    buffer_info: Option<BufferInfo>,
    pool: Option<SlotPool>,
    buffer: Option<Buffer>,
    image: Option<RgbaPixels>,
    error: Option<String>,
    ready: bool,
    done: bool,
}

impl CaptureState {
    fn output(&self, name: Option<&str>) -> Result<WlOutput, String> {
        let outputs = self.output_state.outputs().collect::<Vec<_>>();
        outputs
            .into_iter()
            .find(|output| {
                self.output_state
                    .info(output)
                    .and_then(|info| info.name)
                    .as_deref()
                    == name
            })
            .or_else(|| {
                if name.is_some() {
                    None
                } else {
                    self.output_state.outputs().next()
                }
            })
            .ok_or_else(|| {
                name.map_or_else(
                    || "没有可用的 Wayland 输出".to_string(),
                    |name| format!("显示器 {name} 不存在或未启用"),
                )
            })
    }

    fn allocate_and_copy(&mut self, frame: &ZwlrScreencopyFrameV1) {
        let Some(info) = self.buffer_info else {
            self.error = Some("screencopy 未提供 wl_shm 缓冲区".into());
            self.done = true;
            return;
        };
        if info.width == 0 || info.height == 0 || info.stride < info.width.saturating_mul(4) {
            self.error = Some("screencopy 缓冲区几何无效".into());
            self.done = true;
            return;
        }
        let _size = match PixelSize::new(info.width, info.height) {
            Ok(size) => size,
            Err(error) => {
                self.error = Some(error);
                self.done = true;
                return;
            }
        };
        let pool_size = info.stride as usize * info.height as usize;
        let pool = match SlotPool::new(pool_size, &self.shm) {
            Ok(pool) => pool,
            Err(error) => {
                self.error = Some(format!("无法创建截图共享内存: {error}"));
                self.done = true;
                return;
            }
        };
        let mut pool = pool;
        let buffer = match pool.create_buffer(
            info.width as i32,
            info.height as i32,
            info.stride as i32,
            info.format,
        ) {
            Ok((buffer, _canvas)) => buffer,
            Err(error) => {
                self.error = Some(format!("无法创建截图缓冲区: {error}"));
                self.done = true;
                return;
            }
        };
        buffer
            .activate()
            .map_err(|error| {
                self.error = Some(format!("无法激活截图缓冲区: {error}"));
                self.done = true;
                error
            })
            .ok();
        if self.done {
            return;
        }
        frame.copy(buffer.wl_buffer());
        self.pool = Some(pool);
        self.buffer = Some(buffer);
    }

    fn finish(&mut self) {
        let (Some(info), Some(buffer), Some(pool)) =
            (self.buffer_info, &self.buffer, &mut self.pool)
        else {
            self.error = Some("截图缓冲区尚未创建".into());
            self.done = true;
            return;
        };
        // The screencopy `ready` event guarantees that the compositor finished
        // writing. raw_data_mut is safe here even before wl_buffer.release; we
        // never reuse this buffer and drop it after copying the completed frame.
        let slot = buffer.slot();
        let data = pool.raw_data_mut(&slot);
        let size = match PixelSize::new(info.width, info.height) {
            Ok(size) => size,
            Err(error) => {
                self.error = Some(error);
                self.done = true;
                return;
            }
        };
        let mut image = RgbaPixels::new(size);
        let format = info.format;
        for y in 0..info.height as usize {
            let source =
                &data[y * info.stride as usize..y * info.stride as usize + info.width as usize * 4];
            for (src, dst) in source
                .chunks_exact(4)
                .zip(image.bytes[y * size.stride..(y + 1) * size.stride].chunks_exact_mut(4))
            {
                match format {
                    wl_shm::Format::Argb8888 | wl_shm::Format::Xrgb8888 => {
                        dst.copy_from_slice(&[
                            src[2],
                            src[1],
                            src[0],
                            if format == wl_shm::Format::Argb8888 {
                                src[3]
                            } else {
                                255
                            },
                        ]);
                    }
                    wl_shm::Format::Abgr8888 | wl_shm::Format::Xbgr8888 => {
                        dst.copy_from_slice(&[
                            src[0],
                            src[1],
                            src[2],
                            if format == wl_shm::Format::Abgr8888 {
                                src[3]
                            } else {
                                255
                            },
                        ]);
                    }
                    _ => {
                        self.error = Some(format!("不支持的 wl_shm 截图格式: {format:?}"));
                        self.done = true;
                        return;
                    }
                }
            }
        }
        self.image = Some(image);
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
impl ShmHandler for CaptureState {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
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
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_screencopy_frame_v1::Event::Buffer {
                format: WEnum::Value(format),
                width,
                height,
                stride,
            } => {
                state.buffer_info = Some(BufferInfo {
                    format,
                    width,
                    height,
                    stride,
                });
            }
            zwlr_screencopy_frame_v1::Event::BufferDone => state.allocate_and_copy(frame),
            zwlr_screencopy_frame_v1::Event::Ready { .. } => {
                state.ready = true;
                state.finish();
                frame.destroy();
            }
            zwlr_screencopy_frame_v1::Event::Failed => {
                state.error = Some("合成器拒绝了 screencopy 请求".into());
                state.done = true;
            }
            _ => {}
        }
    }
}

delegate_output!(CaptureState);
delegate_shm!(CaptureState);
delegate_registry!(CaptureState);
impl ProvidesRegistryState for CaptureState {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState];
}

/// Capture one wlroots-compatible output into an owned RGBA pixel buffer.
pub fn capture(output_name: Option<&str>) -> Result<CapturedFrame, String> {
    let connection =
        Connection::connect_to_env().map_err(|error| format!("无法连接 Wayland: {error}"))?;
    let (globals, mut queue) = registry_queue_init::<CaptureState>(&connection)
        .map_err(|error| format!("读取 Wayland 全局对象失败: {error}"))?;
    let qh = queue.handle();
    let shm = Shm::bind(&globals, &qh).map_err(|error| format!("wl_shm 不可用: {error}"))?;
    let manager: ZwlrScreencopyManagerV1 = globals
        .bind(&qh, 1..=3, GlobalData)
        .map_err(|error| format!("screencopy 协议不可用: {error}"))?;
    let mut state = CaptureState {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        shm,
        manager,
        frame: None,
        buffer_info: None,
        pool: None,
        buffer: None,
        image: None,
        error: None,
        ready: false,
        done: false,
    };
    queue
        .roundtrip(&mut state)
        .map_err(|error| format!("Wayland 输出查询失败: {error}"))?;
    queue
        .roundtrip(&mut state)
        .map_err(|error| format!("Wayland 输出信息不完整: {error}"))?;
    let output = state.output(output_name)?;
    state.frame = Some(state.manager.capture_output(0, &output, &qh, ()));
    while !state.done {
        queue
            .blocking_dispatch(&mut state)
            .map_err(|error| format!("screencopy 事件循环失败: {error}"))?;
    }
    state.error.map_or_else(
        || {
            state
                .image
                .take()
                .map(|pixels| CapturedFrame {
                    logical_width: pixels.width(),
                    logical_height: pixels.height(),
                    pixels,
                    name: output_name.map(str::to_owned),
                    scale: 1.0,
                })
                .ok_or_else(|| "screencopy 没有返回像素".into())
        },
        Err,
    )
}
