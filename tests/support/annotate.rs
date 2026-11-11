/// 箭头/直线工具与“无工具时拖动已有标注”的真实交互：画箭头 → Esc 退出工具 →
/// 拖动箭头 → 保存；再验证撤销移动后箭头回到原位。
/// 导出图中第 `y` 行 `[x0, x1)` 范围内的红色标注像素数。默认标注颜色为红色；
/// 只看导出图本身，不受截图前后桌面内容变化影响。
fn changed(_: &ShmImage, b: &ShmImage, x0: u32, x1: u32, y: u32) -> usize {
    (x0..x1).filter(|&x| red(b.rgba(x, y))).count()
}

/// 打开形状菜单并选第 `shape` 项；`tool` 为当前工具，决定工具栏高度和位置。
fn open_shape(input: &Input, dir: &Path, tool: u8, screen: (u32, u32), shape: usize) {
    let ui = native_ui::EditorUi {
        editing: true,
        tool,
        ..Default::default()
    };
    let selection = native_ui::BoxF {
        x: 0.0,
        y: 0.0,
        width: screen.0 as f32,
        height: screen.1 as f32,
    };
    let toolbar = ui.toolbar(screen, selection);
    let chevron = (0..toolbar.width as u32)
        .map(|dx| (f64::from(toolbar.x) + f64::from(dx), f64::from(toolbar.y) + 25.))
        .find(|&p| {
            ui.hit(screen, selection, p) == Some(native_ui::Action::Open(native_ui::Menu::Shapes))
        })
        .expect("形状菜单入口");
    input.click(chevron.0 + 2., chevron.1);
    let open = native_ui::EditorUi {
        editing: true,
        tool,
        menu: Some(native_ui::Menu::Shapes),
        ..Default::default()
    };
    // 菜单子表面出现需要一帧；过早点击会落在菜单之外。
    pause(700);
    let _ = dir;
    let menu = open.menu_box(screen, selection, native_ui::Menu::Shapes);
    input.click(
        f64::from(menu.x) + 75.,
        f64::from(menu.y) + 7. + 36. * shape as f64 + 18.,
    );
}

#[test]
#[ignore = "needs an interactive test Wayland compositor; sends pointer and keyboard events"]
fn arrows_lines_and_moving_marks() {
    let (_temp, dir) = directory("move-marks");
    let output = compositor::select_output(&compositor::outputs().unwrap(), None).unwrap();
    let screen = (output.width, output.height);
    let input = Input::new(screen);
    let export = dir.join("export");
    let (y0, y1, x0, x1) = (300., 520., 300., 700.);

    for undo in [false, true] {
        let before = capture(&output.name);
        let mut editor = Editor::start(&dir, true);
        pause(400);
        let scale = f64::from(before.width()) / f64::from(output.width);
        // 形状菜单第 5 项为箭头，第 4 项为直线。
        open_shape(&input, &dir, 0, screen, 4);
        input.drag((x0, y0), (x1, y0), 40);
        open_shape(&input, &dir, 5, screen, 3);
        input.drag((x0, y0 + 60.), (x1, y0 + 60.), 40);
        input.key(1, "Escape", false);
        // 拖动箭头的线身，把它整体移到 y1。
        input.drag(((x0 + x1) / 2., y0), ((x0 + x1) / 2., y1), 40);
        if undo {
            input.key(44, "z", true);
        }
        pause(150);
        input.key(46, "s", true);
        editor.wait();
        let result = newest_export(&export);
        let row = |y: f64| (y * scale) as u32;
        let span = ((x0 + 30.) * scale) as u32..((x1 - 40.) * scale) as u32;
        let width = span.end - span.start;
        let at = |y: f64| changed(&before, &result, span.start, span.end, row(y));
        println!(
            "undo={undo}: 原位 {} / 新位置 {} / 直线 {} / 共 {width}",
            at(y0),
            at(y1),
            at(y0 + 60.)
        );
        assert!(at(y0 + 60.) as u32 > width * 9 / 10, "直线应保留在原位");
        if undo {
            assert!(at(y0) as u32 > width * 9 / 10, "撤销移动后箭头回到原位");
            assert!((at(y1) as u32) < width / 20, "撤销后新位置不应有箭头");
        } else {
            assert!((at(y0) as u32) < width / 20, "移动后原位不应残留箭头");
            assert!(at(y1) as u32 > width * 9 / 10, "箭头应移动到新位置");
        }
        // 箭头头部宽于线身：在终点附近、偏离中线处也有颜色。
        let tip_y = if undo { y0 } else { y1 };
        let head = changed(
            &before,
            &result,
            ((x1 - 12.) * scale) as u32,
            ((x1 - 4.) * scale) as u32,
            ((tip_y + 4.) * scale) as u32,
        );
        assert!(head > 0, "箭头头部");
    }
    println!("箭头、直线与标注移动通过: {}", dir.display());
}
