use super::{Compositor, OutputInfo, command_json, integer, positive_size, scale};
use image::RgbaImage;
use serde_json::Value;

pub(super) struct Hyprland;

impl Compositor for Hyprland {
    fn outputs(&self) -> Result<Vec<OutputInfo>, String> {
        parse_hyprland(&command_json("hyprctl", &["-j", "monitors"])?)
    }

    fn capture(&self, output: &str) -> Result<RgbaImage, String> {
        super::grim::capture(output)
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_rotated_fractional_output_to_logical_units() {
        let value = serde_json::json!([{ "name":"DP-1","x":-1152,"y":0,"width":2560,"height":1440,"scale":1.25,"transform":1,"focused":true }]);
        let output = parse_hyprland(&value).unwrap().remove(0);
        assert_eq!((output.width, output.height), (1152, 2048));
        assert_eq!(output.scale, 1.25);
        assert!(output.focused);
    }

    #[test]
    fn malformed_geometry_is_rejected() {
        assert!(
            parse_hyprland(
                &serde_json::json!([{"name":"DP-1","x":0,"y":0,"width":1920,"height":1080,"scale":0}])
            )
            .is_err()
        );
    }
}
