//! 复制 PNG 到剪贴板：Wayland 交给 wl-copy；X11 由 babry 自己的后台进程持有选区。
use std::{
    io::{BufRead, BufReader},
    path::Path,
    process::{Command, Stdio},
    time::Duration,
};

/// 当前是否为 X11 会话。`XDG_SESSION_TYPE` 优先；未设置时按显示变量判断。
pub fn x11_session() -> bool {
    match std::env::var("XDG_SESSION_TYPE").ok().as_deref() {
        Some("x11") => return true,
        Some("wayland") => return false,
        _ => {}
    }
    std::env::var_os("WAYLAND_DISPLAY").is_none() && std::env::var_os("DISPLAY").is_some()
}

pub fn copy_png(path: &Path) -> Result<(), String> {
    if x11_session() {
        return copy_x11(path);
    }
    let file = std::fs::File::open(path).map_err(|error| error.to_string())?;
    let status = Command::new("wl-copy")
        .args(["--type", "image/png"])
        .stdin(file)
        .status()
        .map_err(|error| format!("无法启动 wl-copy: {error}"))?;
    status
        .success()
        .then_some(())
        .ok_or_else(|| format!("wl-copy 失败: {status}"))
}

/// X11 剪贴板没有服务进程：内容由拥有者在被请求时提供。启动一个独立的
/// `babry --x11-clipboard PATH` 持有 CLIPBOARD，等它确认拿到所有权后返回。
fn copy_x11(path: &Path) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|error| error.to_string())?;
    let mut child = Command::new(exe)
        .arg("--x11-clipboard")
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|error| format!("无法启动剪贴板进程: {error}"))?;
    let stdout = child.stdout.take().ok_or("剪贴板进程没有输出")?;
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let _ = BufReader::new(stdout).read_line(&mut line);
        let _ = sender.send(line);
    });
    match receiver.recv_timeout(Duration::from_secs(3)) {
        Ok(line) if line.trim() == "ready" => Ok(()),
        Ok(_) => Err("X11 剪贴板进程未能取得所有权".into()),
        Err(_) => Err("X11 剪贴板进程超时".into()),
    }
}
