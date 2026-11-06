//! 滚动长截图：编辑器退出后，在选区四周画一圈不接收输入的边框，用户照常滚动下方
//! 窗口；后台线程按区域连续截图，交给 `stitch` 拼接，点“完成”后保存并复制。
//!
//! 边框和控制条都放在截图区域之外，不会被拍进结果。
use babry::{
    compositor::{Backend, OutputInfo},
    config,
    core::RectPx,
    kde,
    screencopy::Grabber,
    shm_image::ShmImage,
    stitch::{Frame, Push, Stitcher},
};
use std::{
    fs::File,
    io::BufWriter,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

mod overlay;

/// 取帧间隔；约 12 帧每秒足够跟上滚轮，同时限制 CPU。
const FRAME_INTERVAL: Duration = Duration::from_millis(80);
/// 等待编辑器表面从屏幕上消失后再取第一帧。
const SETTLE: Duration = Duration::from_millis(250);

pub struct Request {
    pub output: OutputInfo,
    /// 编辑器截图的物理尺寸，用于把选区换算到输出逻辑坐标。
    pub physical: (u32, u32),
    pub selection: RectPx,
    pub save_dir: PathBuf,
}

/// 后台线程报告给控制条的进度。
#[derive(Clone, Copy, Debug)]
pub struct Progress {
    pub height: u32,
    pub push: Push,
}

/// 把物理像素选区换算为输出逻辑坐标，向内取整，保证边框落在截图区域之外。
pub fn logical_region(selection: RectPx, physical: (u32, u32), logical: (u32, u32)) -> RectPx {
    let sx = f64::from(logical.0) / f64::from(physical.0.max(1));
    let sy = f64::from(logical.1) / f64::from(physical.1.max(1));
    let left = (f64::from(selection.x) * sx).ceil() as i32;
    let top = (f64::from(selection.y) * sy).ceil() as i32;
    let right = ((f64::from(selection.x) + f64::from(selection.width)) * sx).floor() as i32;
    let bottom = ((f64::from(selection.y) + f64::from(selection.height)) * sy).floor() as i32;
    let left = left.clamp(0, logical.0 as i32);
    let top = top.clamp(0, logical.1 as i32);
    RectPx::new(
        left,
        top,
        (right.min(logical.0 as i32) - left).max(1) as u32,
        (bottom.min(logical.1 as i32) - top).max(1) as u32,
    )
}

/// 取帧来源：wlroots screencopy 直接截区域；KDE 截整屏再裁剪。
enum Source {
    Wlr(Box<Grabber>),
    Kde { name: String, crop: RectPx },
}

impl Source {
    fn open(output: &OutputInfo, region: RectPx) -> Result<Self, String> {
        let area = (
            region.x,
            region.y,
            region.width as i32,
            region.height as i32,
        );
        match Grabber::new(Some(&output.name), Some(area)) {
            Ok(grabber) => Ok(Self::Wlr(Box::new(grabber))),
            // KWin 没有 wlr-screencopy，改用 ScreenShot2 截整屏后裁剪。
            Err(_) if Backend::detect().ok() == Some(Backend::Kde) => {
                let scale = f64::from(output.scale);
                let crop = RectPx::new(
                    (f64::from(region.x) * scale).round() as i32,
                    (f64::from(region.y) * scale).round() as i32,
                    (f64::from(region.width) * scale).round() as u32,
                    (f64::from(region.height) * scale).round() as u32,
                );
                Ok(Self::Kde {
                    name: output.name.clone(),
                    crop,
                })
            }
            Err(error) => Err(error),
        }
    }

    fn frame(&mut self) -> Result<Frame, String> {
        match self {
            Self::Wlr(grabber) => {
                let image = grabber.grab()?;
                let full = RectPx::new(0, 0, image.width(), image.height());
                Ok(to_frame(&image, full))
            }
            Self::Kde { name, crop } => Ok(to_frame(&kde::capture(name)?, *crop)),
        }
    }
}

/// 把共享内存图像中的一块区域转换为紧密排列的 RGB 行。
fn to_frame(image: &ShmImage, rect: RectPx) -> Frame {
    let mut rgba = vec![0u8; rect.width as usize * 4];
    let mut rgb = Vec::with_capacity(rect.width as usize * rect.height as usize * 3);
    for y in 0..rect.height as i32 {
        image.read_row(rect.x, rect.y + y, &mut rgba);
        for pixel in rgba.chunks_exact(4) {
            rgb.extend_from_slice(&pixel[..3]);
        }
    }
    Frame {
        width: rect.width,
        height: rect.height,
        rgb,
    }
}

/// 后台取帧和拼接；`stop` 置位后返回拼接器。
fn spawn_worker(
    mut source: Source,
    stop: Arc<AtomicBool>,
    progress: Sender<Progress>,
) -> JoinHandle<Result<Stitcher, String>> {
    thread::spawn(move || {
        let trace = std::env::var_os("BABRY_TRACE").is_some();
        let mut stitcher = Stitcher::new();
        thread::sleep(SETTLE);
        while !stop.load(Ordering::Relaxed) {
            let started = Instant::now();
            let frame = source.frame()?;
            let grabbed = started.elapsed();
            let push = stitcher.push(frame);
            if trace && push != Push::Unchanged {
                eprintln!(
                    "babry: 长截图 {push:?} 高度 {} 取帧 {:.1}ms 拼接 {:.1}ms",
                    stitcher.height(),
                    grabbed.as_secs_f64() * 1000.0,
                    (started.elapsed() - grabbed).as_secs_f64() * 1000.0
                );
            }
            let _ = progress.send(Progress {
                height: stitcher.height(),
                push,
            });
            if let Some(rest) = FRAME_INTERVAL.checked_sub(started.elapsed()) {
                thread::sleep(rest);
            }
        }
        Ok(stitcher)
    })
}

/// 流式写出 RGB PNG；失败时删除不完整的文件。
fn save_png(path: &Path, width: u32, height: u32, rgb: &[u8]) -> Result<(), String> {
    let write = || -> Result<(), String> {
        let file = File::create(path).map_err(|error| format!("无法创建 PNG: {error}"))?;
        let mut encoder = png::Encoder::new(BufWriter::new(file), width, height);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|error| format!("写入 PNG 头部失败: {error}"))?;
        writer
            .write_image_data(rgb)
            .map_err(|error| format!("写入 PNG 像素失败: {error}"))?;
        writer
            .finish()
            .map_err(|error| format!("完成 PNG 写入失败: {error}"))
    };
    let result = write();
    if result.is_err() {
        let _ = std::fs::remove_file(path);
    }
    result
}

