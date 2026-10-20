//! KDE Plasma 6.8+（KWin）会话支持。
//!
//! KWin 不提供 wlroots 系的 `wlr-screencopy` / `ext-image-copy-capture` 截屏协议，
//! 因此 `grim` 在 KDE 上不可用。截屏走 KWin 的 `org.kde.KWin.ScreenShot2`
//! （Spectacle 使用的同一接口），通过 pipe 文件描述符接收原始像素。KWin 6.8+
//! 不需要桌面文件声明；更早的 Plasma 需要 `X-KDE-DBUS-Restricted-Interfaces`，
//! 本后端仅支持 Plasma 6.8+。
//!
//! 跨桌面的标准路径是 XDG Desktop Portal（ScreenCast）+ PipeWire；本后端不采用它，
//! 因为它需要用户授权弹窗并引入 PipeWire，和 grim 在 wlroots 上的直接截屏体验不一致。
//! ScreenShot2 是 KWin 提供的会话内截屏接口，调用方式与 Spectacle 相同。
//!
//! 输出枚举使用与合成器无关的 Wayland `xdg_output`；焦点输出通过
//! `org.kde.KWin.activeOutputName` 查询。

use crate::compositor::{Compositor, OutputInfo};
use image::RgbaImage;
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::reexports::client::globals::registry_queue_init;
use smithay_client_toolkit::reexports::client::protocol::wl_output::{Transform, WlOutput};
use smithay_client_toolkit::reexports::client::{Connection, QueueHandle};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use std::collections::HashMap;
use std::io::Read;
use std::os::fd::OwnedFd;
use zbus::zvariant::{Fd, OwnedValue, Value};

const KWIN_DEST: &str = "org.kde.KWin";
const KWIN_PATH: &str = "/KWin";
const SCREENSHOT_DEST: &str = "org.kde.KWin.ScreenShot2";
const SCREENSHOT_PATH: &str = "/org/kde/KWin/ScreenShot2";

// KWin 的屏幕截取固定返回 QImage::Format_RGBX8888，内存字节序为 R,G,B,X。
const FORMAT_RGBX8888: u32 = 16;

pub(super) struct Kde;

impl Compositor for Kde {
    fn outputs(&self) -> Result<Vec<OutputInfo>, String> {
        enumerate_outputs()
    }

    fn capture(&self, output: &str) -> Result<RgbaImage, String> {
        capture_output(output)
    }
}

fn session_bus() -> Result<zbus::blocking::Connection, String> {
    zbus::blocking::Connection::session().map_err(|e| format!("无法连接 D-Bus 会话总线: {e}"))
}

fn screenshot_proxy(bus: &zbus::blocking::Connection) -> Result<zbus::blocking::Proxy<'_>, String> {
    zbus::blocking::Proxy::new(bus, SCREENSHOT_DEST, SCREENSHOT_PATH, SCREENSHOT_DEST)
        .map_err(|e| format!("无法访问 KWin 截屏接口: {e}"))
}

fn active_output_name() -> Result<String, String> {
    let bus = session_bus()?;
    let kwin = zbus::blocking::Proxy::new(&bus, KWIN_DEST, KWIN_PATH, KWIN_DEST)
        .map_err(|e| format!("无法访问 KWin 接口: {e}"))?;
    kwin.call("activeOutputName", &())
        .map_err(|e| format!("无法查询 KDE 焦点显示器: {e}"))
}

/// 截取指定输出的物理像素，等价于 `grim -o NAME`，返回 RGBA 图像。
fn capture_output(name: &str) -> Result<RgbaImage, String> {
    let bus = session_bus()?;
    let shot = screenshot_proxy(&bus)?;
    let (mut reader, writer) = std::io::pipe().map_err(|e| format!("创建管道失败: {e}"))?;
    let writer = OwnedFd::from(writer);
    let results: HashMap<String, OwnedValue> = shot
        .call("CaptureScreen", &(name, capture_options(), Fd::from(&writer)))
        .map_err(|e| {
            let error = e.to_string();
            if error.contains("NoAuthorized") {
                format!(
                    "KWin 拒绝截屏：Plasma 6.8 之前的版本需要桌面文件声明 X-KDE-DBUS-Restricted-Interfaces；Babry 仅支持 Plasma 6.8+（{error}）"
                )
            } else {
                format!("KWin 截取 {name} 失败: {error}")
            }
        })?;
    // 必须先关写端再读：KWin 异步写完即关闭其副本，读到 EOF 才算完整。
    drop(writer);
    let mut data = Vec::new();
    reader
        .read_to_end(&mut data)
        .map_err(|e| format!("读取截屏数据失败: {e}"))?;
    decode_raw(&results, &data)
}

/// `native-resolution` 取物理像素，与 grim 的输出分辨率语义一致。
fn capture_options() -> HashMap<&'static str, Value<'static>> {
    [("native-resolution", Value::from(true))].into()
}

