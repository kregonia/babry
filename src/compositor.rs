//! 合成器检测、直接 IPC 和 Wayland 输出元数据。
use crate::core::RectPx;
use serde_json::Value;
use smithay_client_toolkit::reexports::client::{
    Connection, QueueHandle,
    globals::registry_queue_init,
    protocol::wl_output::{Transform, WlOutput},
};
use smithay_client_toolkit::{
    delegate_output, delegate_registry,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
};
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
    time::Duration,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    Niri,
    Hyprland,
    Kde,
}
impl Backend {
    pub fn detect() -> Result<Self, String> {
        let desktop = std::env::var("XDG_CURRENT_DESKTOP")
            .unwrap_or_default()
            .to_lowercase();
        if desktop.contains("kde") || desktop.contains("plasma") {
            return Ok(Self::Kde);
        }
        if desktop.contains("niri") {
            return Ok(Self::Niri);
        }
        if desktop.contains("hypr") {
            return Ok(Self::Hyprland);
        }
        if std::env::var_os("NIRI_SOCKET").is_some() {
            Ok(Self::Niri)
        } else if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some() {
            Ok(Self::Hyprland)
        } else if std::env::var_os("KDE_FULL_SESSION").is_some() {
            Ok(Self::Kde)
        } else {
            Err("当前支持 niri、Hyprland 和 KDE Plasma 6.8+".into())
        }
    }
}
#[derive(Clone, Debug)]
pub struct OutputInfo {
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub scale: f32,
    pub focused: bool,
}
pub fn select_output(outputs: &[OutputInfo], name: Option<&str>) -> Result<OutputInfo, String> {
    if let Some(name) = name {
        outputs
            .iter()
            .find(|o| o.name == name)
            .cloned()
            .ok_or_else(|| format!("显示器 {name} 不存在或未启用"))
    } else {
        outputs
            .iter()
            .find(|o| o.focused)
            .or_else(|| outputs.first())
            .cloned()
            .ok_or("没有启用的显示器".into())
    }
}
fn niri_query(request: &str) -> Result<Value, String> {
    let path = std::env::var_os("NIRI_SOCKET").ok_or("NIRI_SOCKET 未设置")?;
    let mut socket = UnixStream::connect(path).map_err(|e| format!("连接 niri IPC 失败: {e}"))?;
    socket
        .set_read_timeout(Some(Duration::from_secs(3)))
        .map_err(|e| e.to_string())?;
    writeln!(socket, "\"{request}\"").map_err(|e| e.to_string())?;
    let mut data = String::new();
    BufReader::new(socket)
        .take(4 * 1024 * 1024)
        .read_line(&mut data)
        .map_err(|e| e.to_string())?;
    let value: Value = serde_json::from_str(&data).map_err(|e| e.to_string())?;
    value
        .get("Ok")
        .cloned()
        .ok_or_else(|| format!("niri IPC 返回错误: {value}"))
}
fn hypr_query(request: &str) -> Result<Value, String> {
    use std::io::Read;
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").ok_or("XDG_RUNTIME_DIR 未设置")?;
    let signature = std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").ok_or("Hyprland 实例未设置")?;
    let path = PathBuf::from(runtime)
        .join("hypr")
        .join(signature)
        .join(".socket.sock");
    let mut socket =
        UnixStream::connect(path).map_err(|e| format!("连接 Hyprland IPC 失败: {e}"))?;
    socket
        .set_read_timeout(Some(Duration::from_secs(3)))
        .map_err(|e| e.to_string())?;
    socket
        .write_all(format!("j/{request}").as_bytes())
        .map_err(|e| e.to_string())?;
    let mut data = Vec::new();
    socket
        .take(4 * 1024 * 1024)
        .read_to_end(&mut data)
        .map_err(|e| e.to_string())?;
    serde_json::from_slice(&data).map_err(|e| e.to_string())
}
pub fn focused_name() -> Option<String> {
    match Backend::detect().ok()? {
        Backend::Niri => niri_query("FocusedOutput")
            .ok()?
            .get("FocusedOutput")?
            .get("name")?
            .as_str()
            .map(str::to_owned),
        Backend::Hyprland => hypr_query("monitors")
            .ok()?
            .as_array()?
            .iter()
            .find(|o| o["focused"] == true)?
            .get("name")?
            .as_str()
            .map(str::to_owned),
        Backend::Kde => crate::kde::active_output().ok(),
    }
}
struct Probe {
    registry: RegistryState,
    outputs: OutputState,
}
impl OutputHandler for Probe {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.outputs
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlOutput) {}
}
delegate_output!(Probe);
delegate_registry!(Probe);
impl ProvidesRegistryState for Probe {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState];
}
pub fn outputs() -> Result<Vec<OutputInfo>, String> {
    let conn = Connection::connect_to_env().map_err(|e| e.to_string())?;
    let (globals, mut queue) = registry_queue_init::<Probe>(&conn).map_err(|e| e.to_string())?;
    let qh = queue.handle();
    let mut state = Probe {
        registry: RegistryState::new(&globals),
        outputs: OutputState::new(&globals, &qh),
    };
    queue.roundtrip(&mut state).map_err(|e| e.to_string())?;
    queue.roundtrip(&mut state).map_err(|e| e.to_string())?;
    let focused = focused_name();
    Ok(state
        .outputs
        .outputs()
        .filter_map(|o| {
            let i = state.outputs.info(&o)?;
            let name = i.name?;
            let (x, y) = i.logical_position?;
            let (w, h) = i.logical_size?;
            if w <= 0 || h <= 0 {
                return None;
            }
            let scale = i
                .modes
                .iter()
                .find(|m| m.current)
                .map(|m| {
                    let rotated = matches!(
                        i.transform,
                        Transform::_90
                            | Transform::_270
                            | Transform::Flipped90
                            | Transform::Flipped270
                    );
                    let (pw, ph) = if rotated {
                        (m.dimensions.1, m.dimensions.0)
                    } else {
                        m.dimensions
                    };
                    (pw as f32 / w as f32 + ph as f32 / h as f32) * 0.5
                })
                .unwrap_or(i.scale_factor as f32);
            Some(OutputInfo {
                focused: focused.as_ref() == Some(&name),
                name,
                x,
                y,
                width: w as u32,
                height: h as u32,
                scale,
            })
        })
        .collect())
}
/// 只有合成器公布的可见顶层窗口几何可作为锚点，不假设 IPC 提供应用内部控件。
pub fn window_regions(output: &OutputInfo, physical: (u32, u32)) -> Vec<RectPx> {
    let sx = physical.0 as f64 / f64::from(output.width);
    let sy = physical.1 as f64 / f64::from(output.height);
    let mut rects = Vec::new();
    let mut add = |x: f64, y: f64, w: f64, h: f64| {
        let rect = RectPx::new(
            ((x - f64::from(output.x)) * sx).round() as i32,
            ((y - f64::from(output.y)) * sy).round() as i32,
            (w * sx).round().max(0.) as u32,
            (h * sy).round().max(0.) as u32,
        )
        .clamp_to(RectPx::new(0, 0, physical.0, physical.1));
        if rect.width > 2 && rect.height > 2 {
            rects.push(rect)
        }
    };
    match Backend::detect() {
        Ok(Backend::Hyprland) => {
            if let Ok(v) = hypr_query("clients")
                && let Some(list) = v.as_array()
            {
                for w in list {
                    if w["hidden"] == true || w["mapped"] == false {
                        continue;
                    }
                    if let (Some(x), Some(y), Some(width), Some(height)) = (
                        w["at"][0].as_f64(),
                        w["at"][1].as_f64(),
                        w["size"][0].as_f64(),
                        w["size"][1].as_f64(),
                    ) {
                        add(x, y, width, height)
                    }
                }
            }
        }
        Ok(Backend::Niri) => {
            if let Ok(v) = niri_query("Windows")
                && let Some(list) = v["Windows"].as_array()
            {
                for w in list {
                    let l = &w["layout"];
                    if let (Some(x), Some(y), Some(width), Some(height)) = (
                        l["pos_in_scrolling_layout"][0].as_f64(),
                        l["pos_in_scrolling_layout"][1].as_f64(),
                        l["window_size"][0].as_f64(),
                        l["window_size"][1].as_f64(),
                    ) && w["is_focused"] == true
                    {
                        add(x, y, width, height)
                    }
                }
            }
        }
        _ => {}
    }
    rects
}
use std::io::Read;
