//! 轻量级智能选区：截图完成后在后台分析一次，悬停时只查询缓存矩形。
//!
//! 分析分两层：整屏粗分析保留桌面、显示器和完整的大面板；每个合成器窗口再以自身
//! 矩形为边界单独分析，像素分辨率相对窗口而不是屏幕，因此按钮、输入框、列表行等
//! 小区域也能达到可检测的尺寸。两层候选共用同一套父子层级，滚轮仍可逐级切换。
use crate::{core::RectPx, shm_image::ShmImage};

#[derive(Clone, Copy, Debug)]
struct Line {
    position: usize,
    score: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Candidate {
    pub rect: RectPx,
    pub quality: f32,
    pub seed: bool,
    /// 焦点窗口：首帧和后台分析完成后的默认选择。
    pub primary: bool,
}

/// 分析参数。整屏和窗口的分析分辨率、阈值和最小尺寸必须分别设定：
/// 整屏要过滤桌面噪声，窗口内则要保留小控件。
#[derive(Clone, Copy)]
struct Profile {
    max_dimension: u32,
    /// 梯度直方图中作为阈值的高分位数百分比。
    percentile: u32,
    floor: u8,
    min_line: f32,
    max_lines: usize,
    /// 线对最小间距的分母，越小允许越密的线条。
    min_gap_div: u32,
    /// 候选最小边长的分母。
    min_rect_div: u32,
    weakest: f32,
    average: f32,
    flat_div: u32,
    flat_min_area: u32,
    flat_min_w: usize,
    flat_min_h: usize,
    /// 是否把分析边界（窗口边框）当成真实边缘。
    bounds_edges: bool,
    /// 是否用相邻分隔线生成整宽行 / 整高列。
    bands: bool,
    band_min_ratio: f32,
    base_quality: f32,
}

const SCREEN: Profile = Profile {
    max_dimension: 800,
    percentile: 10,
    floor: 20,
    min_line: 0.06,
    max_lines: 24,
    min_gap_div: 12,
    min_rect_div: 40,
    weakest: 0.55,
    average: 0.68,
    flat_div: 2500,
    flat_min_area: 120,
    flat_min_w: 12,
    flat_min_h: 10,
    bounds_edges: false,
    bands: false,
    band_min_ratio: 0.0,
    base_quality: 0.5,
};

const WINDOW: Profile = Profile {
    max_dimension: 768,
    percentile: 8,
    floor: 12,
    min_line: 0.04,
    max_lines: 18,
    min_gap_div: 40,
    min_rect_div: 60,
    weakest: 0.45,
    average: 0.56,
    flat_div: 3000,
    flat_min_area: 90,
    flat_min_w: 8,
    flat_min_h: 6,
    bounds_edges: true,
    bands: true,
    band_min_ratio: 0.02,
    base_quality: 0.55,
};

/// 在不引入 image/imageproc 的前提下，从边缘和纯色区域生成候选矩形。
/// `seeds` 是合成器能提供的窗口几何，第一个元素是焦点窗口。
pub fn detect(image: &ShmImage, seeds: &[RectPx]) -> Vec<Candidate> {
    let (width, height) = (image.width(), image.height());
    if width < 8 || height < 8 {
        return seeds
            .iter()
            .enumerate()
            .map(|(index, &rect)| Candidate {
                rect,
                quality: 0.92,
                seed: true,
                primary: index == 0,
            })
            .collect();
    }
    let full = RectPx::new(0, 0, width, height);
    let global = Grid::new(image, full, SCREEN);
    let mut candidates = global.regions();
    let radius = (width as f32 / global.aw as f32).ceil() as i32 + 2;
    refine_all(image, &mut candidates, radius);
    candidates.extend(global.snap_seeds(seeds));
    // 窗口内部分析：以窗口尺寸归一化，细小的控件不会在整屏缩小时消失。
    let mut analyzed: Vec<RectPx> = seeds
        .iter()
        .map(|seed| seed.clamp_to(full))
        .filter(|rect| rect.width >= 48 && rect.height >= 48)
        .filter(|rect| rect.width < width * 98 / 100 || rect.height < height * 98 / 100)
        .collect();
    analyzed.dedup();
    for rect in analyzed {
        let grid = Grid::new(image, rect, WINDOW);
        let mut inner = grid.regions();
        let radius = (rect.width as f32 / grid.aw as f32).ceil() as i32 + 2;
        refine_all(image, &mut inner, radius);
        for candidate in &mut inner {
            candidate.rect = candidate.rect.clamp_to(rect);
        }
        candidates.extend(inner);
    }
    candidates.retain(|candidate| candidate.seed || candidate.quality >= 0.5);
    deduplicate(&mut candidates);
    // 小区域优先，悬停时先命中更具体的控件；种子由合成器提供，保留在列表里。
    candidates.sort_by_key(|candidate| {
        u64::from(candidate.rect.width) * u64::from(candidate.rect.height)
    });
    candidates.truncate(768);
    candidates
}

fn refine_all(image: &ShmImage, candidates: &mut [Candidate], radius: i32) {
    for candidate in candidates.iter_mut().filter(|candidate| !candidate.seed) {
        candidate.rect = refine(image, candidate.rect, radius);
    }
}

pub fn at(candidates: &[Candidate], point: (f32, f32), fallback: RectPx) -> RectPx {
    candidates
        .iter()
        .filter(|candidate| {
            candidate.rect.contains(crate::core::PointPx {
                x: point.0,
                y: point.1,
            }) && (candidate.seed || candidate.quality >= 0.52)
        })
        .min_by(|a, b| candidate_rank(a).total_cmp(&candidate_rank(b)))
        .map(|candidate| candidate.rect)
        .unwrap_or(fallback)
}

/// 滚轮切换：正方向扩大到包含当前框的最小父区域，负方向缩小到最大子区域。
pub fn step(
    candidates: &[Candidate],
    point: (f32, f32),
    current: RectPx,
    bounds: RectPx,
    direction: i32,
) -> RectPx {
    let point = crate::core::PointPx {
        x: point.0,
        y: point.1,
    };
    let area = |r: RectPx| u64::from(r.width) * u64::from(r.height);
    let hits = candidates
        .iter()
        .map(|candidate| candidate.rect)
        .filter(|rect| rect.contains(point) && *rect != current);
    if direction > 0 {
        hits.filter(|rect| contains_rect(*rect, current))
            .min_by_key(|rect| area(*rect))
            .unwrap_or(bounds)
    } else if direction < 0 {
        hits.filter(|rect| contains_rect(current, *rect))
            .max_by_key(|rect| area(*rect))
            .unwrap_or(current)
    } else {
        current
    }
}

fn contains_rect(parent: RectPx, child: RectPx) -> bool {
    parent.x <= child.x
        && parent.y <= child.y
        && parent.right() >= child.right()
        && parent.bottom() >= child.bottom()
}

/// 悬停状态：滚轮选定层级后，小幅移动不会跳回默认候选。
#[derive(Default)]
pub struct Cursor {
    anchor: Option<(f32, f32)>,
    manual: bool,
}

impl Cursor {
    pub fn hover(
        &mut self,
        candidates: &[Candidate],
        point: (f32, f32),
        current: RectPx,
        bounds: RectPx,
    ) -> RectPx {
        let inside = current.contains(crate::core::PointPx {
            x: point.0,
            y: point.1,
        });
        if self.manual
            && inside
            && self
                .anchor
                .is_some_and(|a| (a.0 - point.0).hypot(a.1 - point.1) <= 12.0)
        {
            return current;
        }
        self.manual = false;
        self.anchor = Some(point);
        at(candidates, point, bounds)
    }

