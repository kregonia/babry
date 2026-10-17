use super::*;
use babry::core::RectPx;
use image::Rgba;
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

// 可选渲染产物便于检查真实 Slint 组件布局，不影响正常测试和工作区。
fn render_review(window: &MinimalSoftwareWindow, app: &AppWindow, label: &str) {
    let Some(directory) = std::env::var_os("BABRY_UI_REVIEW_DIR") else {
        return;
    };
    let size = app.window().size();
    let mut pixels = vec![Rgb8Pixel::default(); (size.width * size.height) as usize];
    app.window().request_redraw();
    window.draw_if_needed(|renderer| {
        renderer.render(&mut pixels, size.width as usize);
    });
    let image = image::RgbImage::from_fn(size.width, size.height, |x, y| {
        let pixel = pixels[(y * size.width + x) as usize];
        image::Rgb([pixel.r, pixel.g, pixel.b])
    });
    let directory = std::path::PathBuf::from(directory);
    std::fs::create_dir_all(&directory).unwrap();
    image.save(directory.join(format!("{label}.png"))).unwrap();
}

fn move_pointer(app: &AppWindow, x: f32, y: f32) {
    app.window().dispatch_event(WindowEvent::PointerMoved {
        position: LogicalPosition::new(x, y),
    });
}

fn click(app: &AppWindow, x: f32, y: f32) {
    let position = LogicalPosition::new(x, y);
    app.window()
        .dispatch_event(WindowEvent::PointerMoved { position });
    app.window().dispatch_event(WindowEvent::PointerPressed {
        position,
        button: PointerEventButton::Left,
    });
    app.window().dispatch_event(WindowEvent::PointerReleased {
        position,
        button: PointerEventButton::Left,
    });
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

#[test]
fn move_mode_moves_and_resizes_selection_before_editing() {
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(window.clone()))).unwrap();
    let app = AppWindow::new().unwrap();
    let source = Arc::new(RgbaImage::from_pixel(640, 480, Rgba([30, 40, 50, 255])));
    let controller = EditorController::install(&app, source.clone());
    app.set_screenshot(image_to_slint(&source));
    app.set_sx(0.25);
    app.set_sy(0.25);
    app.set_sw(0.5);
    app.set_sh(0.5);
    app.set_phase(2);
    app.invoke_editing_started();
    app.show().unwrap();
    window.set_size(PhysicalSize::new(640, 480));

    // 编辑阶段默认是移动模式：选区内部拖动不会创建标注。
    let move_start = LogicalPosition::new(300.0, 200.0);
    let move_end = LogicalPosition::new(340.0, 220.0);
    app.window().dispatch_event(WindowEvent::PointerPressed {
        position: move_start,
        button: PointerEventButton::Left,
    });
    app.window()
        .dispatch_event(WindowEvent::PointerMoved { position: move_end });
    app.window().dispatch_event(WindowEvent::PointerReleased {
        position: move_end,
        button: PointerEventButton::Left,
    });
    assert!((app.get_sx() - 0.3125).abs() < 0.001);
    assert!((app.get_sy() - (140.0 / 480.0)).abs() < 0.001);
    assert_eq!(controller.document.borrow().marks().len(), 0);

    // 右下角拖动同时改变宽高；控制点命中不受当前移动/编辑模式影响。
    let resize_start = LogicalPosition::new(520.0, 380.0);
    let resize_end = LogicalPosition::new(560.0, 420.0);
    app.window().dispatch_event(WindowEvent::PointerPressed {
        position: resize_start,
        button: PointerEventButton::Left,
    });
    app.window().dispatch_event(WindowEvent::PointerMoved {
        position: resize_end,
    });
    app.window().dispatch_event(WindowEvent::PointerReleased {
        position: resize_end,
        button: PointerEventButton::Left,
    });
    assert!((app.get_sw() - (360.0 / 640.0)).abs() < 0.001);
    assert!((app.get_sh() - (280.0 / 480.0)).abs() < 0.001);
    assert_eq!(
        controller.document.borrow().region(),
        RectPx::new(200, 140, 360, 280)
    );

    // 工具栏没有独立模式入口：直接点击矩形即启用，重复点击同一按钮会取消。
    let edit_button = LogicalPosition::new(app.get_toolbar_x() + 50.0, app.get_toolbar_y() + 25.0);
    app.window().dispatch_event(WindowEvent::PointerMoved {
        position: edit_button,
    });
    app.window().dispatch_event(WindowEvent::PointerPressed {
        position: edit_button,
        button: PointerEventButton::Left,
    });
    app.window().dispatch_event(WindowEvent::PointerReleased {
        position: edit_button,
        button: PointerEventButton::Left,
    });
    assert!(app.get_editing_mode());
    app.set_tool(2);
    let draw_start = LogicalPosition::new(260.0, 190.0);
    let draw_end = LogicalPosition::new(400.0, 300.0);
    app.window().dispatch_event(WindowEvent::PointerPressed {
        position: draw_start,
        button: PointerEventButton::Left,
    });
    app.window()
        .dispatch_event(WindowEvent::PointerMoved { position: draw_end });
    app.window().dispatch_event(WindowEvent::PointerReleased {
        position: draw_end,
        button: PointerEventButton::Left,
    });
    assert!((app.get_sx() - (200.0 / 640.0)).abs() < 0.001);
    assert_eq!(
        controller.document.borrow().region(),
        RectPx::new(200, 140, 360, 280)
    );
    assert_eq!(controller.document.borrow().marks().len(), 1);
    click(&app, app.get_toolbar_x() + 50.0, app.get_toolbar_y() + 25.0);
    assert_eq!(
        app.get_tool(),
        0,
        "clicking the same tool did not clear selection"
    );
    assert!(!app.get_editing_mode());
    let start = LogicalPosition::new(310.0, 220.0);
    let end = LogicalPosition::new(330.0, 230.0);
    app.window().dispatch_event(WindowEvent::PointerPressed {
        position: start,
        button: PointerEventButton::Left,
    });
    move_pointer(&app, end.x, end.y);
    app.window().dispatch_event(WindowEvent::PointerReleased {
        position: end,
        button: PointerEventButton::Left,
    });
    assert_eq!(
        controller.document.borrow().region(),
        RectPx::new(220, 150, 360, 280)
    );
    assert_eq!(
        controller.document.borrow().marks().len(),
        1,
        "moving created another mark"
    );
    app.hide().unwrap();
}

