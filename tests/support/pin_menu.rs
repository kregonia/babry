/// 贴图右键菜单：右键不再直接关闭；“复制图片”写入剪贴板，“关闭贴图”只关闭窗口、
/// 不删除图片文件。
#[test]
#[ignore = "needs an interactive test Wayland compositor; sends pointer events and uses the clipboard"]
fn pin_context_menu_copies_and_closes() {
    let (_temp, dir) = directory("pin-menu");
    let output = compositor::select_output(&compositor::outputs().unwrap(), None).unwrap();
    let screen = (output.width, output.height);
    // 一张 400×300 的纯色图，贴图按物理像素 1:1 显示并居中。
    let path = dir.join("pin.png");
    {
        let file = File::create(&path).unwrap();
        let mut encoder = png::Encoder::new(file, 400, 300);
        encoder.set_color(png::ColorType::Rgb);
        let mut writer = encoder.write_header().unwrap();
        writer
            .write_image_data(&[40u8, 120, 220].repeat(400 * 300))
            .unwrap();
    }
    let scale = f64::from(output.scale);
    let (w, h) = (400. / scale, 300. / scale);
    let center = (f64::from(screen.0) / 2., f64::from(screen.1) / 2.);
    // 用假的 zenity 代替保存对话框：直接回答目标路径，验证“另存为”真的复制了文件。
    let fake = dir.join("bin");
    std::fs::create_dir_all(&fake).unwrap();
    let saved = dir.join("另存为 结果");
    std::fs::write(
        fake.join("zenity"),
        format!("#!/bin/sh\nprintf '%s\\n' '{}'\n", saved.display()),
    )
    .unwrap();
    Command::new("chmod").arg("+x").arg(fake.join("zenity")).status().unwrap();
    let search = format!("{}:{}", fake.display(), std::env::var("PATH").unwrap_or_default());
    let start = || {
        let child = Command::new(binary())
            .env("PATH", &search)
            .arg("--pin")
            .arg(&path)
            .stdout(Stdio::null())
            .stderr(File::create(dir.join("pin.log")).unwrap())
            .spawn()
            .unwrap();
        pause(700);
        Editor(child)
    };
    let (menu_w, _) = native_ui::context_menu_size(3);
    let item = |at: (f64, f64), index: usize| {
        let b = native_ui::context_item_box(index);
        (
            at.0 + f64::from(b.x + b.width / 2.),
            at.1 + f64::from(b.y + b.height / 2.),
        )
    };
    let input = Input::new(screen);

    // 1) 右键打开菜单后贴图仍在，选“复制图片”。
    let mut pin = start();
    let at = (center.0 - w / 4., center.1 - h / 4.);
    input.move_to(at.0, at.1);
    pause(100);
    input.button(0x111, true);
    input.button(0x111, false);
    pause(400);
    screenshot(&output.name, &dir.join("menu.png"));
    assert!(pin.0.try_wait().unwrap().is_none(), "右键不应关闭贴图");
    let _ = Command::new("wl-copy").arg("--clear").status();
    let copy = item(at, 0);
    input.move_to(at.0 + f64::from(menu_w) / 2., at.1 + 10.);
    pause(60);
    input.click(copy.0, copy.1);
    pause(800);
    let types = Command::new("wl-paste").arg("--list-types").output().unwrap();
    let types = String::from_utf8_lossy(&types.stdout);
    assert!(types.contains("image/png"), "剪贴板应为 PNG: {types}");
    assert!(pin.0.try_wait().unwrap().is_none(), "复制后贴图仍应保留");

    // 2) “另存为…”：对话框给出无扩展名的路径，保存为同内容的 .png。
    input.move_to(at.0, at.1);
    pause(100);
    input.button(0x111, true);
    input.button(0x111, false);
    pause(400);
    let save = item(at, 1);
    input.move_to(at.0 + 20., at.1 + 10.);
    pause(60);
    input.click(save.0, save.1);
    pause(600);
    let target = saved.with_extension("png");
    assert_eq!(
        std::fs::read(&target).ok(),
        Some(std::fs::read(&path).unwrap()),
        "另存为应复制图片到 {}",
        target.display()
    );

    // 3) 再次右键选“关闭贴图”：进程退出，图片文件保留。
    input.move_to(at.0, at.1);
    pause(100);
    input.button(0x111, true);
    input.button(0x111, false);
    pause(400);
    let close = item(at, 2);
    input.move_to(at.0 + 20., at.1 + 10.);
    pause(60);
    input.click(close.0, close.1);
    pin.wait();
    assert!(path.exists(), "关闭贴图不应删除图片");
    println!("贴图右键菜单通过: {}", dir.display());
}
