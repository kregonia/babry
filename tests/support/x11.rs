/// X11 端到端：在 `BABRY_TEST_X11_DISPLAY` 指定的 X 服务器（例如 rootful Xwayland）里，
/// 已打开 `longshot_page()` 的浏览器窗口位于左上角。用 xdotool 发送输入：
/// 框选 → 画矩形 → 长截图滚动 → 停止 → 在长图里滚动再画箭头 → 保存，检查导出。
fn xdo(display: &str, args: &[&str]) {
    let status = Command::new("xdotool")
        .env("DISPLAY", display)
        .args(args)
        .status()
        .unwrap();
    assert!(status.success(), "xdotool {args:?}");
}

#[test]
#[ignore = "needs BABRY_TEST_X11_DISPLAY with a browser showing longshot_page() at the top-left"]
fn x11_editor_longshot_and_annotations() {
    let Ok(display) = std::env::var("BABRY_TEST_X11_DISPLAY") else {
        eprintln!("skip: 未设置 BABRY_TEST_X11_DISPLAY");
        return;
    };
    let (_temp, dir) = directory("x11");
    let x11_env = |command: &mut Command| {
        command
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("NIRI_SOCKET")
            .env("XDG_SESSION_TYPE", "x11")
            .env("DISPLAY", &display);
    };
    let mut command = Command::new(binary());
    x11_env(&mut command);
    command
        .args(["--standalone", "--save-dir"])
        .arg(dir.join("export"))
        .stdout(Stdio::null())
        .stderr(File::create(dir.join("editor.log")).unwrap());
    let mut editor = Editor(command.spawn().unwrap());
    pause(1200);
    let (left, top, right, bottom) = (20., 100., 900., 900.);
    let screen = {
        let out = Command::new(binary())
            .env_remove("WAYLAND_DISPLAY")
            .env("XDG_SESSION_TYPE", "x11")
            .env("DISPLAY", &display)
            .arg("--list-outputs")
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        let size = text.split(": ").nth(1).unwrap().split(' ').next().unwrap().to_owned();
        let (w, h) = size.split_once('x').unwrap();
        (w.parse::<u32>().unwrap(), h.parse::<u32>().unwrap())
    };
    let s = |v: f64| format!("{}", v.round() as i32);
    xdo(&display, &["mousemove", &s(left), &s(top), "mousedown", "1"]);
    for i in 1..=10 {
        let t = f64::from(i) / 10.;
        xdo(&display, &["mousemove", &s(left + (right - left) * t), &s(top + (bottom - top) * t)]);
    }
    xdo(&display, &["mouseup", "1"]);
    pause(300);
    let selection = native_ui::BoxF {
        x: left as f32,
        y: top as f32,
        width: (right - left) as f32,
        height: (bottom - top) as f32,
    };
    let find = |ui: &native_ui::EditorUi, action: native_ui::Action| {
        let toolbar = ui.toolbar(screen, selection);
        (0..toolbar.width as u32)
            .map(|dx| (f64::from(toolbar.x) + f64::from(dx), f64::from(toolbar.y) + 25.))
            .find(|&p| ui.hit(screen, selection, p) == Some(action))
            .map(|p| (p.0 + 10., p.1))
            .unwrap()
    };
    let idle = native_ui::EditorUi {
        editing: true,
        ..Default::default()
    };
    let click = |p: (f64, f64)| {
        xdo(&display, &["mousemove", &s(p.0), &s(p.1)]);
        pause(60);
        xdo(&display, &["click", "1"]);
        pause(200);
    };
    let drag = |a: (f64, f64), b: (f64, f64)| {
        xdo(&display, &["mousemove", &s(a.0), &s(a.1), "mousedown", "1"]);
        for i in 1..=8 {
            let t = f64::from(i) / 8.;
            xdo(&display, &["mousemove", &s(a.0 + (b.0 - a.0) * t), &s(a.1 + (b.1 - a.1) * t)]);
        }
        xdo(&display, &["mouseup", "1"]);
        pause(150);
    };
    // 1) 长截图：点按钮，滚动，再点同一按钮停止。
    let longshot = find(&idle, native_ui::Action::Longshot);
    click(longshot);
    pause(600);
    xdo(&display, &["mousemove", &s((left + right) / 2.), &s((top + bottom) / 2.)]);
    for _ in 0..20 {
        xdo(&display, &["click", "5"]);
        pause(150);
    }
    pause(600);
    click(longshot);
    pause(500);
    // 2) 长图里滚动一段，再画一个矩形。
    xdo(&display, &["mousemove", &s((left + right) / 2.), &s((top + bottom) / 2.)]);
    for _ in 0..8 {
        xdo(&display, &["click", "5"]);
        pause(30);
    }
    click(find(&idle, native_ui::Action::Tool(2)));
    drag((left + 100., top + 150.), (left + 300., top + 270.));
    // 画笔范围光标：切到画笔后在选区内移动，编辑器不能因创建位图光标出错。
    click(find(&idle, native_ui::Action::Tool(1)));
    xdo(&display, &["mousemove", &s(left + 400.), &s(top + 400.)]);
    pause(200);
    xdo(&display, &["mousemove", &s(left + 420.), &s(top + 420.)]);
    pause(200);
    xdo(&display, &["key", "ctrl+s"]);
    editor.wait();
    let result = newest_export(&dir.join("export"));
    println!("X11 长截图 {}x{}", result.width(), result.height());
    assert!(f64::from(result.height()) > (bottom - top) * 2.5, "拼接高度不足");
    let x = result.width() * 3 / 4;
    let mut runs: Vec<(u32, u32)> = Vec::new();
    for y in 0..result.height() {
        let Some(index) = decode_row(result.rgba(x, y)) else { continue };
        match runs.last_mut() {
            Some((last, count)) if *last == index => *count += 1,
            _ => runs.push((index, 1)),
        }
    }
    for pair in runs.windows(2) {
        assert_eq!(pair[1].0, pair[0].0 + 1, "序号不连续：{pair:?}");
    }
    let span = 60u32..(result.width() as f64 * 0.4) as u32;
    let red_rows: Vec<u32> = (0..result.height())
        .filter(|&y| span.clone().step_by(3).any(|x| red(result.rgba(x, y))))
        .collect();
    let (first, last) = (red_rows[0], *red_rows.last().unwrap());
    println!("色块 {}..{}，矩形 {first}..{last}", runs[0].0, runs.last().unwrap().0);
    assert!(first > 150 + 100, "矩形应在滚动后的位置");
    assert!((f64::from(last - first) - 120.).abs() < 8., "矩形高度");
    println!("X11 编辑器通过: {}", dir.display());
}