#[test]
fn toolbar_menus_dragging_numbering_and_numeric_width_work_through_pointer_events() {
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(window.clone()))).unwrap();
    let app = AppWindow::new().unwrap();
    let source = Arc::new(RgbaImage::from_pixel(1000, 700, Rgba([20, 30, 40, 255])));
    let controller = EditorController::install(&app, source.clone());
    app.set_screenshot(image_to_slint(&source));
    app.set_sx(0.1);
    app.set_sy(0.1);
    app.set_sw(0.6);
    app.set_sh(0.4);
    app.set_phase(2);
    app.invoke_editing_started();
    app.show().unwrap();
    window.set_size(PhysicalSize::new(1000, 700));
    assert_eq!(app.get_tool(), 0);
    let (bar_x, bar_y) = (app.get_toolbar_x(), app.get_toolbar_y());
    render_review(&window, &app, "toolbar-idle");
    // 形状箭头打开窗口浮层；选择圆形会启用，重复点击主按钮才会取消。
    click(&app, bar_x + 78.0, bar_y + 25.0);
    assert_eq!(app.get_open_menu(), 1);
    render_review(&window, &app, "toolbar-shapes");
    click(&app, bar_x + 65.0, bar_y - 64.0);
    assert_eq!(app.get_shape_tool(), 14);
    assert_eq!(app.get_tool(), 14);
    assert_eq!(app.get_open_menu(), 0);
    click(&app, bar_x + 50.0, bar_y + 25.0);
    assert_eq!(app.get_tool(), 0);
    click(&app, bar_x + 78.0, bar_y + 25.0);
    click(&app, bar_x + 65.0, bar_y - 28.0);
    assert_eq!(app.get_shape_tool(), 13);
    assert_eq!(app.get_tool(), 13);
    // 效果菜单从马赛克切换到高斯模糊不会取消工具。
    click(&app, bar_x + 238.0, bar_y + 25.0);
    assert_eq!(app.get_open_menu(), 2);
    render_review(&window, &app, "toolbar-effects");
    click(&app, bar_x + 225.0, bar_y - 28.0);
    assert_eq!(app.get_effect_mode(), 1);
    assert_eq!(app.get_tool(), 8);
    click(&app, bar_x + 211.0, bar_y + 25.0);
    assert_eq!(app.get_tool(), 0);
    // 点击数字工具，再在画布连续点两次：实际历史和工具提示都递增。
    click(&app, bar_x + 177.0, bar_y + 25.0);
    assert_eq!(app.get_tool(), 11);
    click(&app, 300.0, 180.0);
    click(&app, 400.0, 180.0);
    assert_eq!(
        controller
            .document
            .borrow()
            .marks()
            .iter()
            .map(|mark| mark.number)
            .collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert_eq!(app.get_next_number(), 3);
    app.invoke_undo();
    assert_eq!(app.get_next_number(), 2);
    app.invoke_redo();
    assert_eq!(app.get_next_number(), 3);
    click(&app, bar_x + 177.0, bar_y + 25.0);
    assert_eq!(app.get_tool(), 0);
    // 只拖动点阵：工具栏移动，选区和标注历史保持不变。
    let before = controller.document.borrow().region();
    let grip = LogicalPosition::new(bar_x + 20.0, bar_y + 25.0);
    move_pointer(&app, grip.x, grip.y);
    app.window().dispatch_event(WindowEvent::PointerPressed {
        position: grip,
        button: PointerEventButton::Left,
    });
    move_pointer(&app, grip.x + 80.0, grip.y + 100.0);
    app.window().dispatch_event(WindowEvent::PointerReleased {
        position: LogicalPosition::new(grip.x + 80.0, grip.y + 100.0),
        button: PointerEventButton::Left,
    });
    assert!((app.get_toolbar_x() - bar_x - 80.0).abs() < 1.0);
    assert!((app.get_toolbar_y() - bar_y - 100.0).abs() < 1.0);
    assert_eq!(controller.document.borrow().region(), before);
    assert_eq!(controller.document.borrow().marks().len(), 2);
    assert_eq!(app.get_tool(), 0);
    // 画笔粗细右侧数字输入框支持键盘直接输入，与 1～64px 的滑块共用状态。
    let (bar_x, bar_y) = (app.get_toolbar_x(), app.get_toolbar_y());
    click(&app, bar_x + 104.0, bar_y + 25.0);
    assert_eq!(app.get_tool(), 1);
    click(&app, bar_x + 215.0, bar_y + 70.0);
    use slint::platform::Key;
    app.window().dispatch_event(WindowEvent::KeyPressed {
        text: Key::Control.into(),
    });
    app.window()
        .dispatch_event(WindowEvent::KeyPressed { text: "a".into() });
    app.window()
        .dispatch_event(WindowEvent::KeyReleased { text: "a".into() });
    app.window().dispatch_event(WindowEvent::KeyReleased {
        text: Key::Control.into(),
    });
    for character in ["3", "2"] {
        app.window().dispatch_event(WindowEvent::KeyPressed {
            text: character.into(),
        });
        app.window().dispatch_event(WindowEvent::KeyReleased {
            text: character.into(),
        });
    }
    assert_eq!(app.get_line_width(), 32);
    render_review(&window, &app, "toolbar-pen");
    app.hide().unwrap();
}

