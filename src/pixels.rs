//! 无 GUI 框架的像素基础设施：明确的 RGBA 所有权、PPM 兼容解码和流式 PNG。

use std::{
    fs::File,
    io::{BufReader, Read},
    path::Path,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PixelSize {
    pub width: u32,
    pub height: u32,
    pub stride: usize,
}

impl PixelSize {
    pub fn new(width: u32, height: u32) -> Result<Self, String> {
        let stride = (width as usize)
            .checked_mul(4)
            .ok_or_else(|| "像素行宽溢出".to_string())?;
        stride
            .checked_mul(height as usize)
            .ok_or_else(|| "像素缓冲区尺寸溢出".to_string())?;
        Ok(Self {
            width,
            height,
            stride,
        })
    }

    pub fn bytes(self) -> usize {
        self.stride * self.height as usize
    }
}

pub trait PixelTarget {
    fn width(&self) -> u32;
    fn height(&self) -> u32;
    fn pixel_mut(&mut self, x: u32, y: u32) -> Option<&mut [u8]>;
}

pub struct RgbaViewMut<'a> {
    pub size: PixelSize,
    pub bytes: &'a mut [u8],
}

impl<'a> RgbaViewMut<'a> {
    pub fn new(size: PixelSize, bytes: &'a mut [u8]) -> Result<Self, String> {
        if bytes.len() != size.bytes() {
            return Err("RGBA 视图尺寸不匹配".into());
        }
        Ok(Self { size, bytes })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RgbaPixels {
    pub size: PixelSize,
    pub bytes: Vec<u8>,
}

impl PixelTarget for RgbaPixels {
    fn width(&self) -> u32 {
        self.size.width
    }
    fn height(&self) -> u32 {
        self.size.height
    }
    fn pixel_mut(&mut self, x: u32, y: u32) -> Option<&mut [u8]> {
        self.pixel_mut(x, y)
    }
}

impl<'a> PixelTarget for RgbaViewMut<'a> {
    fn width(&self) -> u32 {
        self.size.width
    }
    fn height(&self) -> u32 {
        self.size.height
    }
    fn pixel_mut(&mut self, x: u32, y: u32) -> Option<&mut [u8]> {
        if x >= self.size.width || y >= self.size.height {
            return None;
        }
        let offset = y as usize * self.size.stride + x as usize * 4;
        self.bytes.get_mut(offset..offset + 4)
    }
}

impl RgbaPixels {
    pub fn new(size: PixelSize) -> Self {
        Self {
            size,
            bytes: vec![0; size.bytes()],
        }
    }

    pub fn from_rgba(size: PixelSize, bytes: Vec<u8>) -> Result<Self, String> {
        if bytes.len() != size.bytes() {
            return Err("RGBA 像素尺寸不匹配".into());
        }
        Ok(Self { size, bytes })
    }

    pub fn width(&self) -> u32 {
        self.size.width
    }

    pub fn height(&self) -> u32 {
        self.size.height
    }

    pub fn pixel(&self, x: u32, y: u32) -> Option<[u8; 4]> {
        if x >= self.size.width || y >= self.size.height {
            return None;
        }
        let offset = y as usize * self.size.stride + x as usize * 4;
        self.bytes
            .get(offset..offset + 4)
            .map(|p| [p[0], p[1], p[2], p[3]])
    }

    pub fn pixel_mut(&mut self, x: u32, y: u32) -> Option<&mut [u8]> {
        if x >= self.size.width || y >= self.size.height {
            return None;
        }
        let offset = y as usize * self.size.stride + x as usize * 4;
        self.bytes.get_mut(offset..offset + 4)
    }

    pub fn crop(&self, rect: crate::core::RectPx) -> Result<Self, String> {
        let rect = rect.clamp_to(crate::core::RectPx::new(
            0,
            0,
            self.size.width,
            self.size.height,
        ));
        let size = PixelSize::new(rect.width, rect.height)?;
        let mut result = Self::new(size);
        for row in 0..rect.height as usize {
            let source_start = (rect.y as usize + row) * self.size.stride + rect.x as usize * 4;
            let target_start = row * size.stride;
            result.bytes[target_start..target_start + size.stride]
                .copy_from_slice(&self.bytes[source_start..source_start + size.stride]);
        }
        Ok(result)
    }

    pub fn resize_nearest(&self, width: u32, height: u32) -> Result<Self, String> {
        let size = PixelSize::new(width, height)?;
        let mut result = Self::new(size);
        for y in 0..height {
            let source_y = y.saturating_mul(self.height()) / height.max(1);
            for x in 0..width {
                let source_x = x.saturating_mul(self.width()) / width.max(1);
                let pixel = self.pixel(source_x, source_y).unwrap_or([0, 0, 0, 255]);
                result.pixel_mut(x, y).unwrap().copy_from_slice(&pixel);
            }
        }
        Ok(result)
    }
}

/// 解码 grim 的二进制 PPM；像素只在这里从 RGB 转成 RGBA。
pub fn decode_ppm(mut input: impl Read) -> Result<RgbaPixels, String> {
    let mut bytes = Vec::new();
    input
        .read_to_end(&mut bytes)
        .map_err(|error| format!("读取 PPM 失败: {error}"))?;
    let mut cursor = 0;
    if token(&bytes, &mut cursor)? != b"P6" {
        return Err("仅支持二进制 PPM (P6)".into());
    }
    let width = parse_dimension(token(&bytes, &mut cursor)?, "宽度")?;
    let height = parse_dimension(token(&bytes, &mut cursor)?, "高度")?;
    if token(&bytes, &mut cursor)? != b"255" {
        return Err("PPM 最大颜色值必须是 255".into());
    }
    // P6 has exactly one header/data separator. Do not skip all whitespace:
    // the first RGB byte is allowed to be '\n', ' ' or '\0'.
    match bytes.get(cursor) {
        Some(b'\r') if bytes.get(cursor + 1) == Some(&b'\n') => cursor += 2,
        Some(byte) if byte.is_ascii_whitespace() => cursor += 1,
        _ => return Err("PPM 头部缺少像素分隔符".into()),
    }
    let rgb_len = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(3))
        .ok_or_else(|| "PPM 像素尺寸溢出".to_string())?;
    if bytes.len().saturating_sub(cursor) < rgb_len {
        return Err("PPM 像素数据不完整".into());
    }
    let size = PixelSize::new(width, height)?;
    let mut result = RgbaPixels::new(size);
    for (rgb, rgba) in bytes[cursor..cursor + rgb_len]
        .chunks_exact(3)
        .zip(result.bytes.chunks_exact_mut(4))
    {
        rgba[..3].copy_from_slice(rgb);
        rgba[3] = 255;
    }
    Ok(result)
}

fn token<'a>(bytes: &'a [u8], cursor: &mut usize) -> Result<&'a [u8], String> {
    loop {
        while bytes
            .get(*cursor)
            .is_some_and(|byte| byte.is_ascii_whitespace())
        {
            *cursor += 1;
        }
        if bytes.get(*cursor) == Some(&b'#') {
            while bytes.get(*cursor).is_some_and(|byte| *byte != b'\n') {
                *cursor += 1;
            }
        } else {
            break;
        }
    }
    let start = *cursor;
    while bytes
        .get(*cursor)
        .is_some_and(|byte| !byte.is_ascii_whitespace())
    {
        *cursor += 1;
    }
    (start != *cursor)
        .then_some(&bytes[start..*cursor])
        .ok_or_else(|| "PPM 头部不完整".into())
}