/// X11 剪贴板服务：`--x11-clipboard` 取得 CLIPBOARD 后，其他客户端能读到完全相同的
/// PNG 字节；被别的程序接管后进程退出。
#[test]
#[ignore = "needs BABRY_TEST_X11_DISPLAY"]
fn x11_clipboard_serves_png() {
    use x11rb::{
        connection::Connection,
        protocol::{
            Event,
            xproto::{AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, Property, WindowClass},
        },
    };
    let Ok(display) = std::env::var("BABRY_TEST_X11_DISPLAY") else {
        eprintln!("skip: 未设置 BABRY_TEST_X11_DISPLAY");
        return;
    };
    let (_temp, dir) = directory("x11-clipboard");
    let path = dir.join("image.png");
    let data: Vec<u8> = (0..20_000_000u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect();
    std::fs::write(&path, &data).unwrap();
    let mut server = Command::new(binary())
        .env_remove("WAYLAND_DISPLAY")
        .env("XDG_SESSION_TYPE", "x11")
        .env("DISPLAY", &display)
        .arg("--x11-clipboard")
        .arg(&path)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    std::io::BufRead::read_line(
        &mut std::io::BufReader::new(server.stdout.take().unwrap()),
        &mut line,
    )
    .unwrap();
    assert_eq!(line.trim(), "ready");
    let (conn, screen) = x11rb::connect(Some(&display)).unwrap();
    let root = conn.setup().roots[screen].root;
    let atom = |name: &str| {
        conn.intern_atom(false, name.as_bytes())
            .unwrap()
            .reply()
            .unwrap()
            .atom
    };
    let (clipboard, png, prop, incr) = (
        atom("CLIPBOARD"),
        atom("image/png"),
        atom("BABRY_TEST"),
        atom("INCR"),
    );
    let window = conn.generate_id().unwrap();
    conn.create_window(
        0,
        window,
        root,
        0,
        0,
        1,
        1,
        0,
        WindowClass::INPUT_ONLY,
        0,
        &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
    )
    .unwrap();
    conn.convert_selection(window, clipboard, png, prop, x11rb::CURRENT_TIME)
        .unwrap();
    conn.flush().unwrap();
    loop {
        if let Event::SelectionNotify(e) = conn.wait_for_event().unwrap() {
            assert_ne!(e.property, x11rb::NONE, "剪贴板拒绝了 image/png");
            break;
        }
    }
    let get = || {
        conn.get_property(true, window, prop, AtomEnum::ANY, 0, u32::MAX)
            .unwrap()
            .reply()
            .unwrap()
    };
    let first = get();
    conn.flush().unwrap();
    let mut received = Vec::new();
    if first.type_ == incr {
        // 分块：每次删除属性后等待下一块，空块表示结束。
        loop {
            if let Event::PropertyNotify(e) = conn.wait_for_event().unwrap()
                && e.atom == prop
                && e.state == Property::NEW_VALUE
            {
                let part = get();
                conn.flush().unwrap();
                if part.value.is_empty() {
                    break;
                }
                received.extend(part.value);
            }
        }
    } else {
        received = first.value;
    }
    assert!(received == data, "剪贴板内容不一致：{} 字节", received.len());
    conn.set_selection_owner(window, clipboard, x11rb::CURRENT_TIME)
        .unwrap();
    conn.flush().unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while server.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "被接管后剪贴板进程应退出");
        pause(50);
    }
    println!("X11 剪贴板通过 ({} 字节, INCR={})", data.len(), first.type_ == incr);
}
