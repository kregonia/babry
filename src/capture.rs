use crate::compositor;
use image::{ImageFormat, RgbaImage};
use std::{
    path::Path,
    process::{Command, Stdio},
};

/// 捕获指定显示器并保存为 PNG。
pub fn capture(path: &Path, output: &str) -> Result<(), String> {
    capture_image(output)?
        .save_with_format(path, ImageFormat::Png)
        .map_err(|error| format!("保存截图失败: {error}"))
}

/// 捕获指定显示器的像素；输出名称必填：grim 漏传 `-o` 时会自动拼接所有显示器，
/// KDE 的 `CaptureScreen` 也按输出名截取。
pub fn capture_image(output: &str) -> Result<RgbaImage, String> {
    compositor::Backend::detect()?.compositor().capture(output)
}

pub fn notify(message: &str) {
    let _ = Command::new("notify-send")
        .args(["--app-name", "babry", "Babry", message])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

pub fn copy_png(path: &Path) -> Result<(), String> {
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let status = Command::new("wl-copy")
        .args(["--type", "image/png"])
        .stdin(file)
        .status()
        .map_err(|e| format!("无法启动 wl-copy: {e}"))?;
    status
        .success()
        .then_some(())
        .ok_or_else(|| format!("wl-copy 失败: {status}"))
}
