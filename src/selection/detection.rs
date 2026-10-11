use super::{RegionCandidate, area};
use crate::core::RectPx;
use image::{GrayImage, Luma, Rgba, RgbaImage, imageops};
use imageproc::{
    contours::find_contours,
    distance_transform::Norm,
    edges::canny,
    morphology::{close, dilate},
    region_labelling::{Connectivity, connected_components},
};

const MAX_ANALYSIS_DIMENSION: u32 = 1280;
const MAX_PROPOSALS: usize = 4096;
const MAX_REFINED: usize = 512;

#[derive(Clone, Copy, Debug)]
struct AnalysisRect {
    left: u32,
    top: u32,
    right: u32, // 右、下为不包含边界，和 RectPx 保持一致。
    bottom: u32,
}

impl AnalysisRect {
    fn width(self) -> u32 {
        self.right - self.left
    }
    fn height(self) -> u32 {
        self.bottom - self.top
    }
    fn area(self) -> u64 {
        u64::from(self.width()) * u64::from(self.height())
    }
}

#[derive(Clone, Copy)]
struct Proposal {
    rect: AnalysisRect,
    source: &'static str,
}

// 同时保留方向边缘；文字边缘恰好靠近矩形也不能伪装成连续的四条边。
struct EdgeField {
    vertical: GrayImage,
    horizontal: GrayImage,
    combined: GrayImage,
    threshold: u8,
}

pub(super) fn detect(image: &RgbaImage) -> Vec<RegionCandidate> {
    let (width, height) = image.dimensions();
    if width < 3 || height < 3 {
        return Vec::new();
    }
    let (aw, ah) = analysis_size(width, height);
    let resized;
    let analysis = if (aw, ah) == (width, height) {
        image
    } else {
        resized = imageops::resize(image, aw, ah, imageops::FilterType::Triangle);
        &resized
    };
    let field = edge_field(analysis);
    let closed = close(&field.combined, Norm::LInf, 1);
    let mut proposals = contour_proposals(&closed);
    proposals.extend(component_proposals(&closed, false));
    proposals.extend(line_proposals(&field));
    // 边缘围住的背景区域提供面板/分隔布局线索，不要求面板有完整亮色描边。
    let barriers = dilate(&field.combined, Norm::LInf, 1);
    let background = GrayImage::from_fn(aw, ah, |x, y| {
        Luma([if barriers[(x, y)][0] == 0 { 255 } else { 0 }])
    });
    proposals.extend(component_proposals(&background, true));

    let scale = width as f32 / aw as f32;
    let min_width = (12.0 / scale).ceil().max(5.0) as u32;
    let min_height = (12.0 / scale).ceil().max(5.0) as u32;
    let full_area = u64::from(aw) * u64::from(ah);
    proposals.retain(|p| {
        p.rect.width() >= min_width
            && p.rect.height() >= min_height
            && p.rect.area() < full_area * 995 / 1000
    });
    // 小控件与大面板各保留一部分预算，密集文字不能挤掉窗口候选。
    proposals.sort_by_key(|p| p.rect.area());
    if proposals.len() > MAX_PROPOSALS {
        proposals.drain(MAX_PROPOSALS / 2..proposals.len() - MAX_PROPOSALS / 2);
    }

    let mut candidates: Vec<RegionCandidate> = Vec::new();
    for proposal in proposals {
        let rect = proposal.rect;
        let sides = border_sides(&field, rect);
        let mut sorted = sides;
        sorted.sort_by(f32::total_cmp);
        let support = sides.iter().sum::<f32>() / 4.0;
        // 允许圆角、轻微断边和截断窗口，拒绝只被一两条文字线撑起来的外接框。
        if sorted[1] < 0.48 || sorted[0] < 0.18 || support < 0.58 {
            continue;
        }
        if proposal.source == "lines" && (sorted[0] < 0.70 || support < 0.85) {
            continue;
        }
        let density = interior_density(&field.combined, rect);
        let inset_sides = inset_support(&field, rect);
        let compact = rect.width().min(rect.height()) as f32 * scale < 32.0;
        if compact && (density > 0.30 || (density > 0.12 && inset_sides > 0.60)) {
            continue;
        }
        // 文本碎片的边界通常在内外多行重复出现；小控件边框集中在最外侧。
        let texture_penalty = if compact {
            density * 0.30 + inset_sides * 0.18
        } else {
            density * 0.08
        };
        let quality = (support * 0.65 + sorted[0] * 0.25 + 0.10 - texture_penalty).clamp(0.0, 1.0);
        let candidate = RegionCandidate {
            rect: scale_rect(rect, aw, ah, width, height),
            quality,
            sides,
            interior_edges: density,
            sources: vec![proposal.source],
        };
        merge_candidate(&mut candidates, candidate, scale.ceil() as i32 + 1);
    }

    // 只精修有限个高质量候选，原图不创建灰度/梯度整屏副本。
    candidates.sort_by(|a, b| b.quality.total_cmp(&a.quality));
    candidates.truncate(MAX_REFINED);
    let radius = (scale.ceil() as i32 * 3).clamp(3, 16);
    for candidate in &mut candidates {
        candidate.rect = refine_rect(image, candidate.rect, radius, field.threshold);
    }
    let mut refined = Vec::new();
    for candidate in candidates {
        merge_candidate(&mut refined, candidate, 2);
    }
    refined
}