    pub fn step(
        &mut self,
        candidates: &[Candidate],
        point: (f32, f32),
        current: RectPx,
        bounds: RectPx,
        direction: i32,
    ) -> RectPx {
        self.manual = true;
        self.anchor = Some(point);
        step(candidates, point, current, bounds, direction)
    }
}

/// 一张降采样后的分析图。所有坐标都是相对 `bounds` 的局部坐标。
struct Grid<'a> {
    image: &'a ShmImage,
    bounds: RectPx,
    aw: usize,
    ah: usize,
    rgb: Vec<[u8; 3]>,
    gx: Vec<u8>,
    gy: Vec<u8>,
    threshold: u8,
    profile: Profile,
}

impl<'a> Grid<'a> {
    fn new(image: &'a ShmImage, bounds: RectPx, profile: Profile) -> Self {
        let bounds = bounds.clamp_to(RectPx::new(0, 0, image.width(), image.height()));
        let bw = bounds.width.max(1) as usize;
        let bh = bounds.height.max(1) as usize;
        let scale = (profile.max_dimension as f32 / bw.max(bh) as f32).min(1.0);
        let aw = ((bw as f32 * scale).round() as usize).clamp(4, bw);
        let ah = ((bh as f32 * scale).round() as usize).clamp(4, bh);
        let mut luma = vec![0u8; aw * ah];
        let mut rgb = vec![[0u8; 3]; aw * ah];
        for y in 0..ah {
            let sy = bounds.y as u32 + (y * bh / ah).min(bh - 1) as u32;
            for x in 0..aw {
                let sx = bounds.x as u32 + (x * bw / aw).min(bw - 1) as u32;
                let [r, g, b, _] = image.rgba(sx, sy);
                rgb[y * aw + x] = [r, g, b];
                luma[y * aw + x] =
                    ((u16::from(r) * 54 + u16::from(g) * 183 + u16::from(b) * 19) / 256) as u8;
            }
        }
        let mut gx = vec![0u8; aw * ah];
        let mut gy = vec![0u8; aw * ah];
        let mut histogram = [0u32; 256];
        for y in 1..ah - 1 {
            for x in 1..aw - 1 {
                let i = y * aw + x;
                let dx = luma[i + 1].abs_diff(luma[i - 1]);
                let dy = luma[i + aw].abs_diff(luma[i - aw]);
                gx[i] = dx;
                gy[i] = dy;
                histogram[usize::from(dx.max(dy))] += 1;
            }
        }
        let mut seen = 0u32;
        let target = ((aw * ah) as u32 * profile.percentile / 100).max(1);
        let mut threshold = profile.floor;
        for value in (0..=255).rev() {
            seen += histogram[value];
            if seen >= target {
                threshold = threshold.max(value as u8 / 2);
                break;
            }
        }
        drop(luma);
        Self {
            image,
            bounds,
            aw,
            ah,
            rgb,
            gx,
            gy,
            threshold,
            profile,
        }
    }

