use std::{
    path::Path,
    process::{Command, Stdio},
};

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
