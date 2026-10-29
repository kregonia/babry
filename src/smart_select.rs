//! 轻量级智能选区：只在截图完成后分析一次，悬停时只查询缓存矩形。
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
}

/// 在不引入 image/imageproc 的前提下，从长水平/垂直边缘组成候选矩形。
/// `seeds` 是合成器能提供的窗口几何，始终保留在候选列表中作为稳定兜底。
pub fn detect(image: &ShmImage, seeds: &[RectPx]) -> Vec<Candidate> {
    let (width, height) = (image.width(), image.height());
    if width < 8 || height < 8 {
        return seeds
            .iter()
            .map(|&rect| Candidate {
                rect,
                quality: 0.92,
                seed: true,
            })
            .collect();
    }
    let max_dimension = 800u32;
    let scale = (max_dimension as f32 / width.max(height) as f32).min(1.0);
    let aw = ((width as f32 * scale).round() as usize).max(8);
    let ah = ((height as f32 * scale).round() as usize).max(8);
    let mut luma = vec![0u8; aw * ah];
    let mut rgb = vec![[0u8; 3]; aw * ah];
    for y in 0..ah {
        let sy = (y * height as usize / ah).min(height as usize - 1) as u32;
        for x in 0..aw {
            let sx = (x * width as usize / aw).min(width as usize - 1) as u32;
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
    let target = (aw * ah) as u32 / 10;
    let mut threshold = 20u8;
    for value in (0..=255).rev() {
        seen += histogram[value];
        if seen >= target {
            threshold = threshold.max(value as u8 / 2);
            break;
        }
    }
    let horizontal = line_scores(&gy, aw, ah, threshold, true);
    let vertical = line_scores(&gx, aw, ah, threshold, false);
    let mut h_lines = local_lines(&horizontal, 2, 0.06);
    let mut v_lines = local_lines(&vertical, 2, 0.06);
    h_lines.truncate(24);
    v_lines.truncate(24);
    let to_analysis = |v: i32, full: u32, analysis: usize| {
        ((v.max(0) as f32 / full as f32 * analysis as f32).round() as usize).min(analysis - 1)
    };
    let mut candidates = Vec::new();
    for rect in seeds {
        let rect = rect.clamp_to(RectPx::new(0, 0, width, height));
        if rect.width <= 8 || rect.height <= 8 {
            continue;
        }
        // 合成器给出的窗口几何可能只是估算（例如 niri 不公开滚动布局中的像素位置），
        // 每条边在附近搜索真实的边缘线，证据不足时降级为普通候选。
        let mut sides = [
            to_analysis(rect.x, width, aw),
            to_analysis(rect.y, height, ah),
            to_analysis(rect.right(), width, aw),
            to_analysis(rect.bottom(), height, ah),
        ];
        let radius = (aw.max(ah) / 20).max(4);
        for side in 0..4 {
            let original = sides[side];
            let limit = if side % 2 == 0 { aw } else { ah };
            let mut best = (
                seed_support(&gx, &gy, aw, ah, sides, threshold, side),
                original,
            );
            for position in original.saturating_sub(radius)..=(original + radius).min(limit - 1) {
                let mut trial = sides;
                trial[side] = position;
                if trial[2] <= trial[0] + 8 || trial[3] <= trial[1] + 8 {
                    continue;
                }
                let support = seed_support(&gx, &gy, aw, ah, trial, threshold, side)
                    - original.abs_diff(position) as f32 * 0.002;
                if support > best.0 + 0.05 {
                    best = (support, position);
                }
            }
            sides[side] = best.1;
        }
        let support = (0..4)
            .map(|side| seed_support(&gx, &gy, aw, ah, sides, threshold, side))
            .sum::<f32>()
            / 4.0;
        let back = |v: usize, full: u32, analysis: usize| {
            (v as f32 / analysis as f32 * full as f32).round() as i32
        };
        let (left, top) = (back(sides[0], width, aw), back(sides[1], height, ah));
        let (right, bottom) = (back(sides[2], width, aw), back(sides[3], height, ah));
        // 原始几何本身有边缘支持时保留原值（更精确），否则采用吸附后的结果。
        let refined = if support >= 0.45
            && (left, top, right, bottom) != (rect.x, rect.y, rect.right(), rect.bottom())
        {
            RectPx::new(
                left,
                top,
                (right - left).max(1) as u32,
                (bottom - top).max(1) as u32,
            )
        } else {
            rect
        };
        candidates.push(Candidate {
            rect: refined.clamp_to(RectPx::new(0, 0, width, height)),
            quality: if support >= 0.45 {
                0.92
            } else {
                0.5 + support * 0.4
            },
            seed: support >= 0.45,
        });
    }
    let min_w = aw / 12;
    let min_h = ah / 12;
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
                    if top.position == 0 || left.position == 0 {
                        continue;
                    }
                    // 每条边都必须在自己的跨度上连续成立；文字行只能提供断续的梯度。
                    let sides = [
                        side_support(
                            &gy,
                            aw,
                            ah,
                            top.position,
                            left.position,
                            right.position,
                            threshold,
                        ),
                        side_support(
                            &gy,
                            aw,
                            ah,
                            bottom.position,
                            left.position,
                            right.position,
                            threshold,
                        ),
                        side_support_vertical(
                            &gx,
                            aw,
                            ah,
                            left.position,
                            top.position,
                            bottom.position,
                            threshold,
                        ),
                        side_support_vertical(
                            &gx,
                            aw,
                            ah,
                            right.position,
                            top.position,
                            bottom.position,
                            threshold,
                        ),
                    ];
                    let weakest = sides.iter().copied().fold(1.0_f32, f32::min);
                    let average = sides.iter().sum::<f32>() / 4.0;
                    if weakest < 0.55 || average < 0.68 {
                        continue;
                    }
                    let rect = analysis_to_rect(
                        (left.position, top.position, right.position, bottom.position),
                        (aw, ah),
                        (width, height),
                    );
                    if rect.width < width / 40
                        || rect.height < height / 40
                        || rect.width > width * 995 / 1000
                        || rect.height > height * 995 / 1000
                    {
                        continue;
                    }
                    candidates.push(Candidate {
                        rect,
                        quality: (0.5 + average * 0.45).min(0.95),
                        seed: false,
                    });
                }
            }
        }
    }
    candidates.extend(flat_regions(&rgb, aw, ah, (width, height)));
    drop(rgb);
    // 分析图每个像素对应多个原图像素，吸附半径覆盖量化误差。
    let radius = (width as f32 / aw as f32).ceil() as i32 + 2;
    for candidate in candidates.iter_mut().filter(|c| !c.seed) {
        candidate.rect = refine(image, candidate.rect, radius);
    }
    deduplicate(&mut candidates);
    // 保持由合成器提供的窗口种子优先级；其坐标比纯边缘候选稳定。
    candidates.sort_by_key(|candidate| {
        u64::from(candidate.rect.width) * u64::from(candidate.rect.height)
    });
    candidates.truncate(512);
    candidates
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