    fn region(&self, left: usize, top: usize, right: usize, bottom: usize) -> RectPx {
        let sx = self.bounds.width as f32 / self.aw as f32;
        let sy = self.bounds.height as f32 / self.ah as f32;
        let x = self.bounds.x + (left as f32 * sx).round() as i32;
        let y = self.bounds.y + (top as f32 * sy).round() as i32;
        let right = self.bounds.x + (right as f32 * sx).round() as i32;
        let bottom = self.bounds.y + (bottom as f32 * sy).round() as i32;
        RectPx::new(x, y, (right - x).max(1) as u32, (bottom - y).max(1) as u32)
            .clamp_to(self.bounds)
    }

    /// 水平边：支持度在整条边跨度上取样；窗口边界本身是真实边缘。
    fn support_horizontal(&self, row: usize, left: usize, right: usize) -> f32 {
        if self.profile.bounds_edges && (row == 0 || row + 1 >= self.ah) {
            return 1.0;
        }
        side_support(&self.gy, self.aw, self.ah, row, left, right, self.threshold)
    }

    fn support_vertical(&self, column: usize, top: usize, bottom: usize) -> f32 {
        if self.profile.bounds_edges && (column == 0 || column + 1 >= self.aw) {
            return 1.0;
        }
        side_support_vertical(
            &self.gx,
            self.aw,
            self.ah,
            column,
            top,
            bottom,
            self.threshold,
        )
    }

    fn regions(&self) -> Vec<Candidate> {
        let h_scores = line_scores(&self.gy, self.aw, self.ah, self.threshold, true);
        let v_scores = line_scores(&self.gx, self.aw, self.ah, self.threshold, false);
        let mut h_lines = local_lines(&h_scores, 2, self.profile.min_line);
        let mut v_lines = local_lines(&v_scores, 2, self.profile.min_line);
        h_lines.truncate(self.profile.max_lines);
        v_lines.truncate(self.profile.max_lines);
        if self.profile.bounds_edges {
            // 贴着窗口边框的侧栏、工具栏没有内部边缘，把边界当成一条线。
            h_lines.push(Line {
                position: 0,
                score: 1.0,
            });
            h_lines.push(Line {
                position: self.ah - 1,
                score: 1.0,
            });
            v_lines.push(Line {
                position: 0,
                score: 1.0,
            });
            v_lines.push(Line {
                position: self.aw - 1,
                score: 1.0,
            });
            h_lines.sort_by_key(|line| line.position);
            h_lines.dedup_by_key(|line| line.position);
            v_lines.sort_by_key(|line| line.position);
            v_lines.dedup_by_key(|line| line.position);
        }
        let mut candidates = Vec::new();
        let min_w = (self.aw / self.profile.min_gap_div.max(1) as usize).max(4);
        let min_h = (self.ah / self.profile.min_gap_div.max(1) as usize).max(4);
        let min_rect_w = (self.bounds.width / self.profile.min_rect_div.max(1)) as usize;
        let min_rect_h = (self.bounds.height / self.profile.min_rect_div.max(1)) as usize;
        for top in &h_lines {
            for bottom in h_lines
                .iter()
                .filter(|line| line.position > top.position + min_h)
            {
                for left in &v_lines {
                    for right in v_lines
                        .iter()
                        .filter(|line| line.position > left.position + min_w)
                    {
                        if !self.profile.bounds_edges && (top.position == 0 || left.position == 0) {
                            continue;
                        }
                        // 每条边都必须在自己的跨度上连续成立；文字行只能提供断续的梯度。
                        let sides = [
                            self.support_horizontal(top.position, left.position, right.position),
                            self.support_horizontal(bottom.position, left.position, right.position),
                            self.support_vertical(left.position, top.position, bottom.position),
                            self.support_vertical(right.position, top.position, bottom.position),
                        ];
                        let weakest = sides.iter().copied().fold(1.0_f32, f32::min);
                        let average = sides.iter().sum::<f32>() / 4.0;
                        if weakest < self.profile.weakest || average < self.profile.average {
                            continue;
                        }
                        let rect = self.region(
                            left.position,
                            top.position,
                            right.position,
                            bottom.position,
                        );
                        if rect.width as usize <= min_rect_w
                            || rect.height as usize <= min_rect_h
                            || rect.width > self.bounds.width * 995 / 1000
                            || rect.height > self.bounds.height * 995 / 1000
                        {
                            continue;
                        }
                        candidates.push(Candidate {
                            rect,
                            quality: (self.profile.base_quality + average * 0.45).min(0.95),
                            seed: false,
                            primary: false,
                        });
                    }
                }
            }
        }
        if self.profile.bands {
            candidates.extend(self.bands(&h_lines, &v_lines));
        }
        candidates.extend(self.flat_regions());
        candidates
    }

