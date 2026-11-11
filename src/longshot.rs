//! 滚动长截图的取帧与拼接。编辑器保持打开：工具栏和选区边框留在截图区域之外，
//! 选区内的输入穿透到下方窗口；后台线程按区域连续截图交给 `stitch` 拼接，
//! 再次点击长截图按钮后拼接结果成为可编辑的长图画布。
use babry::{
    compositor::{Backend, OutputInfo},
    core::RectPx,
    kde,
    screencopy::Grabber,
    shm_image::ShmImage,
    stitch::{Frame, MAX_HEIGHT, Push, Stitcher},
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

/// 取帧间隔；约 12 帧每秒足够跟上滚轮，同时限制 CPU。
const FRAME_INTERVAL: Duration = Duration::from_millis(80);
/// 等待编辑器表面从屏幕上消失后再取第一帧。
const SETTLE: Duration = Duration::from_millis(250);

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

/// 拼接结果转为共享内存图像，作为长图画布的底图（之后释放 RGB 缓冲区）。
pub fn into_image(stitcher: Stitcher) -> Result<ShmImage, String> {
    let (width, height, rgb) = stitcher.finish().ok_or("长截图没有取到任何画面")?;
    let mut image = ShmImage::new(width, height, width * 4)?;
    for (src, dst) in rgb
        .chunks_exact(3)
        .zip(image.bytes_mut().chunks_exact_mut(4))
    {
        dst.copy_from_slice(&[src[2], src[1], src[0], 255]);
    }
    Ok(image)
}

/// 进行中的长截图：后台线程、停止标志和进度通道。
pub struct Capture {
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<Result<Stitcher, String>>>,
    progress: Receiver<Progress>,
    first: u32,
    last: Option<Progress>,
    lost: u32,
}

impl Capture {
    /// 选区（截图物理像素）换算到输出逻辑坐标后开始取帧。
    pub fn start(
        output: &OutputInfo,
        selection: RectPx,
        physical: (u32, u32),
    ) -> Result<Self, String> {
        let region = logical_region(selection, physical, (output.width, output.height));
        let source = Source::open(output, region)?;
        let stop = Arc::new(AtomicBool::new(false));
        let (sender, progress) = mpsc::channel();
        let worker = spawn_worker(source, stop.clone(), sender);
        Ok(Self {
            stop,
            worker: Some(worker),
            progress,
            first: 0,
            last: None,
            lost: 0,
        })
    }

    /// 取走新进度，返回需要显示的状态文字（无变化时为 `None`）。
    /// 偶尔一帧对不上很常见（尤其 KDE 截整屏较慢），连续两帧丢失才提示。
    pub fn poll(&mut self) -> Option<String> {
        let mut changed = false;
        while let Ok(progress) = self.progress.try_recv() {
            match progress.push {
                Push::Started => self.first = progress.height,
                Push::Lost => self.lost += 1,
                Push::Appended(_) => self.lost = 0,
                _ => {}
            }
            self.last = Some(progress);
            changed = true;
        }
        changed.then(|| self.status())
    }

    pub fn status(&self) -> String {
        let Some(progress) = self.last else {
            return "长截图准备中…".into();
        };
        match progress.push {
            Push::Full => format!("已达上限 {MAX_HEIGHT} px"),
            _ if self.lost >= 2 => "滚动过快，请回滚一点".into(),
            _ if progress.height <= self.first => "请滚动选区内的页面".into(),
            _ => format!("已拼接 {} px", progress.height),
        }
    }

    /// 取帧线程已经退出（通常是出错）。
    pub fn failed(&self) -> bool {
        self.worker.as_ref().is_none_or(|w| w.is_finished())
    }

    /// 停止取帧并取回拼接器。
    pub fn finish(mut self) -> Result<Stitcher, String> {
        self.stop.store(true, Ordering::Relaxed);
        self.worker
            .take()
            .ok_or("长截图线程不存在")?
            .join()
            .map_err(|_| "长截图线程异常退出".to_string())?
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
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
