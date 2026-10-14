use std::{
    path::Path,
    process::{Command, Stdio},
};

// 输出名称为必填，避免漏传 -o 时 grim 自动拼接所有显示器。
pub fn capture(path: &Path, output: &str) -> Result<(), String> {
    let status = capture_command(path, output)
        .status()
        .map_err(|e| format!("无法启动 grim: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("grim 截图失败: {status}"))
    }
}

fn capture_command(path: &Path, output: &str) -> Command {
    let mut command = Command::new("grim");
    command.args(["-t", "png", "-o", output]).arg(path);
    command
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_always_limits_grim_to_one_output() {
        let command = capture_command(Path::new("capture.png"), "DP-2");
        let args: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args, ["-t", "png", "-o", "DP-2", "capture.png"]);
    }
}