    /// 相邻两条长分隔线之间通常就是一行列表、一条工具栏或一列侧栏。
    /// 四边枚举要求四条边都成立，会漏掉这类只有一条共有分隔线的区域。
    fn bands(&self, h_lines: &[Line], v_lines: &[Line]) -> Vec<Candidate> {
        let mut result = Vec::new();
        let min_w = (self.aw as f32 * self.profile.band_min_ratio).max(3.0) as usize;
        let min_h = (self.ah as f32 * self.profile.band_min_ratio).max(3.0) as usize;
        let mut rows: Vec<&Line> = h_lines.iter().collect();
        rows.sort_by_key(|line| line.position);
        for pair in rows.windows(2) {
            let (a, b) = (pair[0], pair[1]);
            let height = b.position.saturating_sub(a.position);
            if height < min_h || height * 10 > self.ah * 9 {
                continue;
            }
            result.push(Candidate {
                rect: self.region(0, a.position, self.aw, b.position),
                quality: self.profile.base_quality + 0.25 * a.score.min(b.score),
                seed: false,
                primary: false,
            });
        }
        let mut columns: Vec<&Line> = v_lines.iter().collect();
        columns.sort_by_key(|line| line.position);
        for pair in columns.windows(2) {
            let (a, b) = (pair[0], pair[1]);
            let width = b.position.saturating_sub(a.position);
            if width < min_w || width * 10 > self.aw * 9 {
                continue;
            }
            result.push(Candidate {
                rect: self.region(a.position, 0, b.position, self.ah),
                quality: self.profile.base_quality + 0.25 * a.score.min(b.score),
                seed: false,
                primary: false,
            });
        }
        result
    }

    /// 颜色一致的连通区域：面板、输入框、按钮和代码块通常是纯色背景上的文字。
    /// 外接矩形需要被区域本身大面积填充，且四条边都由区域像素构成，排除不规则色块。
    fn flat_regions(&self) -> Vec<Candidate> {
        let (aw, ah) = (self.aw, self.ah);
        let n = aw * ah;
        let mut parent: Vec<u32> = (0..n as u32).collect();
        let similar = |a: [u8; 3], b: [u8; 3]| a.iter().zip(b).all(|(x, y)| x.abs_diff(y) <= 3);
        for y in 0..ah {
            for x in 0..aw {
                let i = y * aw + x;
                for j in [
                    (x + 1 < aw).then_some(i + 1),
                    (y + 1 < ah).then_some(i + aw),
                ]
                .into_iter()
                .flatten()
                {
                    if similar(self.rgb[i], self.rgb[j]) {
                        let (a, b) = (find(&mut parent, i as u32), find(&mut parent, j as u32));
                        if a != b {
                            parent[a.max(b) as usize] = a.min(b);
                        }
                    }
                }
            }
        }
        let mut counts = vec![0u32; n];
        for i in 0..n {
            let root = find(&mut parent, i as u32);
            parent[i] = root;
            counts[root as usize] += 1;
        }
        let min_pixels = (n as u32 / self.profile.flat_div.max(1)).max(self.profile.flat_min_area);
        // (根, 像素数, 左, 上, 右, 下)
        let mut regions: Vec<(u32, u32, usize, usize, usize, usize)> = Vec::new();
        for (root, slot) in counts.iter_mut().enumerate() {
            if *slot >= min_pixels {
                regions.push((root as u32, *slot, usize::MAX, usize::MAX, 0, 0));
                // 合格区域的计数槽改存紧凑编号（最高位作标记）。
                *slot = (regions.len() - 1) as u32 | 0x8000_0000;
            }
        }
        for y in 0..ah {
            for x in 0..aw {
                let id = counts[parent[y * aw + x] as usize];
                if id & 0x8000_0000 != 0 {
                    let region = &mut regions[(id & 0x7fff_ffff) as usize];
                    region.2 = region.2.min(x);
                    region.3 = region.3.min(y);
                    region.4 = region.4.max(x);
                    region.5 = region.5.max(y);
                }
            }
        }
        let mut result = Vec::new();
        for &(root, count, left, top, right, bottom) in &regions {
            let (w, h) = (right + 1 - left, bottom + 1 - top);
            if w < self.profile.flat_min_w
                || h < self.profile.flat_min_h
                || (w * 100 >= aw * 97 && h * 100 >= ah * 97)
            {
                continue;
            }
            let fill = count as f32 / (w * h) as f32;
            if fill < 0.5 {
                continue;
            }
            let member = |x: usize, y: usize| parent[y * aw + x] == root;
            let row = |y: usize| (left..=right).filter(|&x| member(x, y)).count() as f32 / w as f32;
            let column =
                |x: usize| (top..=bottom).filter(|&y| member(x, y)).count() as f32 / h as f32;
            // 最外一圈可能是抗锯齿或圆角，同时检查内缩一像素的行列。
            let sides = [
                row(top).max(row((top + 1).min(bottom))),
                row(bottom).max(row(bottom.saturating_sub(1).max(top))),
                column(left).max(column((left + 1).min(right))),
                column(right).max(column(right.saturating_sub(1).max(left))),
            ];
            let weakest = sides.iter().copied().fold(1.0_f32, f32::min);
            let average = sides.iter().sum::<f32>() / 4.0;
            if weakest < 0.35 || average < 0.6 {
                continue;
            }
            result.push(Candidate {
                rect: self.region(left, top, right + 1, bottom + 1),
                quality: (self.profile.base_quality + fill * 0.2 + average * 0.25).min(0.95),
                seed: false,
                primary: false,
            });
        }
        result
    }

