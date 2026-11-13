/// 已有标注的旋转与等比缩放：画矩形 → Esc → 点中矩形出现选中框 → 拖动旋转手柄转 90°
/// → 拖动角点缩小一半 → 保存；导出图里红色像素的外接框应是竖起来、缩小后的矩形。
/// 第二轮在保存前撤销一次，应只撤销缩放、保留旋转。
#[test]
#[ignore = "needs an interactive test Wayland compositor; sends pointer and keyboard events"]
fn rotate_and_scale_existing_marks() {
    let (_temp, dir) = directory("transform-marks");
    let output = compositor::select_output(&compositor::outputs().unwrap(), None).unwrap();
    let screen = (output.width, output.height);
    let input = Input::new(screen);
    let export = dir.join("export");
    for undo in [false, true] {
        let mut editor = Editor::start(&dir, true);
        pause(400);
        open_shape(&input, &dir, 0, screen, 0);
        input.drag((400., 300.), (700., 400.), 30);
        input.key(1, "Escape", false);
        pause(100);
        input.click(401., 350.);
        pause(250);
        screenshot(&output.name, &dir.join(format!("picked-{undo}.png")));
        // 旋转手柄在选中框（外扩 6px）顶边上方 28px：(550, 266)。转到中心右侧即 +90°。
        input.drag((550., 266.), (650., 350.), 30);
        pause(150);
        screenshot(&output.name, &dir.join(format!("rotated-{undo}.png")));
        // 旋转后原左上角点位于 (606, 194)；拖到它与中心连线的中点，缩小一半。
        input.drag((606., 194.), (578., 272.), 30);
        pause(150);
        if undo {
            input.key(44, "z", true);
            pause(150);
        }
        input.key(46, "s", true);
        editor.wait();
        let result = newest_export(&export);
        let scale = f64::from(result.width()) / f64::from(output.width);
        let (mut l, mut t, mut r, mut b) = (u32::MAX, u32::MAX, 0, 0);
        // 只看矩形可能出现的区域，颜色匹配默认的红色 (255, 59, 48)。
        let area = |v: f64| (v * scale) as u32;
        for y in area(120.)..area(580.) {
            for x in area(380.)..area(720.) {
                let p = result.rgba(x, y);
                if p[0] > 235 && p[1].abs_diff(59) < 20 && p[2].abs_diff(48) < 20 {
                    l = l.min(x);
                    t = t.min(y);
                    r = r.max(x);
                    b = b.max(y);
                }
            }
        }
        let got = [l, t, r, b].map(|v| f64::from(v) / scale);
        let want = if undo {
            [500., 200., 600., 500.]
        } else {
            [525., 275., 575., 425.]
        };
        println!("undo={undo}: 红色外接框 {got:?}，期望 {want:?}");
        for (g, w) in got.iter().zip(want) {
            assert!((g - w).abs() <= 4.0, "undo={undo}: {got:?} vs {want:?}");
        }
    }
    println!("旋转/缩放通过: {}", dir.display());
}