fn analysis_size(width: u32, height: u32) -> (u32, u32) {
    let longest = width.max(height);
    if longest <= MAX_ANALYSIS_DIMENSION {
        return (width, height);
    }
    let scale = MAX_ANALYSIS_DIMENSION as f64 / longest as f64;
    (
        (width as f64 * scale).round().max(3.0) as u32,
        (height as f64 * scale).round().max(3.0) as u32,
    )
}

fn color_difference(a: Rgba<u8>, b: Rgba<u8>) -> u8 {
    (0..3).map(|i| a[i].abs_diff(b[i])).max().unwrap_or(0)
}

fn edge_field(image: &RgbaImage) -> EdgeField {
    let (width, height) = image.dimensions();
    let mut vertical = GrayImage::new(width, height);
    let mut horizontal = GrayImage::new(width, height);
    let mut histogram = [0u32; 256];
    for y in 1..height - 1 {
        for x in 1..width - 1 {
            let center = image[(x, y)];
            let gx = color_difference(image[(x - 1, y)], center)
                .max(color_difference(center, image[(x + 1, y)]));
            let gy = color_difference(image[(x, y - 1)], center)
                .max(color_difference(center, image[(x, y + 1)]));
            vertical[(x, y)] = Luma([gx]);
            horizontal[(x, y)] = Luma([gy]);
            if gx.max(gy) > 2 {
                histogram[gx.max(gy) as usize] += 1;
            }
        }
    }
    // 取中低分位并设上下界：保留弱边框，也不让少量亮文字把全局阈值抬高。
    let count = histogram.iter().sum::<u32>();
    let mut accumulated = 0;
    let percentile = histogram
        .iter()
        .enumerate()
        .find_map(|(value, &n)| {
            accumulated += n;
            (accumulated > count * 35 / 100).then_some(value as u8)
        })
        .unwrap_or(12);
    let threshold = (percentile / 2).clamp(5, 18);
    // 密集亮文字不能掩盖连续弱边框；弱色差仅沿足够长的同方向线段提升。
    let weak_vertical = weak_lines(&vertical, &horizontal, true);
    let weak_horizontal = weak_lines(&horizontal, &vertical, false);
    let high = (f32::from(threshold) * 3.0).clamp(15.0, 54.0);
    let gray = imageops::grayscale(image);
    let mut combined = canny(&gray, high * 0.4, high);
    for y in 0..height {
        for x in 0..width {
            let gx = vertical[(x, y)][0];
            let gy = horizontal[(x, y)][0];
            // 色差边缘补充灰度无法识别的等亮度色块。
            vertical[(x, y)] = Luma([
                if (gx >= threshold && u16::from(gx) * 4 >= u16::from(gy) * 3)
                    || weak_vertical[(x, y)][0] != 0
                {
                    255
                } else {
                    0
                },
            ]);
            horizontal[(x, y)] = Luma([
                if (gy >= threshold && u16::from(gy) * 4 >= u16::from(gx) * 3)
                    || weak_horizontal[(x, y)][0] != 0
                {
                    255
                } else {
                    0
                },
            ]);
            if gx.max(gy) >= threshold || vertical[(x, y)][0] != 0 || horizontal[(x, y)][0] != 0 {
                combined[(x, y)] = Luma([255]);
            }
        }
    }
    EdgeField {
        vertical,
        horizontal,
        combined,
        threshold,
    }
}