    /// 合成器给的窗口尺寸是准确的，只有位置可能因顶栏、滚动偏移而偏差；
    /// 因此只做整体平移，不改变宽高，避免吸附到窗口内部的文字或面板边缘。
    fn snap_seeds(&self, seeds: &[RectPx]) -> Vec<Candidate> {
        let (width, height) = (self.image.width(), self.image.height());
        let mut candidates = Vec::new();
        for (index, rect) in seeds.iter().enumerate() {
            let rect = rect.clamp_to(RectPx::new(0, 0, width, height));
            if rect.width <= 8 || rect.height <= 8 {
                continue;
            }
            let to_analysis = |v: i32, full: u32, analysis: usize| {
                ((v.max(0) as f32 / full as f32 * analysis as f32).round() as usize)
                    .min(analysis - 1)
            };
            let sides = [
                to_analysis(rect.x, width, self.aw),
                to_analysis(rect.y, height, self.ah),
                to_analysis(rect.right(), width, self.aw),
                to_analysis(rect.bottom(), height, self.ah),
            ];
            // 只有焦点窗口的 x/y 位置必须完全可靠：niri 的居中模式、顶栏高度、
            // 列间距都可能和重建假设不同，因此在整屏范围搜索平移量。其他窗口
            // 相对焦点窗口只差累积的间距误差，附近搜索即可。
            let radius = if index == 0 {
                self.aw.max(self.ah) as i32
            } else {
                (self.aw.max(self.ah) / 20).max(4) as i32
            };
            let support = |trial: [usize; 4], side: usize| {
                seed_support(
                    &self.gx,
                    &self.gy,
                    self.aw,
                    self.ah,
                    trial,
                    self.threshold,
                    side,
                )
            };
            // 左右、上下两组边互相独立，可以分别求最佳平移量。
            let shift = |axis: usize| {
                let (first, second) = (axis, axis + 2);
                let base = support(sides, first) + support(sides, second);
                let mut best = (base, 0i32);
                for delta in -radius..=radius {
                    let a = sides[first] as i32 + delta;
                    let b = sides[second] as i32 + delta;
                    let limit = if axis == 0 { self.aw } else { self.ah } as i32;
                    if a < 0 || b >= limit || b - a < 8 {
                        continue;
                    }
                    let mut trial = sides;
                    trial[first] = a as usize;
                    trial[second] = b as usize;
                    let score =
                        support(trial, first) + support(trial, second) - delta.abs() as f32 * 0.002;
                    if score > best.0 {
                        best = (score, delta);
                    }
                }
                if best.0 - base < 0.04 { 0 } else { best.1 }
            };
            let dx = shift(0);
            let dy = shift(1);
            let mut moved = sides;
            moved[0] = (moved[0] as i32 + dx).clamp(0, self.aw as i32 - 1) as usize;
            moved[2] = (moved[2] as i32 + dx).clamp(0, self.aw as i32 - 1) as usize;
            moved[1] = (moved[1] as i32 + dy).clamp(0, self.ah as i32 - 1) as usize;
            moved[3] = (moved[3] as i32 + dy).clamp(0, self.ah as i32 - 1) as usize;
            let average = (0..4).map(|side| support(moved, side)).sum::<f32>() / 4.0;
            let back = |v: usize, full: u32, analysis: usize| {
                (v as f32 / analysis as f32 * full as f32).round() as i32
            };
            let x = back(moved[0], width, self.aw);
            let y = back(moved[1], height, self.ah);
            // 宽高来自合成器，平移不应改变；贴边时由 clamp 截断。
            let refined = RectPx::new(x, y, rect.width, rect.height)
                .clamp_to(RectPx::new(0, 0, width, height));
            candidates.push(Candidate {
                rect: refined,
                quality: if average >= 0.45 {
                    0.92
                } else {
                    0.5 + average * 0.4
                },
                seed: average >= 0.45,
                primary: index == 0,
            });
        }
        candidates
    }
}