fn decode_raw(results: &HashMap<String, OwnedValue>, data: &[u8]) -> Result<RgbaImage, String> {
    let kind = meta_string(results, "type")?;
    if kind != "raw" {
        return Err(format!("KWin 截屏类型 {kind} 不受支持"));
    }
    let format = meta_u32(results, "format")?;
    if format != FORMAT_RGBX8888 {
        return Err(format!(
            "KWin 截屏像素格式 {format} 不受支持（仅支持 RGBX8888）"
        ));
    }
    let width = meta_u32(results, "width")? as usize;
    let height = meta_u32(results, "height")? as usize;
    let stride = meta_u32(results, "stride")? as usize;
    if width == 0 || height == 0 {
        return Err("KWin 截屏尺寸无效".into());
    }
    let row_bytes = width
        .checked_mul(4)
        .ok_or_else(|| "KWin 截屏行宽溢出".to_string())?;
    if stride < row_bytes {
        return Err(format!("KWin 截屏 stride {stride} 小于行宽 {row_bytes}"));
    }
    let expected = stride
        .checked_mul(height)
        .ok_or_else(|| "KWin 截屏尺寸溢出".to_string())?;
    if data.len() < expected {
        return Err(format!(
            "KWin 截屏数据不完整: {} 字节，需要 {expected}",
            data.len()
        ));
    }
    let pixel_bytes = row_bytes
        .checked_mul(height)
        .ok_or_else(|| "KWin 截屏像素缓冲区溢出".to_string())?;
    // 去掉每行尾部的 stride 填充，R,G,B,X 字节序可直接作为 RGBA 使用。
    let pixels = if stride == row_bytes {
        data[..pixel_bytes].to_vec()
    } else {
        let mut pixels = Vec::with_capacity(pixel_bytes);
        for row in 0..height {
            let start = row * stride;
            pixels.extend_from_slice(&data[start..start + row_bytes]);
        }
        pixels
    };
    RgbaImage::from_raw(width as u32, height as u32, pixels)
        .ok_or_else(|| "KWin 截屏像素缓冲区无效".to_string())
}

fn meta_u32(results: &HashMap<String, OwnedValue>, key: &str) -> Result<u32, String> {
    let value = results
        .get(key)
        .ok_or_else(|| format!("KWin 截屏结果缺少字段 {key}"))?;
    u32::try_from(value).map_err(|e| format!("KWin 截屏字段 {key} 无效: {e}"))
}

fn meta_string(results: &HashMap<String, OwnedValue>, key: &str) -> Result<String, String> {
    let value = results
        .get(key)
        .ok_or_else(|| format!("KWin 截屏结果缺少字段 {key}"))?;
    value
        .downcast_ref::<String>()
        .map_err(|e| format!("KWin 截屏字段 {key} 无效: {e}"))
}

struct OutputProbe {
    registry_state: RegistryState,
    output_state: OutputState,
}

impl OutputHandler for OutputProbe {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlOutput) {}
}

smithay_client_toolkit::delegate_registry!(OutputProbe);
smithay_client_toolkit::delegate_output!(OutputProbe);

impl ProvidesRegistryState for OutputProbe {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    smithay_client_toolkit::registry_handlers!(OutputState);
}

/// 枚举输出：`xdg_output` 逻辑坐标 + 当前 mode 推导的分数缩放，焦点输出由 KWin 指定。
fn enumerate_outputs() -> Result<Vec<OutputInfo>, String> {
    let connection = Connection::connect_to_env().map_err(|e| format!("无法连接 Wayland: {e}"))?;
    let (globals, mut queue) = registry_queue_init::<OutputProbe>(&connection)
        .map_err(|e| format!("读取全局对象失败: {e}"))?;
    let qh = queue.handle();
    let mut probe = OutputProbe {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
    };
    queue
        .roundtrip(&mut probe)
        .map_err(|e| format!("Wayland 事件循环失败: {e}"))?;
    // xdg_output 的逻辑几何在 wl_output 之后到达，需要第二轮 roundtrip。
    queue
        .roundtrip(&mut probe)
        .map_err(|e| format!("Wayland 事件循环失败: {e}"))?;
    let focused = match active_output_name() {
        Ok(name) => Some(name),
        Err(error) => {
            eprintln!("babry: {error}");
            None
        }
    };
    Ok(probe
        .output_state
        .outputs()
        .filter_map(|output| {
            let info = probe.output_state.info(&output)?;
            let name = info.name.clone()?;
            let (x, y) = info.logical_position?;
            let (width, height) = info.logical_size?;
            if width <= 0 || height <= 0 {
                return None;
            }
            Some(OutputInfo {
                name,
                x,
                y,
                width: width as u32,
                height: height as u32,
                scale: fractional_scale(&info),
                focused: focused.as_deref() == info.name.as_deref(),
            })
        })
        .collect())
}