fn analysis_to_rect(
    sides: (usize, usize, usize, usize),
    analysis: (usize, usize),
    full: (u32, u32),
) -> RectPx {
    let x = |v: usize| (v as f32 / analysis.0 as f32 * full.0 as f32).round() as i32;
    let y = |v: usize| (v as f32 / analysis.1 as f32 * full.1 as f32).round() as i32;
    let (left, top, right, bottom) = (x(sides.0), y(sides.1), x(sides.2), y(sides.3));
    RectPx::new(
        left,
        top,
        (right - left).max(1) as u32,
        (bottom - top).max(1) as u32,
    )
    .clamp_to(RectPx::new(0, 0, full.0, full.1))
}

fn find(parent: &mut [u32], mut x: u32) -> u32 {
    while parent[x as usize] != x {
        let next = parent[parent[x as usize] as usize];
        parent[x as usize] = next;
        x = next;
    }
    x
}

/// 颜色一致的连通区域：面板、输入框、按钮和代码块通常是纯色背景上的文字。
/// 外接矩形需要被区域本身大面积填充，且四条边都由区域像素构成，排除不规则色块。
fn flat_regions(rgb: &[[u8; 3]], aw: usize, ah: usize, full: (u32, u32)) -> Vec<Candidate> {
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
                if similar(rgb[i], rgb[j]) {
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
    let min_pixels = (n / 2500).max(120) as u32;
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
        if w < 12 || h < 10 || (w * 100 >= aw * 97 && h * 100 >= ah * 97) {
            continue;
        }
        let fill = count as f32 / (w * h) as f32;
        if fill < 0.5 {
            continue;
        }
        let member = |x: usize, y: usize| parent[y * aw + x] == root;
        let row = |y: usize| (left..=right).filter(|&x| member(x, y)).count() as f32 / w as f32;
        let column = |x: usize| (top..=bottom).filter(|&y| member(x, y)).count() as f32 / h as f32;
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
            rect: analysis_to_rect((left, top, right + 1, bottom + 1), (aw, ah), full),
            quality: (0.45 + fill * 0.25 + average * 0.25).min(0.95),
            seed: false,
        });
    }
    result
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
            // 合并重复候选时保留“窗口种子”标记，矩形取证据更强的一方。
            existing.seed |= candidate.seed;
            if candidate.quality > existing.quality {
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
        }
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
        let mut bytes = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let inside = (120..520).contains(&x) && (80..330).contains(&y);
                let v = if inside { 235 } else { 40 };
                let i = ((y * w + x) * 4) as usize;
                bytes[i..i + 4].copy_from_slice(&[v, v, v, 255]);
            }
        }
        let image = ShmImage::from_rgba(
            &RgbaPixels::from_rgba(PixelSize::new(w, h).unwrap(), bytes).unwrap(),
        )
        .unwrap();
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
    }
}
