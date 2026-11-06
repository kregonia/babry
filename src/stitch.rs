//! 长截图拼接：比较相邻两帧的行特征，求出内容向上滚动的像素数并追加新行。
//!
//! 每帧先找出与上一帧完全相同的顶部/底部行（吸顶标题栏、底部工具栏），只在中间的
//! 滚动区域里匹配位移。行特征是把一行分成最多 64 段后的平均灰度，允许字体渲染或
//! 光标闪烁带来的轻微差异。拼接结果按 RGB 行存储，峰值内存约为“宽 × 总高 × 3”。

/// 单帧 RGB 像素，行紧密排列。
#[derive(Clone, Debug)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub rgb: Vec<u8>,
}

impl Frame {
    pub fn row(&self, y: u32) -> &[u8] {
        let len = self.width as usize * 3;
        &self.rgb[y as usize * len..(y as usize + 1) * len]
    }
}

/// 一次 `push` 的结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Push {
    /// 第一帧，作为拼接起点。
    Started,
    /// 内容向上滚动了若干行，已追加。
    Appended(u32),
    /// 与上一帧相比没有滚动。
    Unchanged,
    /// 找不到可靠的重叠：滚得太快、向上滚动或内容不连续；该帧被丢弃。
    Lost,
    /// 帧尺寸与第一帧不同，已丢弃。
    Mismatch,
    /// 已达到高度上限，不再追加。
    Full,
}

/// 拼接结果的高度上限；超过后忽略继续滚动，避免无限增长的内存占用。
pub const MAX_HEIGHT: u32 = 40_000;
const BINS: usize = 64;
/// 行内段间灰度差低于该值视为空白行，不参与匹配评分。
const DETAIL: u8 = 10;
/// 匹配行的平均每段灰度差上限。
const TOLERANCE: u32 = 3;
const MIN_ROWS: usize = 8;

struct Profile {
    bins: Vec<[u8; BINS]>,
    detail: Vec<bool>,
}

fn profile(frame: &Frame) -> Profile {
    let width = frame.width as usize;
    let bins = BINS.min(width);
    let mut rows = Vec::with_capacity(frame.height as usize);
    let mut detail = Vec::with_capacity(frame.height as usize);
    for y in 0..frame.height {
        let row = frame.row(y);
        let mut out = [0u8; BINS];
        for (bin, value) in out.iter_mut().enumerate().take(bins) {
            let (start, end) = (bin * width / bins, (bin + 1) * width / bins);
            let sum: u32 = row[start * 3..end * 3]
                .chunks_exact(3)
                .map(|p| (u32::from(p[0]) * 77 + u32::from(p[1]) * 150 + u32::from(p[2]) * 29) >> 8)
                .sum();
            *value = (sum / (end - start).max(1) as u32) as u8;
        }
        let used = &out[..bins];
        let max = used.iter().max().copied().unwrap_or(0);
        let min = used.iter().min().copied().unwrap_or(0);
        rows.push(out);
        detail.push(max - min >= DETAIL);
    }
    Profile { bins: rows, detail }
}

fn row_cost(a: &[u8; BINS], b: &[u8; BINS]) -> u32 {
    a.iter()
        .zip(b)
        .map(|(x, y)| u32::from(x.abs_diff(*y)))
        .sum()
}

/// 在两帧的滚动区域 `[top, bottom)` 中寻找内容上移量；返回 `None` 表示没有可靠匹配。
fn find_shift(
    prev: &Profile,
    cur: &Profile,
    top: usize,
    bottom: usize,
    bins: usize,
) -> Option<usize> {
    let span = bottom.saturating_sub(top);
    let min_overlap = (span / 8).max(24);
    let limit = TOLERANCE * bins as u32;
    let mut best: Option<(u64, usize)> = None;
    for shift in 1..span.saturating_sub(min_overlap) {
        let (mut cost, mut rows) = (0u64, 0usize);
        let overlap = span - shift;
        let budget = |rows: usize| u64::from(limit) * rows.max(MIN_ROWS) as u64;
        let mut rejected = false;
        for y in top..bottom - shift {
            if !prev.detail[y + shift] && !cur.detail[y] {
                continue;
            }
            cost += u64::from(row_cost(&prev.bins[y + shift], &cur.bins[y]));
            rows += 1;
            // 已累积的误差超过全部重叠行都允许的总量时提前放弃该位移。
            if cost > u64::from(limit) * overlap as u64 {
                rejected = true;
                break;
            }
        }
        if rejected || rows < MIN_ROWS || cost > budget(rows) {
            continue;
        }
        // 用每行平均误差比较不同位移；乘以 1024 保留小数。
        let score = cost * 1024 / rows as u64;
        if best.is_none_or(|(value, _)| score < value) {
            best = Some((score, shift));
        }
    }
    best.map(|(_, shift)| shift)
}

