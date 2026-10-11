mod detection;
mod diagnostics;
#[cfg(test)]
mod tests;

use crate::core::{PointPx, RectPx};
use image::RgbaImage;

pub use diagnostics::write_diagnostics;

const MAX_CANDIDATES: usize = 512;

/// 候选框保留检测来源和质量，避免把文字轮廓与完整面板一视同仁。
#[derive(Clone, Debug)]
pub struct RegionCandidate {
    pub rect: RectPx,
    pub quality: f32,
    pub sides: [f32; 4], // 左、上、右、下的方向边界支持度。
    pub interior_edges: f32,
    pub sources: Vec<&'static str>,
}

/// 截图分析只执行一次；悬停与层级切换只查询已缓存的矩形。
pub struct RegionDetector {
    image_width: u32,
    image_height: u32,
    candidates: Vec<RegionCandidate>,
}

impl RegionDetector {
    pub fn new(image: &RgbaImage) -> Self {
        let (image_width, image_height) = image.dimensions();
        let mut candidates = detection::detect(image);
        // 优先保留可靠边界，限制照片和复杂网页产生的候选数量。
        candidates.sort_by(|a, b| {
            b.quality
                .total_cmp(&a.quality)
                .then(area(a.rect).cmp(&area(b.rect)))
        });
        candidates.truncate(MAX_CANDIDATES);
        candidates.sort_by_key(|candidate| area(candidate.rect));
        Self {
            image_width,
            image_height,
            candidates,
        }
    }

    pub const fn image_width(&self) -> u32 {
        self.image_width
    }
    pub const fn image_height(&self) -> u32 {
        self.image_height
    }
    pub fn candidates(&self) -> &[RegionCandidate] {
        &self.candidates
    }

    pub fn region_at(&self, normalized_x: f32, normalized_y: f32) -> RectPx {
        let point = self.point(normalized_x, normalized_y);
        self.candidates
            .iter()
            .filter(|candidate| candidate.rect.contains(point))
            .min_by(|a, b| candidate_rank(a).total_cmp(&candidate_rank(b)))
            .map_or(self.bounds(), |candidate| candidate.rect)
    }

    /// 从小到大返回一条真正包含关系的链，不把互相交叉的框当作父子。
    pub fn regions_at(&self, normalized_x: f32, normalized_y: f32) -> Vec<RectPx> {
        let point = self.point(normalized_x, normalized_y);
        let current = self.region_at(normalized_x, normalized_y);
        let mut chain = vec![current];
        // 从默认候选向内构建子链，交叉的小框不能截断当前候选的父链。
        while let Some(child) = self
            .candidates
            .iter()
            .filter(|candidate| {
                candidate.rect.contains(point)
                    && candidate.rect != chain[0]
                    && contains_rect(chain[0], candidate.rect)
            })
            .max_by_key(|candidate| area(candidate.rect))
        {
            chain.insert(0, child.rect);
        }
        while let Some(parent) = self
            .candidates
            .iter()
            .filter(|candidate| {
                candidate.rect.contains(point)
                    && candidate.rect != *chain.last().unwrap()
                    && contains_rect(candidate.rect, *chain.last().unwrap())
            })
            .min_by_key(|candidate| area(candidate.rect))
        {
            chain.push(parent.rect);
        }
        if chain.last() != Some(&self.bounds()) {
            chain.push(self.bounds());
        }
        chain
    }

    fn bounds(&self) -> RectPx {
        RectPx::new(0, 0, self.image_width, self.image_height)
    }

    fn point(&self, x: f32, y: f32) -> PointPx {
        PointPx {
            x: (x.clamp(0.0, 1.0) * self.image_width as f32)
                .min(self.image_width.saturating_sub(1) as f32),
            y: (y.clamp(0.0, 1.0) * self.image_height as f32)
                .min(self.image_height.saturating_sub(1) as f32),
        }
    }
}

// 可靠边界先进入优先级，再按尺度选择；弱小碎片不能抢走完整控件或面板。
// 同一质量层级使用四次方根面积，仍能优先命中较小的有效子区域。
fn candidate_rank(candidate: &RegionCandidate) -> f32 {
    let tier = if candidate.quality >= 0.80 {
        0.0
    } else {
        10_000.0
    };
    tier + (area(candidate.rect) as f32).sqrt().sqrt() / candidate.quality.max(0.1).powi(2)
}

fn area(rect: RectPx) -> u64 {
    u64::from(rect.width) * u64::from(rect.height)
}

fn contains_rect(parent: RectPx, child: RectPx) -> bool {
    parent.x <= child.x
        && parent.y <= child.y
        && parent.right() >= child.right()
        && parent.bottom() >= child.bottom()
}

/// 会话中的悬停状态：小幅移动不跳框，滚轮选择不会在单击时被重置。
#[derive(Default)]
pub struct SelectionCursor {
    current: Option<RectPx>,
    anchor: Option<PointPx>,
    manual_level: bool,
}

impl SelectionCursor {
    pub fn hover(&mut self, detector: &RegionDetector, x: f32, y: f32) -> RectPx {
        let point = detector.point(x, y);
        let tolerance = if self.manual_level { 6.0 } else { 3.0 };
        if let Some(current) = self.current
            && current.contains(point)
            && self
                .anchor
                .is_some_and(|anchor| (point.x - anchor.x).hypot(point.y - anchor.y) <= tolerance)
        {
            return current;
        }
        let rect = detector.region_at(x, y);
        self.current = Some(rect);
        self.anchor = Some(point);
        self.manual_level = false;
        rect
    }

    /// 正方向扩大到父区域，负方向缩小到子区域；到链末端时保持当前框。
    pub fn step(&mut self, detector: &RegionDetector, x: f32, y: f32, direction: i32) -> RectPx {
        let current = self.hover(detector, x, y);
        let point = detector.point(x, y);
        let hits = detector
            .candidates
            .iter()
            .filter(|candidate| candidate.rect.contains(point) && candidate.rect != current);
        let rect = if direction > 0 {
            hits.filter(|candidate| contains_rect(candidate.rect, current))
                .min_by_key(|candidate| area(candidate.rect))
                .map_or(detector.bounds(), |candidate| candidate.rect)
        } else if direction < 0 {
            hits.filter(|candidate| contains_rect(current, candidate.rect))
                .max_by_key(|candidate| area(candidate.rect))
                .map_or(current, |candidate| candidate.rect)
        } else {
            current
        };
        self.current = Some(rect);
        self.anchor = Some(detector.point(x, y));
        self.manual_level = true;
        rect
    }
}