fn weak_lines(gradient: &GrayImage, other: &GrayImage, vertical: bool) -> GrayImage {
    let (width, height) = gradient.dimensions();
    let mut mask = GrayImage::new(width, height);
    let (across, along) = if vertical {
        (width, height)
    } else {
        (height, width)
    };
    for fixed in 0..across {
        let mut start = None;
        for position in 0..=along {
            let (x, y) = if vertical {
                (fixed, position)
            } else {
                (position, fixed)
            };
            let hit = position < along
                && gradient[(x, y)][0] >= 5
                && u16::from(gradient[(x, y)][0]) * 4 >= u16::from(other[(x, y)][0]) * 3;
            if hit {
                start.get_or_insert(position);
            } else if let Some(first) = start.take()
                && position - first >= 24
            {
                for along in first..position {
                    let (x, y) = if vertical {
                        (fixed, along)
                    } else {
                        (along, fixed)
                    };
                    mask[(x, y)] = Luma([255]);
                }
            }
        }
    }
    mask
}

fn contour_proposals(edges: &GrayImage) -> Vec<Proposal> {
    find_contours::<i32>(edges)
        .into_iter()
        .filter(|contour| contour.points.len() >= 16)
        .filter_map(|contour| {
            let mut bounds = ComponentBounds::default();
            for point in contour.points {
                if let (Ok(x), Ok(y)) = (u32::try_from(point.x), u32::try_from(point.y)) {
                    bounds.add(x, y);
                }
            }
            bounds.rect().map(|rect| Proposal {
                rect,
                source: "contour",
            })
        })
        .collect()
}

#[derive(Clone, Copy)]
struct ComponentBounds {
    left: u32,
    top: u32,
    right: u32,
    bottom: u32,
    pixels: u32,
}

impl Default for ComponentBounds {
    fn default() -> Self {
        Self {
            left: u32::MAX,
            top: u32::MAX,
            right: 0,
            bottom: 0,
            pixels: 0,
        }
    }
}

impl ComponentBounds {
    fn add(&mut self, x: u32, y: u32) {
        self.left = self.left.min(x);
        self.top = self.top.min(y);
        self.right = self.right.max(x);
        self.bottom = self.bottom.max(y);
        self.pixels += 1;
    }
    fn rect(self) -> Option<AnalysisRect> {
        (self.pixels > 0).then_some(AnalysisRect {
            left: self.left,
            top: self.top,
            right: self.right + 1,
            bottom: self.bottom + 1,
        })
    }
}

fn component_proposals(image: &GrayImage, background: bool) -> Vec<Proposal> {
    let connectivity = if background {
        Connectivity::Four
    } else {
        Connectivity::Eight
    };
    let labels = connected_components(image, connectivity, Luma([0]));
    let count = labels.pixels().map(|p| p[0]).max().unwrap_or(0) as usize;
    let mut bounds = vec![ComponentBounds::default(); count + 1];
    for (x, y, pixel) in labels.enumerate_pixels() {
        if pixel[0] != 0 {
            bounds[pixel[0] as usize].add(x, y);
        }
    }
    bounds
        .into_iter()
        .skip(1)
        .filter_map(|bounds| {
            let rect = bounds.rect()?;
            if bounds.pixels < 24 {
                return None;
            }
            // 不规则的背景连通域不能仅凭外接矩形成为候选。
            if background && f64::from(bounds.pixels) / (rect.area() as f64) < 0.72 {
                return None;
            }
            Some(Proposal {
                rect,
                source: if background {
                    "background"
                } else {
                    "component"
                },
            })
        })
        .collect()
}

#[derive(Clone, Copy, PartialEq)]
struct Segment {
    fixed: u32,
    start: u32,
    end: u32,
}

fn long_segments(edges: &GrayImage, vertical: bool) -> Vec<Segment> {
    let (across, along) = if vertical {
        edges.dimensions()
    } else {
        (edges.height(), edges.width())
    };
    let mut segments = Vec::new();
    for fixed in 0..across {
        let mut start = None;
        let mut last = 0;
        for position in 0..=along {
            let hit = position < along
                && if vertical {
                    edges[(fixed, position)][0] != 0
                } else {
                    edges[(position, fixed)][0] != 0
                };
            if hit {
                start.get_or_insert(position);
                last = position;
            } else if let Some(first) = start
                && (position == along || position - last > 3)
            {
                if last - first >= 24 {
                    segments.push(Segment {
                        fixed,
                        start: first,
                        end: last + 1,
                    });
                }
                start = None;
            }
        }
    }
    segments.sort_by_key(|line| std::cmp::Reverse(line.end - line.start));
    let mut unique: Vec<Segment> = Vec::new();
    for line in segments {
        if !unique.iter().any(|other| {
            line.fixed.abs_diff(other.fixed) <= 2
                && line.start.abs_diff(other.start) <= 5
                && line.end.abs_diff(other.end) <= 5
        }) {
            unique.push(line);
            if unique.len() >= 192 {
                break;
            }
        }
    }
    unique
}