#[test]
fn all_selection_edges_and_corners_resize() {
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(window.clone()))).unwrap();
    let app = AppWindow::new().unwrap();
    let source = Arc::new(RgbaImage::from_pixel(640, 480, Rgba([30, 40, 50, 255])));
    let _controller = EditorController::install(&app, source.clone());
    app.set_screenshot(image_to_slint(&source));
    app.set_phase(2);
    app.show().unwrap();
    window.set_size(PhysicalSize::new(640, 480));

    let drag = |start: (f32, f32), end: (f32, f32)| {
        app.set_sx(0.25);
        app.set_sy(0.25);
        app.set_sw(0.5);
        app.set_sh(0.5);
        let start = LogicalPosition::new(start.0, start.1);
        let end = LogicalPosition::new(end.0, end.1);
        app.window().dispatch_event(WindowEvent::PointerPressed {
            position: start,
            button: PointerEventButton::Left,
        });
        app.window()
            .dispatch_event(WindowEvent::PointerMoved { position: end });
        app.window().dispatch_event(WindowEvent::PointerReleased {
            position: end,
            button: PointerEventButton::Left,
        });
    };
    let cases = [
        ((160.0, 240.0), (128.0, 240.0), (0.20, 0.25, 0.55, 0.50)), // 左
        (
            (320.0, 120.0),
            (320.0, 88.0),
            (0.25, 88.0 / 480.0, 0.50, 0.5666667),
        ), // 上
        ((480.0, 240.0), (512.0, 240.0), (0.25, 0.25, 0.55, 0.50)), // 右
        (
            (320.0, 360.0),
            (320.0, 392.0),
            (0.25, 0.25, 0.50, 0.5666667),
        ), // 下
        (
            (160.0, 120.0),
            (128.0, 88.0),
            (0.20, 88.0 / 480.0, 0.55, 0.5666667),
        ), // 左上
        (
            (480.0, 120.0),
            (512.0, 88.0),
            (0.25, 88.0 / 480.0, 0.55, 0.5666667),
        ), // 右上
        (
            (480.0, 360.0),
            (512.0, 392.0),
            (0.25, 0.25, 0.55, 0.5666667),
        ), // 右下
        (
            (160.0, 360.0),
            (128.0, 392.0),
            (0.20, 0.25, 0.55, 0.5666667),
        ), // 左下
    ];
    for (start, end, expected) in cases {
        drag(start, end);
        let actual = (app.get_sx(), app.get_sy(), app.get_sw(), app.get_sh());
        assert!(
            (actual.0 - expected.0).abs() < 0.002
                && (actual.1 - expected.1).abs() < 0.002
                && (actual.2 - expected.2).abs() < 0.002
                && (actual.3 - expected.3).abs() < 0.002,
            "resize {start:?} -> {end:?}: actual {actual:?}, expected {expected:?}"
        );
    }
    app.hide().unwrap();
}

