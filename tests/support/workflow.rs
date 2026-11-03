#[test]
#[ignore = "needs an interactive test Wayland compositor; sends pointer and keyboard events"]
fn selection_tools_clipboard_pin_and_shortcuts() {
    let (_temp, dir) = directory("workflow");
    let output = compositor::select_output(&compositor::outputs().unwrap(), None).unwrap();
    let (w, h) = (f64::from(output.width), f64::from(output.height));
    let input = Input::new((output.width, output.height));
    let mut editor = Editor::start(&dir, false);
    let (left, top, right, bottom) = (100., 70., w - 80., h - 235.);
    input.drag((left, top), (right, bottom), 55);
    let selection = native_ui::BoxF {
        x: left as f32,
        y: top as f32,
        width: (right - left) as f32,
        height: (bottom - top) as f32,
    };
    let mut ui = native_ui::EditorUi {
        editing: true,
        ..Default::default()
    };
    let toolbar = ui.toolbar((output.width, output.height), selection);
    let (tx, ty) = (f64::from(toolbar.x), f64::from(toolbar.y));
    // 圆形/方形菜单、色盘、画笔、编号、荧光笔、效果和橡皮擦。
    input.click(tx + 78., ty + 25.);
    let menu = ui.menu_box(
        (output.width, output.height),
        selection,
        native_ui::Menu::Shapes,
    );
    input.click(f64::from(menu.x) + 75., f64::from(menu.y) + 61.);
    ui.tool = 14;
    let a = (left + 25., top + 25.);
    let b = (left + 130., top + 120.);
    input.drag(a, b, 40);
    let circle = screenshot(&output.name, &dir.join("circle.png"));
    input.click(tx + 78., ty + 25.);
    input.click(f64::from(menu.x) + 75., f64::from(menu.y) + 97.);
    input.drag((left + 155., top + 25.), (left + 255., top + 120.), 40);
    input.click(tx + 105., ty + 25.);
    input.click(tx + 295., ty + 68.);
    ui.tool = 1;
    let palette = ui.menu_box(
        (output.width, output.height),
        selection,
        native_ui::Menu::Colors,
    );
    input.click(f64::from(palette.x) + 20., f64::from(palette.y) + 48.);
    input.drag((left + 20., bottom - 35.), (right - 20., bottom - 35.), 70);
    input.click(tx + 177., ty + 25.);
    input.click(right - 65., top + 45.);
    input.click(right - 65., top + 100.);
    input.key(44, "z", true);
    input.key(21, "y", true);
    input.click(tx + 301., ty + 25.);
    input.drag((left + 40., top + 150.), (left + 250., top + 150.), 45);
    input.click(tx + 238., ty + 25.);
    let effects = ui.menu_box(
        (output.width, output.height),
        selection,
        native_ui::Menu::Effects,
    );
    input.click(f64::from(effects.x) + 75., f64::from(effects.y) + 25.);
    input.drag((left + 30., top + 170.), (right - 100., top + 170.), 55);
    input.click(tx + 238., ty + 25.);
    input.click(f64::from(effects.x) + 75., f64::from(effects.y) + 61.);
    input.drag((left + 30., top + 195.), (right - 100., top + 195.), 55);
    input.click(tx + 265., ty + 25.);
    input.drag((left + 180., bottom - 50.), (left + 210., bottom - 20.), 20);
    pause(160);
    let preview = screenshot(&output.name, &dir.join("all-tools.png"));
    // 输出尺寸和已画圆形均保留；使用实际像素而非“进程仍在运行”判定交互成功。
    let sx = f64::from(preview.width()) / w;
    let sy = f64::from(preview.height()) / h;
    let circle_area = RectPx::new(
        ((left + 15.) * sx) as i32,
        ((top + 15.) * sy) as i32,
        (125. * sx) as u32,
        (125. * sy) as u32,
    );
    let (_, circle_pixels) = distance_mask(&circle, &circle, circle_area);
    assert!(circle_pixels > 100, "circle menu did not draw a circle");
    input.key(46, "c", true);
    editor.wait();
    let copied = Command::new("wl-paste")
        .args(["--no-newline", "--type", "image/png"])
        .output()
        .unwrap();
    assert!(copied.status.success(), "clipboard has no PNG");
    let copied_path = dir.join("clipboard.png");
    std::fs::write(&copied_path, copied.stdout).unwrap();
    let image = ShmImage::read_png(&copied_path).unwrap();
    let expected = (
        (right * sx).round() as u32 - (left * sx).round() as u32,
        (bottom * sy).round() as u32 - (top * sy).round() as u32,
    );
    assert_eq!((image.width(), image.height()), expected);
    // 贴图独立进程：等待首帧、移动、停住、反向、四角缩放、关闭。
    let pin_log = File::create(dir.join("pin.log")).unwrap();
    let mut pin = Editor(
        Command::new(binary())
            .arg("--pin")
            .arg(&copied_path)
            .arg("--output")
            .arg(&output.name)
            .stderr(pin_log)
            .spawn()
            .unwrap(),
    );
    pause(400);
    let natural = (
        f64::from(image.width()) / f64::from(output.scale),
        f64::from(image.height()) / f64::from(output.scale),
    );
    let (px, py) = ((w - natural.0) / 2., (h - natural.1) / 2.);
    let center = (px + natural.0 / 2., py + natural.1 / 2.);
    input.drag(center, (center.0 + 25., center.1 + 20.), 20);
    let moved = screenshot(&output.name, &dir.join("pin-moved.png"));
    pause(200);
    let stationary = screenshot(&output.name, &dir.join("pin-stationary.png"));
    let inner = RectPx::new(
        ((px + 45.) * sx) as i32,
        ((py + 40.) * sy) as i32,
        (natural.0.max(80.) * 0.5 * sx) as u32,
        (natural.1.max(80.) * 0.4 * sy) as u32,
    );
    let (lost, count) = distance_mask(&moved, &stationary, inner);
    assert!(count > 100, "pin did not display annotated pixels");
    assert!(lost < 5, "pin drifted after pointer stopped");
    input.drag((px + 27., py + 22.), (px + 57., py + 42.), 20);
    input.move_to(center.0 + 25., center.1 + 20.);
    input.button(0x111, true);
    input.button(0x111, false);
    pin.wait();
    // 默认无工具状态下 Esc 可直接结束下一次截图。
    let mut again = Editor::start(&dir, true);
    input.key(1, "Escape", false);
    again.wait();
    println!(
        "workflow passed on {:?}: {}",
        Backend::detect().unwrap(),
        dir.display()
    );
}