fn line_proposals(field: &EdgeField) -> Vec<Proposal> {
    let mut proposals = Vec::new();
    for vertical in [true, false] {
        let edges = if vertical {
            &field.vertical
        } else {
            &field.horizontal
        };
        let mut segments = long_segments(edges, vertical);
        segments.sort_by_key(|line| line.fixed);
        for (index, first) in segments.iter().enumerate() {
            let aligned: Vec<_> = segments[index + 1..]
                .iter()
                .filter(|second| {
                    let tolerance = ((first.end - first.start).min(second.end - second.start) / 12)
                        .clamp(4, 16);
                    first.fixed.abs_diff(second.fixed) >= 8
                        && first.start.abs_diff(second.start) <= tolerance
                        && first.end.abs_diff(second.end) <= tolerance
                })
                .collect();
            // 相邻边界生成分区，最远边界保留外层窗口；不穷举所有分隔线组合。
            let mut pairs: Vec<_> = aligned.iter().take(2).copied().collect();
            if let Some(last) = aligned.last()
                && !pairs.contains(last)
            {
                pairs.push(*last);
            }
            for second in pairs {
                let tolerance =
                    ((first.end - first.start).min(second.end - second.start) / 12).clamp(4, 16);
                if first.fixed.abs_diff(second.fixed) < 8
                    || first.start.abs_diff(second.start) > tolerance
                    || first.end.abs_diff(second.end) > tolerance
                {
                    continue;
                }
                let lo = first.fixed.min(second.fixed);
                let hi = first.fixed.max(second.fixed) + 1;
                let start = first.start.min(second.start);
                let end = first.end.max(second.end);
                let rect = if vertical {
                    AnalysisRect {
                        left: lo,
                        right: hi,
                        top: start,
                        bottom: end,
                    }
                } else {
                    AnalysisRect {
                        left: start,
                        right: end,
                        top: lo,
                        bottom: hi,
                    }
                };
                proposals.push(Proposal {
                    rect,
                    source: "lines",
                });
            }
        }
    }
    proposals
}

fn border_sides(field: &EdgeField, rect: AnalysisRect) -> [f32; 4] {
    [
        side_support(&field.vertical, rect.left, rect.top, rect.bottom, true),
        side_support(&field.horizontal, rect.top, rect.left, rect.right, false),
        side_support(&field.vertical, rect.right - 1, rect.top, rect.bottom, true),
        side_support(
            &field.horizontal,
            rect.bottom - 1,
            rect.left,
            rect.right,
            false,
        ),
    ]
}

fn side_support(edges: &GrayImage, fixed: u32, start: u32, end: u32, vertical: bool) -> f32 {
    let limit = if vertical {
        edges.width()
    } else {
        edges.height()
    };
    // 屏幕边缘是真实截断面，仍需要其他方向提供足够的检测证据。
    if fixed <= 1 || fixed + 2 >= limit {
        return 0.85;
    }
    let margin = ((end - start) / 10).min(12);
    let start = start + margin;
    let end = end - margin;
    let step = ((end - start) / 160).max(1) as usize;
    let mut best: f32 = 0.0;
    // 必须在同一条直线上连续得到支持；不能拼接邻近文字/曲线的不同位置。
    for across in fixed.saturating_sub(2)..=(fixed + 2).min(limit - 1) {
        let mut supported = 0;
        let mut samples = 0;
        for along in (start..end).step_by(step) {
            let hit = if vertical {
                edges[(across, along)][0] != 0
            } else {
                edges[(along, across)][0] != 0
            };
            supported += u32::from(hit);
            samples += 1;
        }
        best = best.max(supported as f32 / samples.max(1) as f32);
    }
    best
}

fn interior_density(edges: &GrayImage, rect: AnalysisRect) -> f32 {
    let inset = 3;
    if rect.width() <= inset * 2 || rect.height() <= inset * 2 {
        return 0.0;
    }
    let mut count = 0;
    let mut samples = 0;
    for y in (rect.top + inset..rect.bottom - inset).step_by((rect.height() / 32).max(1) as usize) {
        for x in
            (rect.left + inset..rect.right - inset).step_by((rect.width() / 32).max(1) as usize)
        {
            count += u32::from(edges[(x, y)][0] != 0);
            samples += 1;
        }
    }
    count as f32 / samples.max(1) as f32
}