#[allow(clippy::too_many_arguments)]
fn seed_support(
    gx: &[u8],
    gy: &[u8],
    aw: usize,
    ah: usize,
    sides: [usize; 4],
    threshold: u8,
    side: usize,
) -> f32 {
    let [left, top, right, bottom] = sides;
    // 屏幕边缘是真实截断面，没有可测量的梯度。
    let at_edge = match side {
        0 => left <= 1,
        1 => top <= 1,
        2 => right + 2 >= aw,
        _ => bottom + 2 >= ah,
    };
    if at_edge {
        return 0.85;
    }
    match side {
        0 => side_support_vertical(gx, aw, ah, left, top, bottom, threshold),
        1 => side_support(gy, aw, ah, top, left, right, threshold),
        2 => side_support_vertical(gx, aw, ah, right, top, bottom, threshold),
        _ => side_support(gy, aw, ah, bottom, left, right, threshold),
    }
}

fn candidate_rank(candidate: &Candidate) -> f32 {
    let area = u64::from(candidate.rect.width) * u64::from(candidate.rect.height);
    let tier = if candidate.seed || candidate.quality >= 0.72 {
        0.0
    } else {
        10_000.0
    };
    tier + (area as f32).sqrt().sqrt() / candidate.quality.max(0.1).powi(2)
}

fn line_scores(
    edges: &[u8],
    width: usize,
    height: usize,
    threshold: u8,
    horizontal: bool,
) -> Vec<f32> {
    let count = if horizontal { height } else { width };
    let span = if horizontal { width } else { height };
    let mut scores = vec![0.0; count];
    for (line, score) in scores.iter_mut().enumerate().take(count - 1).skip(1) {
        // 只统计允许 2 像素缺口的最长连续段：控件边框连续，文字行断续。
        let mut longest = 0usize;
        let mut run = 0usize;
        let mut gap = 0usize;
        for offset in 1..span - 1 {
            let index = if horizontal {
                line * width + offset
            } else {
                offset * width + line
            };
            if edges[index] >= threshold {
                run += 1 + gap;
                gap = 0;
                longest = longest.max(run);
            } else if run > 0 && gap < 2 {
                gap += 1;
            } else {
                run = 0;
                gap = 0;
            }
        }
        *score = longest as f32 / span as f32;
    }
    scores
}

fn local_lines(scores: &[f32], radius: usize, minimum: f32) -> Vec<Line> {
    let mut result: Vec<Line> = Vec::new();
    for position in radius..scores.len().saturating_sub(radius) {
        let score = scores[position];
        if score < minimum
            || (1..=radius)
                .any(|delta| scores[position - delta] > score || scores[position + delta] > score)
        {
            continue;
        }
        if let Some(last) = result.last_mut()
            && position.saturating_sub(last.position) <= radius * 2
        {
            if score > last.score {
                *last = Line { position, score };
            }
        } else {
            result.push(Line { position, score });
        }
    }
    result.sort_by(|a, b| b.score.total_cmp(&a.score));
    result
}

fn side_support(
    edges: &[u8],
    width: usize,
    height: usize,
    row: usize,
    left: usize,
    right: usize,
    threshold: u8,
) -> f32 {
    let start = left.min(width.saturating_sub(1));
    let end = right.min(width.saturating_sub(1)).max(start + 1);
    let step = ((end - start) / 24).max(1);
    let mut total = 0;
    let mut hit = 0;
    for y in row.saturating_sub(1)..=(row + 1).min(height - 1) {
        for x in (start..end).step_by(step) {
            total += 1;
            hit += usize::from(edges[y * width + x] >= threshold);
        }
    }
    hit as f32 / total.max(1) as f32
}
fn side_support_vertical(
    edges: &[u8],
    width: usize,
    height: usize,
    column: usize,
    top: usize,
    bottom: usize,
    threshold: u8,
) -> f32 {
    let start = top.min(height.saturating_sub(1));
    let end = bottom.min(height.saturating_sub(1)).max(start + 1);
    let step = ((end - start) / 24).max(1);
    let mut total = 0;
    let mut hit = 0;
    for y in (start..end).step_by(step) {
        for x in column.saturating_sub(1)..=(column + 1).min(width - 1) {
            total += 1;
            hit += usize::from(edges[y * width + x] >= threshold);
        }
    }
    hit as f32 / total.max(1) as f32
}