fn parse_dimension(value: &[u8], label: &str) -> Result<u32, String> {
    let value = std::str::from_utf8(value).map_err(|_| format!("PPM {label}无效"))?;
    let value = value
        .parse::<u32>()
        .map_err(|_| format!("PPM {label}无效"))?;
    (value > 0)
        .then_some(value)
        .ok_or_else(|| format!("PPM {label}必须大于零"))
}

pub fn read_png(path: &Path) -> Result<RgbaPixels, String> {
    let file = File::open(path).map_err(|error| format!("无法读取 PNG: {error}"))?;
    let decoder = png::Decoder::new(BufReader::new(file));
    let mut reader = decoder
        .read_info()
        .map_err(|error| format!("读取 PNG 头部失败: {error}"))?;
    let mut buffer = vec![0; reader.output_buffer_size().ok_or("PNG 输出尺寸溢出")?];
    let info = reader
        .next_frame(&mut buffer)
        .map_err(|error| format!("解码 PNG 失败: {error}"))?;
    let size = PixelSize::new(info.width, info.height)?;
    let bytes = match info.color_type {
        png::ColorType::Rgba => buffer[..info.buffer_size()].to_vec(),
        png::ColorType::Rgb => buffer[..info.buffer_size()]
            .chunks_exact(3)
            .flat_map(|pixel| [pixel[0], pixel[1], pixel[2], 255])
            .collect(),
        _ => return Err("PNG 必须是 RGB 或 RGBA 图像".into()),
    };
    RgbaPixels::from_rgba(size, bytes)
}

