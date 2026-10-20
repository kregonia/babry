use super::{Compositor, OutputInfo, command_json, integer, positive_size, scale};
use image::RgbaImage;
use serde_json::Value;

pub(super) struct Niri;

impl Compositor for Niri {
    fn outputs(&self) -> Result<Vec<OutputInfo>, String> {
        let value = command_json("niri", &["msg", "-j", "outputs"])?;
        let focus = command_json("niri", &["msg", "-j", "focused-output"])?;
        parse_niri(&value, focus.get("name").and_then(Value::as_str))
    }

    fn capture(&self, output: &str) -> Result<RgbaImage, String> {
        super::grim::capture(output)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uses_logical_geometry_and_skips_disabled_outputs() {
        let value = serde_json::json!({"DP-1":{"logical":{"x":-2048,"y":0,"width":2048,"height":1152,"scale":1.25}},"DP-2":{"logical":null},"HDMI-A-1":{"logical":{"x":0,"y":0,"width":1920,"height":1080,"scale":1}}});
        let outputs = parse_niri(&value, Some("HDMI-A-1")).unwrap();
        assert_eq!(outputs.len(), 2);
        assert!(outputs[0].contains(-100.0, 500.0));
        assert_eq!(outputs[1].name, "HDMI-A-1");
        assert!(outputs[1].focused);
    }

    #[test]
    fn malformed_geometry_is_rejected() {
        assert!(
            parse_niri(
                &serde_json::json!({"DP-1":{"logical":{"x":0,"y":0,"width":-1,"height":1080,"scale":1}}}),
                None
            )
            .is_err()
        );
    }
}
