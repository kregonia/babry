use super::*;
use imageproc::{drawing::draw_filled_rect_mut, rect::Rect};
use slint::{
    LogicalPosition, Rgb8Pixel,
    platform::{
        Platform, PointerEventButton, WindowAdapter, WindowEvent,
        software_renderer::{MinimalSoftwareWindow, RepaintBufferType},
    },
};

struct TestPlatform(Rc<MinimalSoftwareWindow>);
impl Platform for TestPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        Ok(self.0.clone())
    }
}

#[test]
fn wheel_selected_parent_is_committed_by_click_and_drag_still_works() {
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(window.clone()))).unwrap();
    let app = AppWindow::new().unwrap();
    let mut image = RgbaImage::from_pixel(640, 480, Rgba([20, 20, 20, 255]));
    draw_filled_rect_mut(
        &mut image,
        Rect::at(40, 30).of_size(560, 300),
        Rgba([45, 45, 45, 255]),
    );
    draw_filled_rect_mut(
        &mut image,
        Rect::at(120, 90).of_size(140, 50),
        Rgba([75, 75, 75, 255]),
    );
    let detector = RegionDetector::new(&image);
    app.set_selection_loading(true);
    app.set_screenshot(image_to_slint(&image));
    app.show().unwrap();
    window.set_size(PhysicalSize::new(640, 480));
    let mut pixels = vec![Rgb8Pixel::default(); 640 * 480];
    window.draw_if_needed(|renderer| {
        renderer.render(&mut pixels, 640);
    });
    let position = LogicalPosition::new(180.0, 110.0);
    app.window()
        .dispatch_event(WindowEvent::PointerMoved { position });
    assert_eq!(app.get_sw(), 0.0);
    app.set_selection_loading(false);
    install_selection_detector(&app, detector);
    // 后台结果到达时不需要再次移动鼠标就能显示候选。
    assert!((app.get_sw() - 140.0 / 640.0).abs() < 0.005);
    app.window().dispatch_event(WindowEvent::PointerScrolled {
        position,
        delta_x: 0.0,
        delta_y: 120.0,
    });
    assert!((app.get_sw() - 560.0 / 640.0).abs() < 0.005);
    app.window().dispatch_event(WindowEvent::PointerPressed {
        position,
        button: PointerEventButton::Left,
    });
    app.window().dispatch_event(WindowEvent::PointerReleased {
        position,
        button: PointerEventButton::Left,
    });
    assert_eq!(app.get_phase(), 2);
    assert!(
        (app.get_sw() - 560.0 / 640.0).abs() < 0.005,
        "click reset the wheel choice"
    );

    app.set_phase(0);
    let start = LogicalPosition::new(60.0, 60.0);
    let end = LogicalPosition::new(290.0, 210.0);
    app.window().dispatch_event(WindowEvent::PointerPressed {
        position: start,
        button: PointerEventButton::Left,
    });
    app.window()
        .dispatch_event(WindowEvent::PointerMoved { position: end });
    install_selection_detector(&app, RegionDetector::new(&image));
    // 结果刷新不会覆盖鼠标按住期间已经拖出的手动选区。
    assert!((app.get_sw() - 230.0 / 640.0).abs() < 0.001);
    app.window().dispatch_event(WindowEvent::PointerReleased {
        position: end,
        button: PointerEventButton::Left,
    });
    assert_eq!(app.get_phase(), 2);
    assert!((app.get_sx() - 60.0 / 640.0).abs() < 0.001);
    assert!((app.get_sy() - 60.0 / 480.0).abs() < 0.001);
    assert!((app.get_sw() - 230.0 / 640.0).abs() < 0.001);
    assert!((app.get_sh() - 150.0 / 480.0).abs() < 0.001);
    app.hide().unwrap();
}