fn find(parent: &mut [u32], mut x: u32) -> u32 {
    while parent[x as usize] != x {
        let next = parent[parent[x as usize] as usize];
        parent[x as usize] = next;
        x = next;
    }
    x
}

/// 在原分辨率上把每条边吸附到 ±radius 像素内最强的颜色跳变。
fn refine(image: &ShmImage, rect: RectPx, radius: i32) -> RectPx {
    let (w, h) = (image.width() as i32, image.height() as i32);
    let score = |p: i32, vertical: bool, from: i32, to: i32| -> f32 {
        let limit = if vertical { w } else { h };
        if p <= 0 || p >= limit {
            return 0.0;
        }
        let margin = (to - from) / 10;
        let (from, to) = (from + margin, to - margin);
        let step = ((to - from) / 48).max(1);
        let (mut sum, mut samples) = (0u32, 0u32);
        let mut along = from;
        while along < to {
            let (a, b) = if vertical {
                (
                    image.rgba((p - 1) as u32, along as u32),
                    image.rgba(p as u32, along as u32),
                )
            } else {
                (
                    image.rgba(along as u32, (p - 1) as u32),
                    image.rgba(along as u32, p as u32),
                )
            };
            sum += u32::from(
                a[0].abs_diff(b[0])
                    .max(a[1].abs_diff(b[1]))
                    .max(a[2].abs_diff(b[2])),
            );
            samples += 1;
            along += step;
        }
        sum as f32 / samples.max(1) as f32
    };
    let best = |original: i32, vertical: bool, from: i32, to: i32| -> i32 {
        let limit = if vertical { w } else { h };
        if original <= radius {
            return 0;
        }
        if original >= limit - radius {
            return limit;
        }
        let mut choice = (score(original, vertical, from, to), original);
        for p in original - radius..=original + radius {
            let value = score(p, vertical, from, to) - (p - original).abs() as f32 * 0.5;
            if value > choice.0 {
                choice = (value, p);
            }
        }
        if choice.0 >= 6.0 { choice.1 } else { original }
    };
    let left = best(rect.x, true, rect.y, rect.bottom());
    let right = best(rect.right(), true, rect.y, rect.bottom());
    let top = best(rect.y, false, left, right);
    let bottom = best(rect.bottom(), false, left, right);
    if right - left < 4 || bottom - top < 4 {
        return rect;
    }
    RectPx::new(left, top, (right - left) as u32, (bottom - top) as u32)
}

