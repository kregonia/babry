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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    Niri,
    Hyprland,
}

impl Backend {
    pub fn detect() -> Result<Self, String> {
        let desktop = std::env::var("XDG_CURRENT_DESKTOP")
            .unwrap_or_default()
            .to_ascii_lowercase();
        if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some() || desktop.contains("hypr") {
            Ok(Self::Hyprland)
        } else if std::env::var_os("NIRI_SOCKET").is_some() || desktop.contains("niri") {
            Ok(Self::Niri)
        } else {
            Err("当前仅支持 niri 和 Hyprland 会话".into())
        }
    }
}

pub fn outputs() -> Result<Vec<OutputInfo>, String> {
    let result = match Backend::detect()? {
        Backend::Niri => {
            let value = command_json("niri", &["msg", "-j", "outputs"])?;
            let focus = command_json("niri", &["msg", "-j", "focused-output"])?;
            parse_niri(&value, focus.get("name").and_then(Value::as_str))?
        }
        Backend::Hyprland => parse_hyprland(&command_json("hyprctl", &["-j", "monitors"])?)?,
    };
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

fn parse_niri(value: &Value, focused: Option<&str>) -> Result<Vec<OutputInfo>, String> {
    let object = value
        .as_object()
        .ok_or_else(|| "niri 输出格式无效".to_string())?;
    object
        .iter()
        .filter(|(_, output)| output.get("logical").is_some_and(|v| !v.is_null()))
        .map(|(name, output)| {
            let logical = &output["logical"];
            let info = OutputInfo {
                name: name.clone(),
                x: integer(logical, "x")?,
                y: integer(logical, "y")?,
                width: positive_size(logical, "width")?,
                height: positive_size(logical, "height")?,
                scale: scale(logical)?,
                focused: focused == Some(name.as_str()),
            };
            Ok(info)
        })
        .collect()
}

fn parse_hyprland(value: &Value) -> Result<Vec<OutputInfo>, String> {
    let array = value
        .as_array()
        .ok_or_else(|| "Hyprland 输出格式无效".to_string())?;
    array
        .iter()
        .filter(|monitor| monitor.get("disabled").and_then(Value::as_bool) != Some(true))
        .map(|monitor| {
            let scale = scale(monitor)?;
            let mut width = positive_size(monitor, "width")?;
            let mut height = positive_size(monitor, "height")?;
            let transform = monitor
                .get("transform")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            if transform > 7 {
                return Err("Hyprland 输出旋转值无效".into());
            }
            if transform % 2 == 1 {
                std::mem::swap(&mut width, &mut height);
            }
            Ok(OutputInfo {
                name: monitor
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|n| !n.is_empty())
                    .ok_or_else(|| "Hyprland 输出缺少名称".to_string())?
                    .to_string(),
                x: integer(monitor, "x")?,
                y: integer(monitor, "y")?,
                width: (width as f64 / f64::from(scale)).round().max(1.0) as u32,
                height: (height as f64 / f64::from(scale)).round().max(1.0) as u32,
                scale,
                focused: monitor
                    .get("focused")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            })
        })
        .collect()
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
    fn niri_uses_logical_geometry_and_skips_disabled_outputs() {
        let value = serde_json::json!({"DP-1":{"logical":{"x":-2048,"y":0,"width":2048,"height":1152,"scale":1.25}},"DP-2":{"logical":null},"HDMI-A-1":{"logical":{"x":0,"y":0,"width":1920,"height":1080,"scale":1}}});
        let outputs = parse_niri(&value, Some("HDMI-A-1")).unwrap();
        assert_eq!(outputs.len(), 2);
        assert_eq!(select_output(&outputs, None).unwrap().name, "HDMI-A-1");
        assert!(
            select_output(&outputs, Some("DP-1"))
                .unwrap()
                .contains(-100.0, 500.0)
        );
        assert!(select_output(&outputs, Some("DP-2")).is_err());
    }
    #[test]
    fn hyprland_converts_rotated_fractional_output_to_logical_units() {
        let value = serde_json::json!([{ "name":"DP-1","x":-1152,"y":0,"width":2560,"height":1440,"scale":1.25,"transform":1,"focused":true }]);
        let output = parse_hyprland(&value).unwrap().remove(0);
        assert_eq!((output.width, output.height), (1152, 2048));
        assert_eq!(output.scale, 1.25);
        assert!(output.focused);
    }
    #[test]
    fn malformed_geometry_is_rejected() {
        assert!(parse_hyprland(&serde_json::json!([{"name":"DP-1","x":0,"y":0,"width":1920,"height":1080,"scale":0}])).is_err());
        assert!(parse_niri(&serde_json::json!({"DP-1":{"logical":{"x":0,"y":0,"width":-1,"height":1080,"scale":1}}}),None).is_err());
    }
}