pub struct Stitcher {
    width: u32,
    frame_height: u32,
    /// 已拼接的 RGB 行。
    rows: Vec<u8>,
    /// 最近一次被接受的帧及其行特征。
    last: Option<(Frame, Profile)>,
    /// `rows` 的末尾对应 `last` 中的这一行（不含）；其后的行在结束时补齐。
    tail: u32,
    full: bool,
}

impl Default for Stitcher {
    fn default() -> Self {
        Self::new()
    }
}

impl Stitcher {
    pub fn new() -> Self {
        Self {
            width: 0,
            frame_height: 0,
            rows: Vec::new(),
            last: None,
            tail: 0,
            full: false,
        }
    }

    /// 当前拼接图的高度（含最后一帧尚未追加的底部行）。
    pub fn height(&self) -> u32 {
        let stride = self.width.max(1) as usize * 3;
        (self.rows.len() / stride) as u32 + self.frame_height.saturating_sub(self.tail)
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn push(&mut self, frame: Frame) -> Push {
        let Some((last, last_profile)) = self.last.as_ref() else {
            self.width = frame.width;
            self.frame_height = frame.height;
            self.tail = frame.height;
            self.rows = frame.rgb.clone();
            let profile = profile(&frame);
            self.last = Some((frame, profile));
            return Push::Started;
        };
        if frame.width != self.width || frame.height != self.frame_height {
            return Push::Mismatch;
        }
        if self.full {
            return Push::Full;
        }
        let height = frame.height as usize;
        let limit = height * 2 / 5;
        let top = (0..height)
            .take_while(|&y| last.row(y as u32) == frame.row(y as u32))
            .count();
        if top == height {
            return Push::Unchanged;
        }
        let top = top.min(limit);
        let bottom = height
            - (0..height)
                .rev()
                .take_while(|&y| last.row(y as u32) == frame.row(y as u32))
                .count()
                .min(limit);
        let cur = profile(&frame);
        let bins = BINS.min(frame.width as usize);
        let Some(shift) = find_shift(last_profile, &cur, top, bottom, bins) else {
            return Push::Lost;
        };
        let stride = self.width as usize * 3;
        // 先让已拼接内容恰好结束在上一帧的滚动区域底部，再追加当前帧新露出的行。
        let bottom = bottom as u32;
        if self.tail > bottom {
            let keep = self.rows.len() - (self.tail - bottom) as usize * stride;
            self.rows.truncate(keep);
        } else {
            for y in self.tail..bottom {
                self.rows.extend_from_slice(last.row(y));
            }
        }
        let current = self.rows.len() / stride;
        let room = (MAX_HEIGHT as usize).saturating_sub(current + height - bottom as usize);
        let added = shift.min(room) as u32;
        for y in bottom - shift as u32..bottom - shift as u32 + added {
            self.rows.extend_from_slice(frame.row(y));
        }
        if added < shift as u32 {
            self.full = true;
        }
        // 达到上限时只追加了一部分新行，以对应的行作为后续补齐底部的起点。
        self.tail = bottom - shift as u32 + added;
        self.last = Some((frame, cur));
        if added == 0 {
            Push::Full
        } else {
            Push::Appended(added)
        }
    }

    /// 补齐最后一帧剩余的底部行，返回 `(宽, 高, RGB)`。
    pub fn finish(mut self) -> Option<(u32, u32, Vec<u8>)> {
        let (last, _) = self.last.take()?;
        for y in self.tail..last.height {
            self.rows.extend_from_slice(last.row(y));
        }
        let height = (self.rows.len() / (self.width as usize * 3)) as u32;
        Some((self.width, height, self.rows))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: u32 = 120;
    const H: u32 = 200;
    const HEADER: u32 = 30;
    const FOOTER: u32 = 20;

    /// 伪随机“文字行”：每 12 行一组，组内有内容的行宽度和亮度各不相同，组间留白。
    fn page(height: u32) -> Vec<u8> {
        let mut seed = 0x9e37_79b9_u32;
        let mut rgb = vec![250u8; (W * height * 3) as usize];
        for y in 0..height {
            if y % 12 >= 8 {
                continue;
            }
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let len = 20 + seed % (W - 30);
            for x in 4..len {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let i = ((y * W + x) * 3) as usize;
                let v = (seed >> 24) as u8 & 0x7f;
                rgb[i..i + 3].copy_from_slice(&[v, v / 2, 255 - v]);
            }
        }
        rgb
    }

    /// 视口在页面偏移 `offset` 处的一帧，顶部和底部覆盖固定栏。
    fn frame(page: &[u8], offset: u32) -> Frame {
        let stride = (W * 3) as usize;
        let mut rgb = page[offset as usize * stride..(offset + H) as usize * stride].to_vec();
        for y in (0..HEADER).chain(H - FOOTER..H) {
            let shade = if y < HEADER { 40 } else { 90 };
            for x in 0..W {
                let i = ((y * W + x) * 3) as usize;
                let mark = if x % 17 == (y % 5) * 3 { 220 } else { shade };
                rgb[i..i + 3].copy_from_slice(&[mark, shade, shade]);
            }
        }
        Frame {
            width: W,
            height: H,
            rgb,
        }
    }

    /// 期望结果：第一帧的顶部栏 + 页面内容 + 最后一帧的底部栏。
    fn expected(page: &[u8], last: u32) -> Vec<u8> {
        let stride = (W * 3) as usize;
        let first = frame(page, 0);
        let end = frame(page, last);
        let mut rgb = first.rgb[..HEADER as usize * stride].to_vec();
        rgb.extend_from_slice(
            &page[HEADER as usize * stride..(last + H - FOOTER) as usize * stride],
        );
        rgb.extend_from_slice(&end.rgb[(H - FOOTER) as usize * stride..]);
        rgb
    }

    #[test]
    fn stitches_scrolling_content_between_fixed_bars() {
        let page = page(1200);
        let mut stitcher = Stitcher::new();
        assert_eq!(stitcher.push(frame(&page, 0)), Push::Started);
        assert_eq!(stitcher.push(frame(&page, 0)), Push::Unchanged);
        assert_eq!(stitcher.push(frame(&page, 37)), Push::Appended(37));
        assert_eq!(stitcher.push(frame(&page, 150)), Push::Appended(113));
        // 一次滚过整个滚动区域，没有重叠可匹配。
        assert_eq!(stitcher.push(frame(&page, 400)), Push::Lost);
        // 向上滚动同样没有向下的匹配，保持上一帧为基准。
        assert_eq!(stitcher.push(frame(&page, 100)), Push::Lost);
        assert_eq!(stitcher.push(frame(&page, 260)), Push::Appended(110));
        assert_eq!(stitcher.push(frame(&page, 261)), Push::Appended(1));
        let (width, height, rgb) = stitcher.finish().unwrap();
        assert_eq!((width, height), (W, 261 + H));
        assert!(rgb == expected(&page, 261), "拼接内容与原页面不一致");
    }

    #[test]
    fn rejects_frames_with_different_size() {
        let page = page(400);
        let mut stitcher = Stitcher::new();
        stitcher.push(frame(&page, 0));
        let mut other = frame(&page, 10);
        other.height -= 1;
        other.rgb.truncate((W * other.height * 3) as usize);
        assert_eq!(stitcher.push(other), Push::Mismatch);
        assert_eq!(stitcher.height(), H);
    }

    #[test]
    fn blank_content_has_no_reliable_shift() {
        let blank = vec![250u8; (W * 600 * 3) as usize];
        let mut stitcher = Stitcher::new();
        stitcher.push(frame(&blank, 0));
        // 滚动区域全是空白，任何位移都同样“匹配”，不能凭空追加。
        let mut moved = frame(&blank, 50);
        moved.rgb[((HEADER + 5) * W * 3) as usize] = 0;
        assert_eq!(stitcher.push(moved), Push::Lost);
    }
}
