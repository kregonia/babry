use crate::core::{PointPx, RectPx};
use image::{GrayImage, Luma, RgbaImage, imageops};
use imageproc::{
    contours::find_contours,
    distance_transform::Norm,
    edges::canny,
    morphology::{close, dilate},
    region_labelling::{Connectivity, connected_components},
};

const MAX_ANALYSIS_DIMENSION: u32 = 1280;
const CANNY_LOW_THRESHOLD: f32 = 35.0;
const CANNY_HIGH_THRESHOLD: f32 = 90.0;
const MIN_BORDER_SUPPORT: f32 = 0.16;

#[derive(Clone, Copy, Debug)]
struct Candidate {
    rect: RectPx,
    border_support: f32,
}

/// Detects rectangular visual regions once, then resolves hover points cheaply.
///
/// Canny edges provide the boundaries while contours and connected components
/// produce both compact controls and larger window/panel candidates.
pub struct RegionDetector {
    image_width: u32,
    image_height: u32,
    candidates: Vec<Candidate>,
}

impl RegionDetector {
    pub fn new(image: &RgbaImage) -> Self {
        let (image_width, image_height) = image.dimensions();
        if image_width < 3 || image_height < 3 {
            return Self {
                image_width,
                image_height,
                candidates: Vec::new(),
            };
        }

        let (analysis_width, analysis_height) = analysis_size(image_width, image_height);
        let resized = imageops::resize(
            image,
            analysis_width,
            analysis_height,
            imageops::FilterType::Triangle,
        );
        let gray = imageops::grayscale(&resized);
        let edges = canny(&gray, CANNY_LOW_THRESHOLD, CANNY_HIGH_THRESHOLD);
        let closed_edges = close(&edges, Norm::LInf, 1);

        let mut proposals = contour_proposals(&closed_edges);
        proposals.extend(component_proposals(&closed_edges));

        let candidates = prepare_candidates(
            proposals,
            &edges,
            analysis_width,
            analysis_height,
            image_width,
            image_height,
        );

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

    pub fn region_at(&self, normalized_x: f32, normalized_y: f32) -> RectPx {
        let bounds = RectPx::new(0, 0, self.image_width, self.image_height);
        let point = PointPx {
            x: normalized_x.clamp(0.0, 1.0) * self.image_width as f32,
            y: normalized_y.clamp(0.0, 1.0) * self.image_height as f32,
        };

        self.candidates
            .iter()
            .filter(|candidate| candidate.rect.contains(point))
            .min_by(|first, second| candidate_rank(first).total_cmp(&candidate_rank(second)))
            .map_or(bounds, |candidate| candidate.rect)
    }
}

fn candidate_rank(candidate: &Candidate) -> f32 {
    let area = candidate.rect.width as f32 * candidate.rect.height as f32;
    area * (1.15 - candidate.border_support.min(1.0) * 0.3)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct AnalysisRect {
    left: u32,
    top: u32,
    right: u32,
    bottom: u32,
}

impl AnalysisRect {
    fn new(left: u32, top: u32, right: u32, bottom: u32) -> Option<Self> {
        (right >= left && bottom >= top).then_some(Self {
            left,
            top,
            right,
            bottom,
        })
    }

    fn width(self) -> u32 {
        self.right - self.left + 1
    }

    fn height(self) -> u32 {
        self.bottom - self.top + 1
    }

    fn area(self) -> u64 {
        u64::from(self.width()) * u64::from(self.height())
    }

    fn expand(self, amount: u32, width: u32, height: u32) -> Self {
        Self {
            left: self.left.saturating_sub(amount),
            top: self.top.saturating_sub(amount),
            right: self.right.saturating_add(amount).min(width - 1),
            bottom: self.bottom.saturating_add(amount).min(height - 1),
        }
    }
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

fn contour_proposals(edges: &GrayImage) -> Vec<AnalysisRect> {
    find_contours::<i32>(edges)
        .into_iter()
        .filter(|contour| contour.points.len() >= 12)
        .filter_map(|contour| {
            let mut left = u32::MAX;
            let mut top = u32::MAX;
            let mut right = 0;
            let mut bottom = 0;
            for point in contour.points {
                let Ok(x) = u32::try_from(point.x) else {
                    continue;
                };
                let Ok(y) = u32::try_from(point.y) else {
                    continue;
                };
                left = left.min(x);
                top = top.min(y);
                right = right.max(x);
                bottom = bottom.max(y);
            }
            (left != u32::MAX)
                .then(|| AnalysisRect::new(left, top, right, bottom))
                .flatten()
        })
        .collect()
}

#[derive(Clone, Copy, Debug)]
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

fn component_proposals(edges: &GrayImage) -> Vec<AnalysisRect> {
    let joined = dilate(edges, Norm::LInf, 2);
    let labels = connected_components(&joined, Connectivity::Eight, Luma([0]));
    let label_count = labels.pixels().map(|pixel| pixel[0]).max().unwrap_or(0) as usize;
    let mut bounds = vec![ComponentBounds::default(); label_count + 1];

    for (x, y, pixel) in labels.enumerate_pixels() {
        let label = pixel[0] as usize;
        if label == 0 {
            continue;
        }
        let component = &mut bounds[label];
        component.left = component.left.min(x);
        component.top = component.top.min(y);
        component.right = component.right.max(x);
        component.bottom = component.bottom.max(y);
        component.pixels += 1;
    }

    bounds
        .into_iter()
        .skip(1)
        .filter(|component| component.pixels >= 20 && component.left != u32::MAX)
        .filter_map(|component| {
            AnalysisRect::new(
                component.left.saturating_add(2),
                component.top.saturating_add(2),
                component.right.saturating_sub(2),
                component.bottom.saturating_sub(2),
            )
        })
        .collect()
}

fn prepare_candidates(
    proposals: Vec<AnalysisRect>,
    edges: &GrayImage,
    analysis_width: u32,
    analysis_height: u32,
    image_width: u32,
    image_height: u32,
) -> Vec<Candidate> {
    let analysis_area = u64::from(analysis_width) * u64::from(analysis_height);
    let min_width = (analysis_width / 100).max(16);
    let min_height = (analysis_height / 100).max(12);
    let min_area = (analysis_area / 6000).max(160);
    let max_area = (analysis_area as f64 * 0.995) as u64;

    let mut candidates = proposals
        .into_iter()
        .map(|rect| rect.expand(2, analysis_width, analysis_height))
        .filter(|rect| {
            rect.width() >= min_width
                && rect.height() >= min_height
                && rect.area() >= min_area
                && rect.area() <= max_area
        })
        .filter_map(|rect| {
            let support = border_support(edges, rect);
            (support >= MIN_BORDER_SUPPORT).then_some((rect, support))
        })
        .map(|(rect, border_support)| Candidate {
            rect: scale_rect(
                rect,
                analysis_width,
                analysis_height,
                image_width,
                image_height,
            ),
            border_support,
        })
        .collect::<Vec<_>>();

    candidates.sort_by(|first, second| {
        let first_area = u64::from(first.rect.width) * u64::from(first.rect.height);
        let second_area = u64::from(second.rect.width) * u64::from(second.rect.height);
        first_area
            .cmp(&second_area)
            .then_with(|| second.border_support.total_cmp(&first.border_support))
    });

    let mut deduplicated: Vec<Candidate> = Vec::new();
    for candidate in candidates {
        if let Some(existing) = deduplicated
            .iter_mut()
            .find(|existing| nearly_same_rect(existing.rect, candidate.rect))
        {
            if candidate.border_support > existing.border_support {
                *existing = candidate;
            }
        } else {
            deduplicated.push(candidate);
        }
    }
    deduplicated
}

fn scale_rect(
    rect: AnalysisRect,
    analysis_width: u32,
    analysis_height: u32,
    image_width: u32,
    image_height: u32,
) -> RectPx {
    let left = scale_coordinate(rect.left, analysis_width, image_width);
    let top = scale_coordinate(rect.top, analysis_height, image_height);
    let right = scale_coordinate(rect.right + 1, analysis_width, image_width).max(left + 1);
    let bottom = scale_coordinate(rect.bottom + 1, analysis_height, image_height).max(top + 1);
    RectPx::new(left as i32, top as i32, right - left, bottom - top).clamp_to(RectPx::new(
        0,
        0,
        image_width,
        image_height,
    ))
}

fn scale_coordinate(value: u32, from: u32, to: u32) -> u32 {
    ((u64::from(value) * u64::from(to) + u64::from(from) / 2) / u64::from(from)) as u32
}

fn border_support(edges: &GrayImage, rect: AnalysisRect) -> f32 {
    let horizontal_step = (rect.width() / 180).max(1) as usize;
    let vertical_step = (rect.height() / 120).max(1) as usize;
    let mut supported = 0u32;
    let mut samples = 0u32;

    for x in (rect.left..=rect.right).step_by(horizontal_step) {
        supported += u32::from(edge_near(edges, x, rect.top, 2));
        supported += u32::from(edge_near(edges, x, rect.bottom, 2));
        samples += 2;
    }
    for y in (rect.top..=rect.bottom).step_by(vertical_step) {
        supported += u32::from(edge_near(edges, rect.left, y, 2));
        supported += u32::from(edge_near(edges, rect.right, y, 2));
        samples += 2;
    }

    supported as f32 / samples.max(1) as f32
}

fn edge_near(edges: &GrayImage, x: u32, y: u32, radius: u32) -> bool {
    let left = x.saturating_sub(radius);
    let right = x.saturating_add(radius).min(edges.width() - 1);
    let top = y.saturating_sub(radius);
    let bottom = y.saturating_add(radius).min(edges.height() - 1);
    (top..=bottom)
        .any(|sample_y| (left..=right).any(|sample_x| edges.get_pixel(sample_x, sample_y)[0] != 0))
}

fn nearly_same_rect(first: RectPx, second: RectPx) -> bool {
    let tolerance = ((first
        .width
        .max(first.height)
        .min(second.width.max(second.height)) as f32)
        * 0.025)
        .round()
        .max(3.0) as i32;
    (first.x - second.x).abs() <= tolerance
        && (first.y - second.y).abs() <= tolerance
        && (first.right() - second.right()).abs() <= tolerance
        && (first.bottom() - second.bottom()).abs() <= tolerance
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgba;
    use imageproc::{drawing::draw_hollow_rect_mut, rect::Rect};

    #[test]
    fn falls_back_to_the_full_image_without_edges() {
        let image = RgbaImage::from_pixel(320, 200, Rgba([40, 40, 40, 255]));
        let detector = RegionDetector::new(&image);
        assert_eq!(detector.region_at(0.5, 0.5), RectPx::new(0, 0, 320, 200));
    }

    #[test]
    fn selects_the_smallest_detected_rectangle_under_the_pointer() {
        let mut image = RgbaImage::from_pixel(320, 220, Rgba([25, 25, 25, 255]));
        draw_hollow_rect_mut(
            &mut image,
            Rect::at(30, 20).of_size(260, 180),
            Rgba([220, 220, 220, 255]),
        );
        draw_hollow_rect_mut(
            &mut image,
            Rect::at(90, 70).of_size(120, 70),
            Rgba([240, 240, 240, 255]),
        );

        let detector = RegionDetector::new(&image);
        let selected = detector.region_at(0.45, 0.45);
        assert!(
            selected.x <= 90 && selected.right() >= 210,
            "unexpected horizontal bounds: {selected:?}"
        );
        assert!(
            selected.y <= 70 && selected.bottom() >= 140,
            "unexpected vertical bounds: {selected:?}"
        );
        assert!(selected.width < 200, "unexpected candidate: {selected:?}");
    }
}
