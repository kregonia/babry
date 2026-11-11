/// 浏览器长页面：顶部是吸顶标题栏，下面每一行右侧是按序号编码颜色的色块。
/// 序号 i 编码为 (20 + i%12·20, 20 + (i/12)%12·20, 20 + (i/144)·20)，按列解码即可
/// 检查拼接结果是否连续、没有重复或缺行。
const LONGSHOT_ROWS: u32 = 260;
const LONGSHOT_ROW_CSS: f64 = 44.0;
const LONGSHOT_HEADER: [u8; 3] = [5, 250, 5];

fn longshot_page() -> String {
    let mut rows = String::new();
    for i in 0..LONGSHOT_ROWS {
        let (r, g, b) = (20 + i % 12 * 20, 20 + i / 12 % 12 * 20, 20 + i / 144 * 20);
        rows.push_str(&format!(
            "<div class=r style=\"background:rgb({r},{g},{b})\"><span>第 {i} 行 Babry 长截图 {}</span></div>\n",
            "测试文本 ".repeat((i % 5 + 1) as usize)
        ));
    }
    format!(
        "<!doctype html><meta charset=utf-8><style>\
         html,body{{margin:0;font:15px sans-serif;background:#fff}}\
         header{{position:sticky;top:0;height:56px;background:rgb(5,250,5);font:20px sans-serif;padding:14px}}\
         .r{{height:{LONGSHOT_ROW_CSS}px;display:flex;align-items:center}}\
         .r span{{width:45%;height:100%;background:#fff;display:flex;align-items:center;padding-left:10px}}\
         </style><header>Babry 长截图吸顶标题</header>\n{rows}"
    )
}

fn decode_row(p: [u8; 4]) -> Option<u32> {
    let digit = |c: u8, max: u32| {
        let value = (f64::from(c) - 20.0) / 20.0;
        let rounded = value.round();
        ((value - rounded).abs() * 20.0 <= 6.0 && rounded >= 0.0 && rounded < f64::from(max))
            .then_some(rounded as u32)
    };
    Some(digit(p[0], 12)? + digit(p[1], 12)? * 12 + digit(p[2], 3)? * 144)
}