fn copy_to_clipboard(path: &Path) -> Result<(), String> {
    let file = File::open(path).map_err(|error| error.to_string())?;
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

pub fn run(request: Request) -> Result<(), String> {
    let logical = (request.output.width, request.output.height);
    let region = logical_region(request.selection, request.physical, logical);
    let source = Source::open(&request.output, region)?;
    let stop = Arc::new(AtomicBool::new(false));
    let (sender, receiver): (Sender<Progress>, Receiver<Progress>) = mpsc::channel();
    let worker = spawn_worker(source, stop.clone(), sender);
    let finished = overlay::run(&request.output, region, receiver, &worker);
    stop.store(true, Ordering::Relaxed);
    let stitcher = worker
        .join()
        .map_err(|_| "长截图线程异常退出".to_string())??;
    if !finished? {
        return Ok(());
    }
    let (width, height, rgb) = stitcher.finish().ok_or("长截图没有取到任何画面")?;
    let path = config::output_path(&request.save_dir)?;
    save_png(&path, width, height, &rgb)?;
    drop(rgb);
    eprintln!("已保存长截图: {} ({width}x{height})", path.display());
    copy_to_clipboard(&path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logical_region_rounds_inwards_at_fractional_scale() {
        // 125%：物理 2560×1440 对应逻辑 2048×1152。
        let region = logical_region(RectPx::new(101, 51, 1001, 501), (2560, 1440), (2048, 1152));
        assert_eq!(region, RectPx::new(81, 41, 800, 400));
        let full = logical_region(RectPx::new(0, 0, 2560, 1440), (2560, 1440), (2048, 1152));
        assert_eq!(full, RectPx::new(0, 0, 2048, 1152));
    }
}
