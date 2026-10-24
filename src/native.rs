//! Native Wayland building blocks used by the SCTK refactor.
//!
//! This module intentionally keeps pixel ownership explicit: a `ShmFrame` owns one
//! wl_shm canvas and exposes its bytes only while the caller has exclusive access.
//! It is not a claim that wl_shm is zero-copy from a compositor capture source.

use std::{
    fs::File,
    io,
    os::fd::{AsFd, AsRawFd, FromRawFd, RawFd},
};

use smithay_client_toolkit::reexports::client::protocol::{wl_shm, wl_surface};
use smithay_client_toolkit::shm::{Shm, slot::SlotPool};

/// A bounded RGBA pixel surface in the byte order expected by the editor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SurfaceSize {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
}

impl SurfaceSize {
    pub fn new(width: u32, height: u32) -> Result<Self, String> {
        let stride = width
            .checked_mul(4)
            .ok_or_else(|| "共享内存行宽溢出".to_string())?;
        let _ = stride
            .checked_mul(height)
            .ok_or_else(|| "共享内存尺寸溢出".to_string())?;
        Ok(Self {
            width,
            height,
            stride,
        })
    }

    pub fn bytes(self) -> usize {
        self.stride as usize * self.height as usize
    }
}

/// Linux memfd-backed shared memory helper.
///
/// The file is deliberately kept separate from SCTK's pool API so capture and
/// display buffers can later be backed by the same explicit lifetime object.
pub struct SharedMemory {
    file: File,
    size: usize,
}

impl SharedMemory {
    pub fn new(name: &str, size: usize) -> Result<Self, String> {
        if size == 0 {
            return Err("共享内存尺寸必须大于零".into());
        }
        let name = std::ffi::CString::new(name).map_err(|_| "共享内存名称无效")?;
        // memfd_create is Linux-only in the current Wayland target set. Keep the
        // call isolated so the rest of the native renderer remains portable.
        #[cfg(target_os = "linux")]
        let fd = unsafe {
            libc::syscall(
                libc::SYS_memfd_create,
                name.as_ptr(),
                libc::MFD_CLOEXEC as libc::c_uint,
            ) as RawFd
        };
        #[cfg(not(target_os = "linux"))]
        let fd = -1;
        if fd < 0 {
            return Err(io::Error::last_os_error().to_string());
        }
        let file = unsafe { File::from_raw_fd(fd) };
        file.set_len(size as u64)
            .map_err(|error| format!("无法扩展共享内存: {error}"))?;
        Ok(Self { file, size })
    }

    pub fn fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }

    pub fn len(&self) -> usize {
        self.size
    }

    pub fn is_empty(&self) -> bool {
        self.size == 0
    }

    pub fn as_fd(&self) -> impl AsFd + '_ {
        self.file.as_fd()
    }
}

/// A small wrapper around an SCTK slot canvas with a checked pixel view.
pub struct ShmFrame {
    pub size: SurfaceSize,
    pub pool: SlotPool,
    pub buffer: Option<smithay_client_toolkit::shm::slot::Buffer>,
}

impl ShmFrame {
    pub fn allocate(size: SurfaceSize, shm: &Shm) -> Result<Self, String> {
        let pool = SlotPool::new(size.bytes().max(4), shm)
            .map_err(|error| format!("无法创建 wl_shm 池: {error}"))?;
        Ok(Self {
            size,
            pool,
            buffer: None,
        })
    }

    pub fn create_argb_buffer(
        &mut self,
        surface: &wl_surface::WlSurface,
    ) -> Result<&mut [u8], String> {
        let (buffer, canvas) = self
            .pool
            .create_buffer(
                self.size.width as i32,
                self.size.height as i32,
                self.size.stride as i32,
                wl_shm::Format::Argb8888,
            )
            .map_err(|error| format!("无法创建 wl_shm 帧: {error}"))?;
        buffer
            .attach_to(surface)
            .map_err(|error| format!("无法附加 wl_shm 帧: {error}"))?;
        self.buffer = Some(buffer);
        Ok(canvas)
    }

    pub fn damage_and_commit(&self, surface: &wl_surface::WlSurface) {
        surface.damage_buffer(0, 0, self.size.width as i32, self.size.height as i32);
        surface.commit();
    }
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

pub fn copy_rgba_region(
    source: &[u8],
    source_size: SurfaceSize,
    target: &mut [u8],
    target_size: SurfaceSize,
    region: crate::core::RectPx,
) -> Result<(), String> {
    let region = region.clamp_to(crate::core::RectPx::new(
        0,
        0,
        source_size.width,
        source_size.height,
    ));
    if target.len() != target_size.bytes() || source.len() != source_size.bytes() {
        return Err("像素区域来源尺寸不匹配".into());
    }
    if region.width > target_size.width || region.height > target_size.height {
        return Err("像素区域超出目标尺寸".into());
    }
    for row in 0..region.height as usize {
        let src_start =
            (region.y as usize + row) * source_size.stride as usize + region.x as usize * 4;
        let dst_start = row * target_size.stride as usize;
        let length = region.width as usize * 4;
        target[dst_start..dst_start + length]
            .copy_from_slice(&source[src_start..src_start + length]);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_surface_sizes_and_premultiplies_pixels() {
        assert_eq!(SurfaceSize::new(2, 3).unwrap().bytes(), 24);
        assert!(SurfaceSize::new(u32::MAX, 2).is_err());
        let source = [255, 10, 20, 128, 1, 2, 3, 255];
        let mut target = [0; 8];
        copy_rgba_to_argb8888(&source, &mut target).unwrap();
        assert_eq!(target, [10, 5, 128, 128, 3, 2, 1, 255]);
    }

    #[test]
    fn copies_only_checked_rows() {
        let source_size = SurfaceSize::new(3, 2).unwrap();
        let source = (0..source_size.bytes() as u8).collect::<Vec<_>>();
        let target_size = SurfaceSize::new(2, 1).unwrap();
        let mut target = vec![0xaa; target_size.bytes()];
        copy_rgba_region(
            &source,
            source_size,
            &mut target,
            target_size,
            crate::core::RectPx::new(1, 1, 2, 1),
        )
        .unwrap();
        assert_eq!(&target[..8], &[16, 17, 18, 19, 20, 21, 22, 23]);
    }
}
