use image::{DynamicImage, RgbaImage, codecs::pnm::PnmDecoder};
use std::{
    io::{BufReader, Read},
    process::{Command, Stdio},
};

/// grim 走无压缩 PPM 管道，避开 PNG 压缩、临时文件和再次读取。
/// PPM 不携带 alpha，桌面截图的每个像素转为不透明 RGBA。
pub(super) fn capture(output: &str) -> Result<RgbaImage, String> {
    let mut child = capture_command(output)
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

fn capture_command(output: &str) -> Command {
    let mut command = Command::new("grim");
    command.args(["-t", "ppm", "-o", output, "-"]);
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipe_capture_limits_one_output_and_preserves_rgb_bytes() {
        let command = capture_command("DP-2");
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
}
