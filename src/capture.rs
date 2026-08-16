use std::{path::Path, process::Command};

pub fn capture(path: &Path) -> Result<(), String> {
    let status = Command::new("grim")
        .args(["-t", "png"])
        .arg(path)
        .status()
        .map_err(|e| format!("无法启动 grim: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("grim 截图失败: {status}"))
    }
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
