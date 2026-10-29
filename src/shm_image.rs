//! memfd 共享内存图像。
//!
//! 截图会话只保留一份全分辨率像素：合成器通过 screencopy 直接写入这块 memfd，
//! 编辑器把同一块内存作为 wl_shm 缓冲区显示，导出时再按行读取。内存顺序固定为
//! 小端 XRGB8888/ARGB8888，即每个像素依次为 B、G、R、A/X。
use crate::pixels::RgbaPixels;
use memmap2::MmapMut;
use std::{
    fs::File,
    io::BufReader,
    os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd},
    path::Path,
};

/// 截图协议可能给出的 32 位通道顺序。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChannelOrder {
    /// 内存顺序 B、G、R、A/X（wl_shm Argb8888/Xrgb8888）。
    Bgra,
    /// 内存顺序 R、G、B、A/X（wl_shm Abgr8888/Xbgr8888）。
    Rgba,
}

pub struct ShmImage {
    file: File,
    map: MmapMut,
    width: u32,
    height: u32,
    stride: u32,
    /// 像素含有效 alpha（已预乘）；截图恒为不透明。
    alpha: bool,
}

/// 创建带 CLOEXEC 的匿名共享内存文件。
pub fn memfd(name: &str, len: usize) -> Result<File, String> {
    let name = std::ffi::CString::new(name).map_err(|_| "共享内存名称无效")?;
    // SAFETY: name 是以 NUL 结尾的有效 C 字符串；返回的 fd 立即交给 File 管理。
    let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err(format!(
            "无法创建共享内存: {}",
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: fd 刚由 memfd_create 返回且未被其他对象持有。
    let file = unsafe { File::from_raw_fd(fd) };
    file.set_len(len as u64)
        .map_err(|error| format!("无法扩展共享内存: {error}"))?;
    Ok(file)
}

/// 归还共享内存中一段已不再显示的页面；文件长度保持不变，之后写入会重新分配零页。
pub fn release_pages(fd: BorrowedFd<'_>, offset: usize, len: usize) {
    if len == 0 {
        return;
    }
    // SAFETY: 只对本进程创建的 memfd 打洞；失败只意味着内存未能提前归还。
    unsafe {
        libc::fallocate(
            fd.as_raw_fd(),
            libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE,
            offset as libc::off_t,
            len as libc::off_t,
        );
    }
}

impl ShmImage {
    pub fn new(width: u32, height: u32, stride: u32) -> Result<Self, String> {
        let row = width.checked_mul(4).ok_or("图像行宽溢出")?;
        if width == 0 || height == 0 || stride < row {
            return Err("图像尺寸无效".into());
        }
        let len = stride as usize * height as usize;
        if len > i32::MAX as usize {
            return Err("图像超过 wl_shm 支持的 2 GiB 上限".into());
        }
        let file = memfd("babry-image", len)?;
        // SAFETY: 映射由本对象独占持有，文件长度在映射期间不会缩短。
        let map = unsafe { MmapMut::map_mut(&file) }
            .map_err(|error| format!("无法映射共享内存: {error}"))?;
        Ok(Self {
            file,
            map,
            width,
            height,
            stride,
            alpha: false,
        })
    }

    /// KDE ScreenShot2 等非 screencopy 来源只在这里复制一次，随后可立即释放 RGBA。
    pub fn from_rgba(pixels: &RgbaPixels) -> Result<Self, String> {
        let mut image = Self::new(pixels.width(), pixels.height(), pixels.width() * 4)?;
        let row = pixels.width() as usize * 4;
        for (src, dst) in pixels
            .bytes
            .chunks_exact(row)
            .zip(image.map.chunks_exact_mut(row))
        {
            for (s, d) in src.chunks_exact(4).zip(dst.chunks_exact_mut(4)) {
                d.copy_from_slice(&[s[2], s[1], s[0], 255]);
            }
        }
        Ok(image)
    }

    /// 逐行解码 PNG 到共享内存，避免完整 RGBA 中间副本。
    pub fn read_png(path: &Path) -> Result<Self, String> {
        let file = File::open(path).map_err(|error| format!("无法读取 PNG: {error}"))?;
        let mut decoder = png::Decoder::new(BufReader::new(file));
        decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
        let mut reader = decoder
            .read_info()
            .map_err(|error| format!("读取 PNG 头部失败: {error}"))?;
        let (width, height) = {
            let info = reader.info();
            (info.width, info.height)
        };
        let (color, _) = reader.output_color_type();
        let channels = match color {
            png::ColorType::Rgba => 4,
            png::ColorType::Rgb => 3,
            png::ColorType::GrayscaleAlpha => 2,
            png::ColorType::Grayscale => 1,
            png::ColorType::Indexed => return Err("PNG 调色板展开失败".into()),
        };
        let mut image = Self::new(width, height, width.checked_mul(4).ok_or("PNG 尺寸溢出")?)?;
        image.alpha = matches!(color, png::ColorType::Rgba | png::ColorType::GrayscaleAlpha);
        let stride = image.stride as usize;
        for y in 0..height as usize {
            let row = reader
                .next_row()
                .map_err(|error| format!("解码 PNG 失败: {error}"))?
                .ok_or("PNG 行数不足")?;
            let data = row.data();
            let dst = &mut image.map[y * stride..y * stride + width as usize * 4];
            for (src, d) in data.chunks_exact(channels).zip(dst.chunks_exact_mut(4)) {
                let (r, g, b, a) = match channels {
                    4 => (src[0], src[1], src[2], src[3]),
                    3 => (src[0], src[1], src[2], 255),
                    2 => (src[0], src[0], src[0], src[1]),
                    _ => (src[0], src[0], src[0], 255),
                };
                let premultiply = |v: u8| ((u16::from(v) * u16::from(a) + 127) / 255) as u8;
                d.copy_from_slice(&[premultiply(b), premultiply(g), premultiply(r), a]);
            }
        }
        Ok(image)
    }

    pub fn width(&self) -> u32 {
        self.width
    }
    pub fn height(&self) -> u32 {
        self.height
    }
    pub fn stride(&self) -> u32 {
        self.stride
    }
    pub fn has_alpha(&self) -> bool {
        self.alpha
    }
    pub fn len(&self) -> usize {
        self.map.len()
    }
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
    pub fn fd(&self) -> BorrowedFd<'_> {
        self.file.as_fd()
    }
    pub fn bytes_mut(&mut self) -> &mut [u8] {
        &mut self.map
    }

    /// 截图完成后原地规范通道顺序与上下翻转，不额外分配整帧内存。
    pub fn normalize(&mut self, order: ChannelOrder, opaque: bool, y_invert: bool) {
        let (width, height, stride) = (
            self.width as usize,
            self.height as usize,
            self.stride as usize,
        );
        if y_invert {
            for y in 0..height / 2 {
                let (top, bottom) = self.map.split_at_mut((height - 1 - y) * stride);
                top[y * stride..y * stride + width * 4].swap_with_slice(&mut bottom[..width * 4]);
            }
        }
        for row in self.map.chunks_exact_mut(stride).take(height) {
            for pixel in row[..width * 4].chunks_exact_mut(4) {
                if order == ChannelOrder::Rgba {
                    pixel.swap(0, 2);
                }
                if opaque {
                    pixel[3] = 255;
                }
            }
        }
        self.alpha = !opaque;
    }

    /// 读取一个直通 RGBA 像素；越界时返回黑色。
    pub fn rgba(&self, x: u32, y: u32) -> [u8; 4] {
        if x >= self.width || y >= self.height {
            return [0, 0, 0, 255];
        }
        let i = y as usize * self.stride as usize + x as usize * 4;
        let p = &self.map[i..i + 4];
        [p[2], p[1], p[0], p[3]]
    }

    /// 把 `[x, x + out.len() / 4)` 的一行转换为预乘 RGBA；越界部分填充透明。
    pub fn read_row(&self, x: i32, y: i32, out: &mut [u8]) {
        out.fill(0);
        if y < 0 || y as u32 >= self.height {
            return;
        }
        let start = x.max(0) as u32;
        let end = (x + (out.len() / 4) as i32).clamp(0, self.width as i32) as u32;
        if end <= start {
            return;
        }
        let row = y as usize * self.stride as usize;
        let src = &self.map[row + start as usize * 4..row + end as usize * 4];
        let offset = (start as i32 - x) as usize * 4;
        for (s, d) in src.chunks_exact(4).zip(out[offset..].chunks_exact_mut(4)) {
            d.copy_from_slice(&[s[2], s[1], s[0], s[3]]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pixels::PixelSize;

    #[test]
    fn converts_rgba_and_reads_rows_with_clipping() {
        let pixels = RgbaPixels::from_rgba(
            PixelSize::new(2, 1).unwrap(),
            vec![10, 20, 30, 255, 40, 50, 60, 255],
        )
        .unwrap();
        let image = ShmImage::from_rgba(&pixels).unwrap();
        assert_eq!(image.rgba(1, 0), [40, 50, 60, 255]);
        let mut row = [9u8; 12];
        image.read_row(-1, 0, &mut row);
        assert_eq!(row, [0, 0, 0, 0, 10, 20, 30, 255, 40, 50, 60, 255]);
    }

    #[test]
    fn normalizes_rgba_order_and_vertical_flip() {
        let mut image = ShmImage::new(1, 2, 4).unwrap();
        image.bytes_mut().copy_from_slice(&[1, 2, 3, 0, 4, 5, 6, 0]);
        image.normalize(ChannelOrder::Rgba, true, true);
        assert_eq!(image.rgba(0, 0), [4, 5, 6, 255]);
        assert_eq!(image.rgba(0, 1), [1, 2, 3, 255]);
    }
}