#[test]
#[ignore = "needs a live Wayland session with google-chrome; scrolls a browser window"]
fn longshot_scrolls_and_stitches_browser_page() {
    let backend = Backend::detect().unwrap();
    let chrome = ["google-chrome-stable", "google-chrome", "chromium"]
        .into_iter()
        .find(|name| Command::new("which").arg(name).output().is_ok_and(|o| o.status.success()));
    let Some(chrome) = chrome else {
        eprintln!("skip: 没有 Chrome/Chromium");
        return;
    };
    let (_temp, dir) = directory("longshot");
    let page = dir.join("page.html");
    std::fs::write(&page, longshot_page()).unwrap();
    let mut browser = Command::new(chrome)
        .arg(format!("--user-data-dir={}", dir.join("chrome").display()))
        .args([
            "--ozone-platform=wayland",
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-extensions",
        ])
        .arg(format!("--app=file://{}", page.display()))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    struct Kill(Child);
    impl Drop for Kill {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    pause(4000);
    assert!(browser.try_wait().unwrap().is_none(), "浏览器启动失败");
    let browser = Kill(browser);

    let output = compositor::select_output(&compositor::outputs().unwrap(), None).unwrap();
    let screen = (output.width, output.height);
    let before = capture(&output.name);
    let scale = f64::from(before.width()) / f64::from(output.width);
    // niri 给出焦点窗口；其他合成器取屏幕中央一块区域，由测试者把浏览器放在那里。
    let window = (backend == Backend::Niri)
        .then(|| compositor::window_regions(&output, (before.width(), before.height())))
        .and_then(|seeds| seeds.first().copied())
        .map(|r| {
            (
                f64::from(r.x) / scale,
                f64::from(r.y) / scale,
                f64::from(r.right()) / scale,
                f64::from(r.bottom()) / scale,
            )
        })
        .unwrap_or((
            f64::from(output.width) * 0.25,
            f64::from(output.height) * 0.1,
            f64::from(output.width) * 0.75,
            f64::from(output.height) * 0.85,
        ));
    // 避开浏览器标题栏和窗口边框，但把吸顶标题栏留在选区里。
    let (left, top, right, bottom) = (
        (window.0 + 30.).round(),
        (window.1 + 50.).round(),
        (window.2 - 40.).round(),
        (window.3 - 60.).round(),
    );
    println!("浏览器窗口 {window:?}，选区 ({left}, {top})-({right}, {bottom})");

    let input = Input::new(screen);
    let mut editor = Editor::start(&dir, false);
    input.drag((left, top), (right, bottom), 50);
    pause(200);
    let selection = native_ui::BoxF {
        x: left as f32,
        y: top as f32,
        width: (right - left) as f32,
        height: (bottom - top) as f32,
    };
    let ui = native_ui::EditorUi {
        editing: true,
        ..Default::default()
    };
    let toolbar = ui.toolbar(screen, selection);
    let button = (0..toolbar.width as u32)
        .map(|dx| (f64::from(toolbar.x) + f64::from(dx), f64::from(toolbar.y) + 25.))
        .find(|&p| ui.hit(screen, selection, p) == Some(native_ui::Action::Longshot))
        .expect("工具栏里应有长截图按钮");
    input.click(button.0 + 10., button.1);
    pause(900);
    let overlay = screenshot(&output.name, &dir.join("overlay.png"));
    let region = RectPx::new(
        left as i32,
        top as i32,
        (right - left) as u32,
        (bottom - top) as u32,
    );
    // 滚轮逐格向下，每格之间给浏览器平滑滚动和取帧留时间。工具栏全程可见。
    input.move_to((left + right) / 2., (top + bottom) / 2.);
    for _ in 0..28 {
        input.scroll(1);
        pause(160);
    }
    pause(800);
    screenshot(&output.name, &dir.join("scrolled.png"));
    // 再次点击同一个按钮停止，拼接结果进入长图画布，工具栏仍在。
    input.click(button.0 + 10., button.1);
    pause(800);
    let canvas = screenshot(&output.name, &dir.join("canvas.png"));
    // 长图编辑页：原位置原大小的窗口里滚动页面，再画一个矩形。
    input.move_to((left + right) / 2., (top + bottom) / 2.);
    for _ in 0..10 {
        input.scroll(1);
        pause(40);
    }
    pause(300);
    let paged = screenshot(&output.name, &dir.join("canvas-scrolled.png"));
    let probe = RectPx::new(
        ((left + 40.) * scale) as i32,
        ((top + 40.) * scale) as i32,
        ((right - left - 80.) * scale) as u32,
        ((bottom - top - 80.) * scale) as u32,
    );
    let moved = (probe.y..probe.bottom())
        .step_by(7)
        .filter(|&y| {
            let x = (probe.x + probe.width as i32 * 3 / 4) as u32;
            canvas.rgba(x, y as u32) != paged.rgba(x, y as u32)
        })
        .count();
    assert!(moved > 20, "滚轮应在窗口内滚动长图：{moved}");
    // 窗口外仍是原截图的遮罩，不随页面滚动。
    let outside = (5u32, (top * scale) as u32 + 50);
    assert_eq!(canvas.rgba(outside.0, outside.1), paged.rgba(outside.0, outside.1));
    let rect_tool = (0..toolbar.width as u32)
        .map(|dx| (f64::from(toolbar.x) + f64::from(dx), f64::from(toolbar.y) + 25.))
        .find(|&p| ui.hit(screen, selection, p) == Some(native_ui::Action::Tool(2)))
        .expect("矩形工具");
    input.click(rect_tool.0 + 10., rect_tool.1);
    let mark_y = (top + 150.).round();
    input.drag((left + 100., mark_y), (left + 300., mark_y + 120.), 30);
    input.key(46, "s", true);
    editor.wait();
    drop(browser);

    let result = newest_export(&dir.join("export"));
    println!(
        "长截图 {}x{}，选区物理高度 {}",
        result.width(),
        result.height(),
        (f64::from(region.height) * scale) as u32
    );
    assert!(overlay.width() > 0);
    assert!(
        f64::from(result.height()) > f64::from(region.height) * scale * 2.5,
        "拼接高度不足：{}",
        result.height()
    );
    // 解码右侧色块列：序号必须逐一递增，每行高度一致，标题栏只出现在顶部。
    let x = result.width() * 3 / 4;
    let row_px = LONGSHOT_ROW_CSS * scale;
    let mut runs: Vec<(u32, u32)> = Vec::new();
    let mut header_rows = Vec::new();
    for y in 0..result.height() {
        let p = result.rgba(x, y);
        if p[..3]
            .iter()
            .zip(LONGSHOT_HEADER)
            .all(|(a, b)| a.abs_diff(b) <= 12)
        {
            header_rows.push(y);
        }
        let Some(index) = decode_row(p) else { continue };
        match runs.last_mut() {
            Some((last, count)) if *last == index => *count += 1,
            _ => runs.push((index, 1)),
        }
    }
    println!(
        "色块序号 {}..{}，共 {} 段；标题栏行 {:?}",
        runs.first().unwrap().0,
        runs.last().unwrap().0,
        runs.len(),
        header_rows.first().zip(header_rows.last())
    );
    for pair in runs.windows(2) {
        assert_eq!(pair[1].0, pair[0].0 + 1, "序号不连续：{pair:?}");
    }
    for &(index, count) in &runs[1..runs.len() - 1] {
        assert!(
            (f64::from(count) - row_px).abs() <= 3.0,
            "第 {index} 行高度 {count}，应约为 {row_px:.1}"
        );
    }
    assert!(runs.len() > 20, "拼接出的行太少：{}", runs.len());
    let header_end = header_rows.last().copied().unwrap_or(0);
    assert!(
        f64::from(header_end) < 80.0 * scale,
        "吸顶标题栏不应出现在拼接结果中部：最后一行 {header_end}"
    );
    // 矩形画在滚动后的位置：导出图里它的上边应在第一屏以下。只看页面左侧的
    // 白底文字区（色块在 45% 宽度之后），避免把红色色块当成矩形。
    let span = (60. * scale) as u32..(result.width() as f64 * 0.4) as u32;
    let red_rows: Vec<u32> = (0..result.height())
        .filter(|&y| span.clone().step_by(3).any(|x| red(result.rgba(x, y))))
        .collect();
    let first = *red_rows.first().expect("导出图应有矩形");
    let last = *red_rows.last().unwrap();
    println!("矩形位于第 {first}..{last} 行");
    // 矩形逻辑高度 120，物理约 150；上边位置 = 窗口内 150 逻辑像素 + 滚动量。
    assert!((f64::from(last - first) - 120. * scale).abs() < 12., "矩形高度");
    assert!(
        f64::from(first) > (150. + 80.) * scale,
        "矩形应位于滚动后的页面位置：{first}"
    );
    println!("长截图通过: {}", dir.display());
}
