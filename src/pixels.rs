//! 拥有所有权的直通 RGBA 像素，用于测试和需要从普通内存导入图像的场景。

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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RgbaPixels {
    pub size: PixelSize,
    pub bytes: Vec<u8>,
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_buffer_sizes() {
        let size = PixelSize::new(2, 3).unwrap();
        assert_eq!(size.bytes(), 24);
        assert!(RgbaPixels::from_rgba(size, vec![0; 23]).is_err());
        assert_eq!(RgbaPixels::new(size).bytes.len(), 24);
    }
}