fn inset_support(field: &EdgeField, rect: AnalysisRect) -> f32 {
    if rect.width() < 12 || rect.height() < 12 {
        return 0.0;
    }
    let inner = AnalysisRect {
        left: rect.left + 4,
        top: rect.top + 4,
        right: rect.right - 4,
        bottom: rect.bottom - 4,
    };
    border_sides(field, inner).iter().sum::<f32>() / 4.0
}

fn scale_rect(rect: AnalysisRect, aw: u32, ah: u32, width: u32, height: u32) -> RectPx {
    let convert = |value: u32, from: u32, to: u32| {
        ((u64::from(value) * u64::from(to) + u64::from(from) / 2) / u64::from(from)) as u32
    };
    let left = convert(rect.left, aw, width);
    let top = convert(rect.top, ah, height);
    let right = convert(rect.right, aw, width);
    let bottom = convert(rect.bottom, ah, height);
    RectPx::new(left as i32, top as i32, right - left, bottom - top)
}

fn merge_candidate(
    candidates: &mut Vec<RegionCandidate>,
    candidate: RegionCandidate,
    tolerance: i32,
) {
    if let Some(existing) = candidates
        .iter_mut()
        .find(|c| nearly_same_rect(c.rect, candidate.rect, tolerance))
    {
        let mut sources = existing.sources.clone();
        for source in &candidate.sources {
            if !sources.contains(source) {
                sources.push(source);
            }
        }
        if candidate.quality > existing.quality {
            *existing = candidate;
        }
        existing.sources = sources;
    } else {
        candidates.push(candidate);
    }
}

fn nearly_same_rect(a: RectPx, b: RectPx, tolerance: i32) -> bool {
    // 精确边界的近邻去重，不能按大框尺寸百分比吞掉相邻的层级。
    (a.x - b.x).abs() <= tolerance
        && (a.y - b.y).abs() <= tolerance
        && (a.right() - b.right()).abs() <= tolerance
        && (a.bottom() - b.bottom()).abs() <= tolerance
        && area(a).min(area(b)) as f64 / area(a).max(area(b)) as f64 > 0.8
}

fn refine_rect(image: &RgbaImage, rect: RectPx, radius: i32, threshold: u8) -> RectPx {
    let left = refine_side(
        image,
        rect.x,
        rect.y,
        rect.bottom(),
        true,
        radius,
        threshold,
    );
    let right = refine_side(
        image,
        rect.right(),
        rect.y,
        rect.bottom(),
        true,
        radius,
        threshold,
    );
    let top = refine_side(image, rect.y, left, right, false, radius, threshold);
    let bottom = refine_side(image, rect.bottom(), left, right, false, radius, threshold);
    if right - left < 8 || bottom - top < 8 {
        return rect;
    }
    RectPx::new(left, top, (right - left) as u32, (bottom - top) as u32).clamp_to(RectPx::new(
        0,
        0,
        image.width(),
        image.height(),
    ))
}

fn refine_side(
    image: &RgbaImage,
    original: i32,
    start: i32,
    end: i32,
    vertical: bool,
    radius: i32,
    threshold: u8,
) -> i32 {
    let limit = if vertical {
        image.width()
    } else {
        image.height()
    } as i32;
    if original <= 1 || original >= limit - 1 {
        return original;
    }
    let margin = ((end - start) / 10).clamp(1, 24);
    let start = (start + margin).max(0);
    let along_limit = if vertical {
        image.height()
    } else {
        image.width()
    } as i32;
    let end = (end - margin).min(along_limit);
    let step = ((end - start) / 128).max(1) as usize;
    let mut best = original;
    let mut best_score = 0.0;
    for position in (original - radius).max(1)..=(original + radius).min(limit - 1) {
        let mut count = 0;
        let mut strength = 0.0;
        let mut samples = 0;
        for along in (start..end).step_by(step) {
            let (a, b) = if vertical {
                (
                    image[((position - 1) as u32, along as u32)],
                    image[(position as u32, along as u32)],
                )
            } else {
                (
                    image[(along as u32, (position - 1) as u32)],
                    image[(along as u32, position as u32)],
                )
            };
            let diff = color_difference(a, b);
            count += u32::from(diff >= threshold.min(8));
            strength += f32::from(diff.min(48)) / 48.0;
            samples += 1;
        }
        let coverage = count as f32 / samples.max(1) as f32;
        // 连续边界优先于几个高对比文字像素；距离罚分避免跳到邻近控件。
        let score = coverage * 0.85 + strength / samples.max(1) as f32 * 0.15
            - (position - original).abs() as f32 * 0.008;
        if coverage >= 0.55 && score > best_score {
            best_score = score;
            best = position;
        }
    }
    best
}
