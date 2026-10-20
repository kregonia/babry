mod grim;
mod hyprland;
mod kde;
mod niri;

use image::RgbaImage;
use serde_json::Value;
use std::process::Command;

/// 所有位置与尺寸均为合成器全局逻辑坐标，可包含负坐标。
#[derive(Clone, Debug, PartialEq)]
pub struct OutputInfo {
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub scale: f32,
    pub focused: bool,
}

/// 合成器的截屏与输出枚举接口；每个后端实现该 trait，由 Backend 负责选择。
pub trait Compositor {
    fn outputs(&self) -> Result<Vec<OutputInfo>, String>;
    fn capture(&self, output: &str) -> Result<RgbaImage, String>;
}

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
            .to_ascii_lowercase();
        detect_from(
            &desktop,
            std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some(),
            std::env::var_os("NIRI_SOCKET").is_some(),
            std::env::var_os("KDE_FULL_SESSION").is_some(),
        )
    }

    pub fn compositor(self) -> Box<dyn Compositor> {
        match self {
            Self::Niri => Box::new(niri::Niri),
            Self::Hyprland => Box::new(hyprland::Hyprland),
            Self::Kde => Box::new(kde::Kde),
        }
    }
}

fn detect_from(desktop: &str, hyprland: bool, niri: bool, kde: bool) -> Result<Backend, String> {
    if hyprland || desktop.contains("hypr") {
        Ok(Backend::Hyprland)
    } else if niri || desktop.contains("niri") {
        Ok(Backend::Niri)
    } else if kde || desktop.contains("kde") || desktop.contains("plasma") {
        Ok(Backend::Kde)
    } else {
        Err("当前仅支持 niri、Hyprland 和 KDE Plasma 6.8+ 会话".into())
    }
}

pub fn outputs() -> Result<Vec<OutputInfo>, String> {
    let result = Backend::detect()?.compositor().outputs()?;
    if result.is_empty() {
        Err("没有启用的显示器".into())
    } else {
        Ok(result)
    }
}

pub fn select_output(outputs: &[OutputInfo], name: Option<&str>) -> Result<OutputInfo, String> {
    if let Some(name) = name {
        outputs
            .iter()
            .find(|output| output.name == name)
            .cloned()
            .ok_or_else(|| format!("显示器 {name} 不存在或未启用"))
    } else {
        outputs
            .iter()
            .find(|output| output.focused)
            .or_else(|| outputs.first())
            .cloned()
            .ok_or_else(|| "没有启用的显示器".into())
    }
}

impl OutputInfo {
    pub fn contains(&self, x: f64, y: f64) -> bool {
        x >= f64::from(self.x)
            && y >= f64::from(self.y)
            && x < f64::from(self.x) + f64::from(self.width)
            && y < f64::from(self.y) + f64::from(self.height)
    }
}

fn integer(object: &Value, key: &str) -> Result<i32, String> {
    object
        .get(key)
        .and_then(Value::as_i64)
        .and_then(|n| i32::try_from(n).ok())
        .ok_or_else(|| format!("输出字段 {key} 不是有效整数"))
}
fn positive_size(object: &Value, key: &str) -> Result<u32, String> {
    let n = integer(object, key)?;
    if n > 0 {
        Ok(n as u32)
    } else {
        Err(format!("输出字段 {key} 必须大于零"))
    }
}
fn scale(object: &Value) -> Result<f32, String> {
    let n = object.get("scale").and_then(Value::as_f64).unwrap_or(0.0);
    if n.is_finite() && n > 0.0 && n <= 16.0 {
        Ok(n as f32)
    } else {
        Err("输出缩放比例无效".into())
    }
}
fn command_json(program: &str, args: &[&str]) -> Result<Value, String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|error| format!("无法启动 {program}: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "{program} 执行失败: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    serde_json::from_slice(&output.stdout).map_err(|error| format!("{program} JSON 无效: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_detection_covers_supported_sessions() {
        assert_eq!(
            detect_from("hyprland", false, false, false).unwrap(),
            Backend::Hyprland
        );
        assert_eq!(
            detect_from("niri", false, false, false).unwrap(),
            Backend::Niri
        );
        assert_eq!(
            detect_from("kde", false, false, false).unwrap(),
            Backend::Kde
        );
        assert_eq!(
            detect_from("plasma", false, false, false).unwrap(),
            Backend::Kde
        );
        assert_eq!(detect_from("", false, false, true).unwrap(), Backend::Kde);
        assert!(detect_from("gnome", false, false, false).is_err());
    }

    #[test]
    fn select_output_prefers_focused_then_first() {
        let outputs = vec![
            OutputInfo {
                name: "DP-1".into(),
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
                scale: 1.0,
                focused: false,
            },
            OutputInfo {
                name: "DP-2".into(),
                x: 1920,
                y: 0,
                width: 2560,
                height: 1440,
                scale: 1.25,
                focused: true,
            },
        ];
        assert_eq!(select_output(&outputs, None).unwrap().name, "DP-2");
        assert_eq!(select_output(&outputs, Some("DP-1")).unwrap().name, "DP-1");
        assert!(select_output(&outputs, Some("HDMI-A-1")).is_err());
    }
}