/// KWin 的 `wl_output.scale` 是向上取整的整数（分数缩放 1.2 上报 2），
/// 真实缩放用当前 mode 的物理尺寸除以 xdg_output 逻辑尺寸推导。
fn fractional_scale(info: &smithay_client_toolkit::output::OutputInfo) -> f32 {
    if let (Some(logical), Some(mode)) = (
        info.logical_size,
        info.modes.iter().find(|mode| mode.current),
    ) && let Some(scale) =
        scale_from_dimensions(logical, mode.dimensions, is_rotated(info.transform))
    {
        return scale;
    }
    info.scale_factor.max(1) as f32
}

fn is_rotated(transform: Transform) -> bool {
    matches!(
        transform,
        Transform::_90 | Transform::_270 | Transform::Flipped90 | Transform::Flipped270
    )
}

fn scale_from_dimensions(logical: (i32, i32), pixel: (i32, i32), rotated: bool) -> Option<f32> {
    if logical.0 <= 0 || logical.1 <= 0 {
        return None;
    }
    let (pixel_w, pixel_h) = if rotated { (pixel.1, pixel.0) } else { pixel };
    let scale = (f64::from(pixel_w) / f64::from(logical.0)
        + f64::from(pixel_h) / f64::from(logical.1))
        / 2.0;
    (scale.is_finite() && scale > 0.0).then_some(scale as f32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(entries: &[(&str, u32)]) -> HashMap<String, OwnedValue> {
        let mut results: HashMap<String, OwnedValue> = entries
            .iter()
            .map(|(key, value)| ((*key).to_string(), OwnedValue::from(*value)))
            .collect();
        results.insert(
            "type".into(),
            OwnedValue::try_from(Value::from("raw")).unwrap(),
        );
        results
    }

    #[test]
    fn decodes_raw_pixels_and_drops_stride_padding() {
        let results = meta(&[("width", 2), ("height", 2), ("stride", 12), ("format", 16)]);
        // 每行 8 字节有效像素 + 4 字节 stride 填充。
        let data = [
            1, 2, 3, 255, 4, 5, 6, 255, 9, 9, 9, 9, //
            7, 8, 9, 255, 10, 11, 12, 255, 9, 9, 9, 9,
        ];
        let image = decode_raw(&results, &data).unwrap();
        assert_eq!(image.dimensions(), (2, 2));
        assert_eq!(
            image.as_raw(),
            &[1, 2, 3, 255, 4, 5, 6, 255, 7, 8, 9, 255, 10, 11, 12, 255]
        );
    }

    #[test]
    fn decodes_rgbx_without_stride_padding() {
        let results = meta(&[("width", 1), ("height", 1), ("stride", 4), ("format", 16)]);
        let image = decode_raw(&results, &[1, 2, 3, 255]).unwrap();
        assert_eq!(image.as_raw(), &[1, 2, 3, 255]);
    }

    #[test]
    fn rejects_unknown_type_or_premultiplied_format() {
        let mut results = meta(&[("width", 1), ("height", 1), ("stride", 4), ("format", 16)]);
        results.insert(
            "type".into(),
            OwnedValue::try_from(Value::from("png")).unwrap(),
        );
        assert!(decode_raw(&results, &[0; 4]).is_err());

        let results = meta(&[("width", 1), ("height", 1), ("stride", 4), ("format", 18)]);
        assert!(decode_raw(&results, &[1, 2, 3, 128]).is_err());
    }

    #[test]
    fn rejects_broken_geometry() {
        let results = meta(&[("width", 2), ("height", 1), ("stride", 4), ("format", 16)]);
        assert!(decode_raw(&results, &[0; 8]).is_err());
        let results = meta(&[("width", 2), ("height", 1), ("stride", 4), ("format", 16)]);
        assert!(decode_raw(&results, &[0; 4]).is_err());
        let results = meta(&[("width", 2), ("height", 2), ("stride", 8), ("format", 16)]);
        assert!(decode_raw(&results, &[0; 12]).is_err());
        let results = meta(&[("width", 1), ("stride", 4), ("format", 16)]);
        assert!(decode_raw(&results, &[0; 4]).is_err());
    }

    #[test]
    fn derives_fractional_scale_from_mode_and_logical_size() {
        let scale = scale_from_dimensions((1867, 1167), (2240, 1400), false).unwrap();
        assert!((scale - 1.1997).abs() < 0.001);
        let rotated = scale_from_dimensions((1152, 2048), (2560, 1440), true).unwrap();
        assert!((rotated - 1.25).abs() < 0.001);
        assert_eq!(scale_from_dimensions((0, 0), (1, 1), false), None);
    }

    #[test]
    fn capture_asks_for_native_resolution() {
        let options = capture_options();
        assert_eq!(options.get("native-resolution"), Some(&Value::from(true)));
    }
}
