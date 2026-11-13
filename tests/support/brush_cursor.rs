/// 画笔范围光标：选画笔后指针变成与笔宽一致的黑白圆环；把笔宽拖到最大后圆环
/// 随之变大。用 `grim -c`（包含指针）截取指针附近，测量白色圆环的半径。
#[test]
#[ignore = "needs an interactive wlroots compositor and grim; sends pointer events"]
fn brush_cursor_matches_tool_width() {
    let (_temp, dir) = directory("brush-cursor");
    let output = compositor::select_output(&compositor::outputs().unwrap(), None).unwrap();
    let screen = (output.width, output.height);
    let scale = f64::from(output.scale);
    let input = Input::new(screen);
    let mut editor = Editor::start(&dir, true);
    pause(400);
    let selection = native_ui::BoxF {
        x: 0.0,
        y: 0.0,
        width: screen.0 as f32,
        height: screen.1 as f32,
    };
    let find = |ui: &native_ui::EditorUi, action: native_ui::Action, dy: f64| {
        let toolbar = ui.toolbar(screen, selection);
        (0..toolbar.width as u32)
            .map(|dx| (f64::from(toolbar.x) + f64::from(dx), f64::from(toolbar.y) + dy))
            .filter(|&p| ui.hit(screen, selection, p) == Some(action))
            .collect::<Vec<_>>()
    };
    let idle = native_ui::EditorUi {
        editing: true,
        ..Default::default()
    };
    let pen = find(&idle, native_ui::Action::Tool(1), 25.);
    input.click(pen[pen.len() / 2].0, pen[0].1);
    pause(300);
    // 测量 (x, y) 处指针圆环：沿水平向右找最远的白色像素。
    let measure = |label: &str| {
        let (x, y) = (600.0_f64, 400.0_f64);
        input.move_to(x, y - 1.);
        pause(60);
        input.move_to(x, y);
        pause(300);
        let path = dir.join(format!("{label}.png"));
        let status = Command::new("grim")
            .args(["-c", "-o", &output.name])
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success());
        let image = ShmImage::read_png(&path).unwrap();
        // 同一时刻不含指针的截图作为背景，差异即光标本身。
        let plain_path = dir.join(format!("{label}-plain.png"));
        assert!(Command::new("grim").args(["-o", &output.name]).arg(&plain_path).status().unwrap().success());
        let plain = ShmImage::read_png(&plain_path).unwrap();
        let (cx, cy) = ((x * scale) as u32, (y * scale) as u32);
        // 沿对角线找最外侧与背景不同的像素（避开中心十字），即圆环外缘。
        let differs = |d: u32| {
            let (a, b) = (image.rgba(cx + d, cy + d), plain.rgba(cx + d, cy + d));
            (0..3).any(|i| a[i].abs_diff(b[i]) > 40)
        };
        let edge = (1..120).filter(|&d| differs(d)).max().expect("没有找到圆环");
        let radius = f64::from(edge) * std::f64::consts::SQRT_2;
        println!("{label}: 圆环外缘半径 {radius:.1} 物理像素");
        radius
    };
    let small = measure("width-4");
    // 4px 笔宽的圆环与中心十字重合，只检查外缘没有超出笔宽加描边。
    assert!(small <= 4.0 * scale / 2.0 + 4.0, "默认 4px 笔宽");
    let ui = native_ui::EditorUi {
        editing: true,
        tool: 1,
        ..Default::default()
    };
    let slider = (0..ui.toolbar(screen, selection).height as u32)
        .flat_map(|dy| find(&ui, native_ui::Action::Width, f64::from(dy)))
        .collect::<Vec<_>>();
    let (left, right) = (slider[0], *slider.last().unwrap());
    println!("粗细滑块 {left:?}..{right:?}");
    let y = (left.1 + right.1) / 2.;
    input.drag((left.0 + 4., y), (right.0 + 40., y), 20);
    pause(200);
    let large = measure("width-64");
    // 外缘 = 作用半径 + 黑色描边的一半（约 1.5 个缓冲区像素）。
    assert!((large - 64.0 * scale / 2.0).abs() <= 4.0, "最大 64px 笔宽");
    input.key(1, "Escape", false);
    input.key(1, "Escape", false);
    editor.wait();
}