#[test]
fn editor_uses_local_images_and_keeps_pending_operations_out_of_exports() {
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(TestPlatform(window.clone()))).unwrap();
    let app = AppWindow::new().unwrap();
    let source = Arc::new(RgbaImage::from_pixel(1280, 960, Rgba([20, 30, 40, 255])));
    let controller = EditorController::install(&app, source.clone());
    app.set_screenshot(image_to_slint(&source));
    app.show().unwrap();
    window.set_size(PhysicalSize::new(640, 480));
    assert!(controller.document.borrow().committed().is_none());
    assert_eq!(app.get_committed_image().size(), (0, 0).into());
    assert!(
        app.get_text_font_family().is_empty(),
        "annotation font was registered before text input"
    );
    app.set_sx(0.125);
    app.set_sy(0.125);
    app.set_sw(0.5);
    app.set_sh(0.5);
    app.set_phase(2);
    app.invoke_editing_started();
    assert!(controller.document.borrow().committed().is_none());
    let red = slint::Color::from_rgb_u8(255, 0, 0);
    app.invoke_begin_mark(2, 0.2, 0.2, red, 4, 0, 8);
    for i in 0..100 {
        app.invoke_update_mark(0.3 + i as f32 / 2000.0, 0.4);
    }
    controller.flush_preview(&app);
    assert!(app.get_preview_image().size().width < 640);
    assert!(app.get_preview_image().size().height < 480);
    assert!(controller.document.borrow().committed().is_none());
    app.invoke_cancel_mark();
    assert!(!app.get_has_preview());
    assert_eq!(app.get_preview_image().size(), (0, 0).into());
    app.invoke_begin_mark(2, 0.2, 0.2, red, 4, 0, 8);
    app.invoke_update_mark(0.4, 0.4);
    app.invoke_commit_mark();
    assert!(app.get_has_committed());
    assert_eq!(app.get_committed_image().size(), (640, 480).into());
    assert_eq!(controller.document.borrow().marks().len(), 1);
    app.invoke_undo();
    assert!(!app.get_has_committed());
    assert!(controller.document.borrow().committed().is_none());
    app.invoke_redo();
    assert!(app.get_has_committed());
    app.invoke_begin_text(0.3, 0.3, red, 24, 0.2);
    app.invoke_update_text("Babry".into());
    app.invoke_update_text_style(red, 32, 0.2);
    app.invoke_commit_text("Babry".into());
    assert_eq!(controller.document.borrow().marks().len(), 2);
    let output = tempfile::tempdir().unwrap();
    controller
        .document
        .borrow()
        .export(&output.path().join("committed.png"))
        .unwrap();
    assert_eq!(
        image::open(output.path().join("committed.png"))
            .unwrap()
            .to_rgba8(),
        *controller.document.borrow().committed().unwrap()
    );
    app.hide().unwrap();
}
