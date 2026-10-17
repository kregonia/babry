use image::{DynamicImage, RgbaImage, codecs::pnm::PnmDecoder};
use std::{
    io::{BufReader, Read},
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

/// 交互截图使用无压缩 PPM 管道，避开 PNG 压缩、临时文件和再次读取。
/// PPM 不携带 alpha，桌面截图的每个像素转为不透明 RGBA。
pub fn capture_image(output: &str) -> Result<RgbaImage, String> {
    let mut child = capture_image_command(output)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|error| format!("无法启动 grim: {error}"))?;
    let image = decode_ppm(BufReader::new(
        child.stdout.take().expect("grim stdout is piped"),
    ));
    // 解码失败时关闭管道并回收进程，不留下阻塞写入的 grim。
    if image.is_err() {
        let _ = child.kill();
    }
    let status = child
        .wait()
        .map_err(|error| format!("等待 grim 失败: {error}"))?;
    if !status.success() {
        return Err(format!("grim 截图失败: {status}"));
    }
    image
}

fn decode_ppm(reader: impl Read) -> Result<RgbaImage, String> {
    let decoder = PnmDecoder::new(reader).map_err(|error| format!("读取截图失败: {error}"))?;
    DynamicImage::from_decoder(decoder)
        .map(DynamicImage::into_rgba8)
        .map_err(|error| format!("解码截图失败: {error}"))
}

fn capture_image_command(output: &str) -> Command {
    let mut command = Command::new("grim");
    command.args(["-t", "ppm", "-o", output, "-"]);
    command
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
    fn pipe_capture_limits_one_output_and_preserves_rgb_bytes() {
        let command = capture_image_command("DP-2");
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            ["-t", "ppm", "-o", "DP-2", "-"]
        );
        let mut ppm = b"P6\n2 1\n255\n".to_vec();
        // 像素中允许出现换行、空格和零字节，不能按文本拆分像素数据。
        ppm.extend([10, 32, 0, 255, 70, 90]);
        let image = decode_ppm(&ppm[..]).unwrap();
        assert_eq!(image.dimensions(), (2, 1));
        assert_eq!(image.as_raw(), &[10, 32, 0, 255, 255, 70, 90, 255]);
        assert!(decode_ppm(&ppm[..ppm.len() - 1]).is_err());
    }

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