fn deduplicate(candidates: &mut Vec<Candidate>) {
    let mut result: Vec<Candidate> = Vec::with_capacity(candidates.len());
    for candidate in candidates.drain(..) {
        if let Some(existing) = result
            .iter_mut()
            .find(|other| iou(candidate.rect, other.rect) > 0.92)
        {
            // 合成器给的窗口尺寸比图像证据更可靠：与种子重叠时保留种子矩形，
            // 只合并质量、种子和焦点标记。
            let existing_seed = existing.seed;
            existing.seed |= candidate.seed;
            existing.primary |= candidate.primary;
            if existing_seed && !candidate.seed {
                existing.quality = existing.quality.max(candidate.quality);
            } else if candidate.seed && !existing_seed {
                existing.rect = candidate.rect;
                existing.quality = existing.quality.max(candidate.quality);
            } else if candidate.quality > existing.quality {
                existing.rect = candidate.rect;
                existing.quality = candidate.quality;
            }
        } else {
            result.push(candidate);
        }
    }
    *candidates = result;
}
fn iou(a: RectPx, b: RectPx) -> f32 {
    let left = a.x.max(b.x);
    let top = a.y.max(b.y);
    let right = a.right().min(b.right());
    let bottom = a.bottom().min(b.bottom());
    if right <= left || bottom <= top {
        return 0.0;
    }
    let intersection = (right - left) as f32 * (bottom - top) as f32;
    let union = (a.width * a.height + b.width * b.height) as f32 - intersection;
    intersection / union.max(1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pixels::{PixelSize, RgbaPixels};

    fn candidate(rect: RectPx, seed: bool) -> Candidate {
        Candidate {
            rect,
            quality: 0.9,
            seed,
            primary: false,
        }
    }

    fn image_from(width: u32, height: u32, pixel: impl Fn(u32, u32) -> u8) -> ShmImage {
        let mut bytes = vec![0u8; (width * height * 4) as usize];
        for y in 0..height {
            for x in 0..width {
                let v = pixel(x, y);
                let i = ((y * width + x) * 4) as usize;
                bytes[i..i + 4].copy_from_slice(&[v, v, v, 255]);
            }
        }
        ShmImage::from_rgba(
            &RgbaPixels::from_rgba(PixelSize::new(width, height).unwrap(), bytes).unwrap(),
        )
        .unwrap()
    }

    fn close(rect: RectPx, expected: RectPx, tolerance: i32) -> bool {
        (rect.x - expected.x).abs() <= tolerance
            && (rect.y - expected.y).abs() <= tolerance
            && (rect.right() - expected.right()).abs() <= tolerance
            && (rect.bottom() - expected.bottom()).abs() <= tolerance
    }

    #[test]
    fn hover_prefers_inner_region_and_wheel_walks_the_hierarchy() {
        let bounds = RectPx::new(0, 0, 200, 140);
        let outer = RectPx::new(10, 10, 180, 120);
        let inner = RectPx::new(50, 40, 80, 50);
        let candidates = vec![candidate(outer, true), candidate(inner, false)];
        assert_eq!(at(&candidates, (60.0, 50.0), bounds), inner);
        assert_eq!(step(&candidates, (60.0, 50.0), inner, bounds, 1), outer);
        assert_eq!(step(&candidates, (60.0, 50.0), outer, bounds, 1), bounds);
        assert_eq!(step(&candidates, (60.0, 50.0), bounds, bounds, -1), outer);
        assert_eq!(step(&candidates, (60.0, 50.0), outer, bounds, -1), inner);
        let mut cursor = Cursor::default();
        assert_eq!(
            cursor.step(&candidates, (60.0, 50.0), inner, bounds, 1),
            outer
        );
        // 小幅移动保持滚轮选定的层级。
        assert_eq!(
            cursor.hover(&candidates, (64.0, 52.0), outer, bounds),
            outer
        );
    }

    #[test]
    fn detects_panels_and_snaps_estimated_window_seeds() {
        let (w, h) = (640u32, 400u32);
        let image = image_from(w, h, |x, y| {
            if (120..520).contains(&x) && (80..330).contains(&y) {
                235
            } else {
                40
            }
        });
        // 估算的窗口位置偏了 14 像素。
        let seed = RectPx::new(134, 66, 400, 250);
        let candidates = detect(&image, &[seed]);
        let snapped = candidates.iter().find(|c| c.seed).expect("seed kept");
        for (a, b) in [
            (snapped.rect.x, 120),
            (snapped.rect.y, 80),
            (snapped.rect.right(), 520),
            (snapped.rect.bottom(), 330),
        ] {
            assert!((a - b).abs() <= 3, "{:?}", snapped.rect);
        }
        assert!(!candidates.is_empty());
        assert!(candidates.iter().any(|c| c.primary));
    }

    #[test]
    fn primary_seed_is_located_without_placement_assumptions() {
        // niri 的顶栏高度/居中模式/列间距都可能与重建假设不同，焦点窗口必须能在
        // 整屏范围找到真实位置，而不是只在估算位置附近吸附。
        let (w, h) = (1600u32, 1000u32);
        let window = RectPx::new(300, 420, 500, 300);
        let image = image_from(w, h, |x, y| {
            let (x, y) = (x as i32, y as i32);
            if x >= window.x && x < window.right() && y >= window.y && y < window.bottom() {
                200
            } else {
                30
            }
        });
        // 估算位置垂直偏了 150 像素，超过局部吸附半径。
        let seed = RectPx::new(300, 270, 500, 300);
        let candidates = detect(&image, &[seed]);
        let primary = candidates
            .iter()
            .find(|candidate| candidate.primary)
            .expect("primary seed");
        assert!(close(primary.rect, window, 4), "{:?}", primary.rect);
    }

    #[test]
    fn window_analysis_finds_small_controls_and_bands() {
        let (w, h) = (1600u32, 1000u32);
        let window = RectPx::new(400, 150, 800, 700);
        let panel = RectPx::new(700, 300, 44, 18);
        let row = RectPx::new(400, 600, 800, 36);
        let image = image_from(w, h, |x, y| {
            let (x, y) = (x as i32, y as i32);
            if x >= panel.x && x < panel.right() && y >= panel.y && y < panel.bottom() {
                230
            } else if x >= row.x && x < row.right() && y >= row.y && y < row.bottom() {
                90
            } else if x >= window.x && x < window.right() && y >= window.y && y < window.bottom() {
                45
            } else {
                24
            }
        });
        let candidates = detect(&image, &[window]);
        // 44x18 的面板在整屏 800px 分析里只剩 22x9，窗口内分析才能保留。
        assert!(
            candidates
                .iter()
                .any(|candidate| close(candidate.rect, panel, 4)),
            "small panel missing: {:#?}",
            candidates
                .iter()
                .filter(|c| close(c.rect, panel, 24))
                .collect::<Vec<_>>()
        );
        // 单条分隔线切出的整宽列表行由 bands 生成。
        assert!(
            candidates.iter().any(|candidate| {
                close(candidate.rect, row, 6)
                    || (candidate.rect.y <= row.y + 4
                        && candidate.rect.bottom() >= row.bottom() - 4
                        && candidate.rect.height <= row.height * 2)
            }),
            "row band missing"
        );
    }
}
