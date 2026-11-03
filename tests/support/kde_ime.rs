#[test]
#[ignore = "needs KDE with fcitx5 Pinyin on a test session; changes the active input method"]
fn kde_pinyin_candidates_stay_visible_and_text_is_exported() {
    if Backend::detect().unwrap() != Backend::Kde {
        return;
    }
    let (_temp, dir) = directory("ime");
    let output = compositor::select_output(&compositor::outputs().unwrap(), None).unwrap();
    let input = Input::new((output.width, output.height));
    let mut editor = Editor::start(&dir, true);
    let (w, h) = (f64::from(output.width), f64::from(output.height));
    input.drag((20., h - 33.), (100., h - 160.), 15);
    let (tx, ty) = (80., h - 185.);
    input.click(tx + 141., ty + 25.);
    let anchor = (w * 0.38, h * 0.3);
    input.click(anchor.0, anchor.1);
    let old = Command::new("fcitx5-remote").arg("-n").output().unwrap();
    let old_name = String::from_utf8_lossy(&old.stdout).trim().to_string();
    assert!(
        Command::new("fcitx5-remote")
            .args(["-s", "pinyin"])
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("fcitx5-remote")
            .arg("-o")
            .status()
            .unwrap()
            .success()
    );
    pause(160);
    let empty = capture(&output.name);
    for code in [49, 23, 35, 30, 24] {
        input.key(code, "", false);
    }
    pause(180);
    let composing = screenshot(&output.name, &dir.join("candidates.png"));
    let sx = f64::from(composing.width()) / w;
    let sy = f64::from(composing.height()) / h;
    // 候选框在光标下方；背景使用差分检验，无需依赖 fcitx 主题颜色。
    let x = (anchor.0 * sx) as u32;
    let y = ((anchor.1 + 31.) * sy) as u32;
    let mut changed = 0;
    for yy in y..(y + (100. * sy) as u32).min(composing.height()) {
        for xx in x..(x + (650. * sx) as u32).min(composing.width()) {
            let a = empty.rgba(xx, yy);
            let b = composing.rgba(xx, yy);
            if (0..3).any(|c| a[c].abs_diff(b[c]) > 30) {
                changed += 1;
            }
        }
    }
    assert!(
        changed > 1000,
        "IME candidates are covered: only {changed} changed pixels below caret"
    );
    input.key(57, "space", false);
    input.key(28, "Return", true);
    pause(100);
    let committed = screenshot(&output.name, &dir.join("committed.png"));
    let area = RectPx::new(
        (anchor.0 * sx) as i32,
        (anchor.1 * sy) as i32,
        (120. * sx) as u32,
        (45. * sy) as u32,
    );
    let (_, count) = distance_mask(&committed, &committed, area);
    assert!(count > 50, "Chinese text was not committed");
    input.key(31, "s", true);
    editor.wait();
    let path = std::fs::read_dir(dir.join("export"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .max()
        .unwrap();
    let exported = ShmImage::read_png(&path).unwrap();
    let (missing, total) = distance_mask(&committed, &exported, area);
    assert!(
        total > 50 && missing * 100 < total,
        "Chinese text differs in export: {missing}/{total}"
    );
    if !old_name.is_empty() {
        let _ = Command::new("fcitx5-remote")
            .args(["-s", &old_name])
            .status();
    }
    println!(
        "IME candidates visible and Chinese PNG verified: {}",
        dir.display()
    );
}