/// 使用 png crate 的按行编码器输出最终文件；编辑器本身不再保留 PNG 中间副本。
pub fn write_png(path: &Path, pixels: &RgbaPixels) -> Result<(), String> {
    let file = File::create(path).map_err(|error| format!("无法创建 PNG: {error}"))?;
    let mut encoder = png::Encoder::new(file, pixels.width(), pixels.height());
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder
        .write_header()
        .map_err(|error| format!("写入 PNG 头部失败: {error}"))?;
    writer
        .write_image_data(&pixels.bytes)
        .map_err(|error| format!("写入 PNG 像素失败: {error}"))
}

pub fn copy_rgba_to_argb8888(source: &[u8], target: &mut [u8]) -> Result<(), String> {
    if source.len() != target.len() || !source.len().is_multiple_of(4) {
        return Err("RGBA/ARGB 缓冲区尺寸不匹配".into());
    }
    for (src, dst) in source.chunks_exact(4).zip(target.chunks_exact_mut(4)) {
        let alpha = u16::from(src[3]);
        dst.copy_from_slice(&[
            ((u16::from(src[2]) * alpha + 127) / 255) as u8,
            ((u16::from(src[1]) * alpha + 127) / 255) as u8,
            ((u16::from(src[0]) * alpha + 127) / 255) as u8,
            src[3],
        ]);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ppm_decoder_handles_binary_whitespace() {
        let mut ppm = b"P6\n2 1\n255\n".to_vec();
        ppm.extend([10, 32, 0, 255, 70, 90]);
        let image = decode_ppm(&ppm[..]).unwrap();
        assert_eq!(image.size, PixelSize::new(2, 1).unwrap());
        assert_eq!(image.bytes, [10, 32, 0, 255, 255, 70, 90, 255]);
        assert!(decode_ppm(&ppm[..ppm.len() - 1]).is_err());
    }

    #[test]
    fn pixels_can_crop_and_resize_without_a_image_crate() {
        let mut image = RgbaPixels::new(PixelSize::new(2, 2).unwrap());
        image
            .bytes
            .copy_from_slice(&[1, 2, 3, 255, 4, 5, 6, 255, 7, 8, 9, 255, 10, 11, 12, 255]);
        let crop = image.crop(crate::core::RectPx::new(1, 0, 1, 2)).unwrap();
        assert_eq!(crop.bytes, [4, 5, 6, 255, 10, 11, 12, 255]);
        assert_eq!(
            image.resize_nearest(1, 1).unwrap().pixel(0, 0),
            Some([1, 2, 3, 255])
        );
    }

    #[test]
    fn premultiplies_argb_pixels() {
        let mut target = [0; 8];
        copy_rgba_to_argb8888(&[255, 10, 20, 128, 1, 2, 3, 255], &mut target).unwrap();
        assert_eq!(target, [10, 5, 128, 128, 3, 2, 1, 255]);
    }
}
