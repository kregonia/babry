#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PointPx {
    pub x: f32,
    pub y: f32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RectPx {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl RectPx {
    pub const fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub fn right(self) -> i32 {
        self.x.saturating_add(self.width as i32)
    }

    pub fn bottom(self) -> i32 {
        self.y.saturating_add(self.height as i32)
    }

    pub fn contains(self, point: PointPx) -> bool {
        point.x >= self.x as f32
            && point.y >= self.y as f32
            && point.x < self.right() as f32
            && point.y < self.bottom() as f32
    }

    pub fn clamp_to(self, bounds: RectPx) -> Self {
        let left = self.x.max(bounds.x);
        let top = self.y.max(bounds.y);
        let right = self.right().min(bounds.right()).max(left);
        let bottom = self.bottom().min(bounds.bottom()).max(top);
        Self::new(left, top, (right - left) as u32, (bottom - top) as u32)
    }

    pub fn from_normalized(
        rect: (f32, f32, f32, f32),
        image_width: u32,
        image_height: u32,
    ) -> Self {
        let (x, y, width, height) = rect;
        let x = (x.clamp(0.0, 1.0) * image_width as f32).round() as i32;
        let y = (y.clamp(0.0, 1.0) * image_height as f32).round() as i32;
        let width = (width.max(0.0) * image_width as f32).round() as u32;
        let height = (height.max(0.0) * image_height as f32).round() as u32;
        Self::new(x, y, width, height).clamp_to(Self::new(0, 0, image_width, image_height))
    }

    pub fn to_normalized(self, image_width: u32, image_height: u32) -> (f32, f32, f32, f32) {
        (
            self.x.max(0) as f32 / image_width.max(1) as f32,
            self.y.max(0) as f32 / image_height.max(1) as f32,
            self.width as f32 / image_width.max(1) as f32,
            self.height as f32 / image_height.max(1) as f32,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalized_rect_round_trips() {
        let rect = RectPx::from_normalized((0.1, 0.2, 0.4, 0.5), 1000, 800);
        assert_eq!(rect, RectPx::new(100, 160, 400, 400));
        let normalized = rect.to_normalized(1000, 800);
        assert!((normalized.0 - 0.1).abs() < f32::EPSILON);
        assert!((normalized.3 - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn rect_is_clamped_to_image_bounds() {
        assert_eq!(
            RectPx::from_normalized((0.8, 0.8, 0.5, 0.5), 100, 100),
            RectPx::new(80, 80, 20, 20)
        );
    }
}
