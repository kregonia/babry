//! 实际合成器回归测试。需要已运行的、供测试使用的 KDE 或 niri Wayland 会话。
//! 会发送鼠标和按键事件，因此默认忽略；请串行运行：
//! cargo test --test wayland_editor -- --ignored --test-threads=1 --nocapture
//! BABRY_TEST_ARTIFACTS 指定截图和日志目录。KWin 可用
//! KWIN_DISABLE_UDMABUF_IMPORT=0 强制覆盖共享内存 DMA-BUF 导入路径。

include!("support/workflow.rs");
include!("support/kde_ime.rs");
include!("support/daemon.rs");
include!("support/longshot.rs");
include!("support/annotate.rs");
include!("support/pin_menu.rs");
use babry::{
    compositor::{self, Backend},
    core::RectPx,
    export,
    font::Fonts,
    kde, native_ui, screencopy,
    shm_image::ShmImage,
    smart_select,
};
use smithay_client_toolkit::reexports::{
    client::{
        Connection, Dispatch, QueueHandle, delegate_noop,
        globals::{GlobalListContents, registry_queue_init},
        protocol::{wl_pointer, wl_registry, wl_seat},
    },
    protocols_wlr::virtual_pointer::v1::client::{
        zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
        zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
    },
};
use std::{
    fs::File,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};
use wayland_protocols_plasma::fake_input::client::org_kde_kwin_fake_input::OrgKdeKwinFakeInput;

struct InputState;
impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for InputState {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
delegate_noop!(InputState: ignore OrgKdeKwinFakeInput);
delegate_noop!(InputState: ignore ZwlrVirtualPointerManagerV1);
delegate_noop!(InputState: ignore ZwlrVirtualPointerV1);
delegate_noop!(InputState: ignore wl_seat::WlSeat);

enum Device {
    Kde(OrgKdeKwinFakeInput),
    Wlr(ZwlrVirtualPointerV1),
}
struct Input {
    connection: Connection,
    device: Device,
    size: (u32, u32),
}
impl Input {
    fn new(size: (u32, u32)) -> Self {
        let connection = Connection::connect_to_env().unwrap();
        let (globals, mut queue) = registry_queue_init::<InputState>(&connection).unwrap();
        let qh = queue.handle();
        let device = if Backend::detect().unwrap() == Backend::Kde {
            let input: OrgKdeKwinFakeInput = globals.bind(&qh, 4..=5, ()).unwrap();
            input.authenticate(
                "Babry regression test".into(),
                "Editor interaction test".into(),
            );
            Device::Kde(input)
        } else {
            let manager: ZwlrVirtualPointerManagerV1 = globals.bind(&qh, 1..=2, ()).unwrap();
            let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=1, ()).unwrap();
            Device::Wlr(manager.create_virtual_pointer(Some(&seat), &qh, ()))
        };
        queue.roundtrip(&mut InputState).unwrap();
        Self {
            connection,
            device,
            size,
        }
    }
    fn flush(&self) {
        self.connection.flush().unwrap();
    }
    fn move_to(&self, x: f64, y: f64) {
        match &self.device {
            Device::Kde(input) => input.pointer_motion_absolute(x, y),
            Device::Wlr(input) => {
                input.motion_absolute(0, x as u32, y as u32, self.size.0, self.size.1);
                input.frame();
            }
        }
        self.flush();
    }
    fn button(&self, button: u32, pressed: bool) {
        match &self.device {
            Device::Kde(input) => input.button(button, u32::from(pressed)),
            Device::Wlr(input) => {
                input.button(
                    0,
                    button,
                    if pressed {
                        wl_pointer::ButtonState::Pressed
                    } else {
                        wl_pointer::ButtonState::Released
                    },
                );
                input.frame();
            }
        }
        self.flush();
    }
    fn click(&self, x: f64, y: f64) {
        self.move_to(x, y);
        pause(25);
        self.button(0x110, true);
        pause(20);
        self.button(0x110, false);
        pause(90);
    }
    fn sweep(&self, from: (f64, f64), to: (f64, f64), steps: u32) {
        for i in 1..=steps {
            let t = f64::from(i) / f64::from(steps);
            self.move_to(from.0 + (to.0 - from.0) * t, from.1 + (to.1 - from.1) * t);
            pause(3);
        }
    }
    fn drag(&self, from: (f64, f64), to: (f64, f64), steps: u32) {
        self.move_to(from.0, from.1);
        pause(25);
        self.button(0x110, true);
        self.sweep(from, to, steps);
        self.button(0x110, false);
        pause(100);
    }
    /// 滚轮向下 `notches` 格（负数向上）。
    fn scroll(&self, notches: i32) {
        match &self.device {
            Device::Kde(input) => input.axis(0, 15.0 * f64::from(notches)),
            Device::Wlr(input) => {
                input.axis_source(wl_pointer::AxisSource::Wheel);
                input.axis_discrete(
                    0,
                    wl_pointer::Axis::VerticalScroll,
                    15.0 * f64::from(notches),
                    notches,
                );
                input.frame();
            }
        }
        self.flush();
    }
    fn key(&self, code: u32, name: &str, ctrl: bool) {
        match &self.device {
            Device::Kde(input) => {
                if ctrl {
                    input.keyboard_key(29, 1);
                }
                input.keyboard_key(code, 1);
                self.flush();
                pause(20);
                input.keyboard_key(code, 0);
                if ctrl {
                    input.keyboard_key(29, 0);
                }
                self.flush();
            }
            Device::Wlr(_) => {
                let mut command = Command::new("wtype");
                if ctrl {
                    command.args(["-P", "Control_L"]);
                }
                command.args(["-k", name]);
                if ctrl {
                    command.args(["-p", "Control_L"]);
                }
                assert!(command.status().unwrap().success());
            }
        }
        pause(100);
    }
}

fn pause(ms: u64) {
    thread::sleep(Duration::from_millis(ms));
}
fn binary() -> std::ffi::OsString {
    std::env::var_os("BABRY_TEST_BINARY").unwrap_or_else(|| env!("CARGO_BIN_EXE_babry").into())
}
struct Editor(Child);
impl Editor {
    fn start(directory: &Path, screen: bool) -> Self {
        let log = File::create(directory.join("editor.log")).unwrap();
        let mut command = Command::new(binary());
        command
            .arg("--standalone")
            .arg("--save-dir")
            .arg(directory.join("export"));
        if screen {
            command.arg("--screen");
        }
        command.stdout(Stdio::null()).stderr(log);
        if std::env::var_os("BABRY_TEST_WAYLAND_LOG").is_some() {
            command.env("WAYLAND_DEBUG", "1");
        }
        let mut editor = Self(command.spawn().unwrap());
        pause(500);
        assert!(
            editor.0.try_wait().unwrap().is_none(),
            "editor exited before input"
        );
        editor
    }
    fn wait(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = self.0.try_wait().unwrap() {
                assert!(status.success(), "editor failed: {status}");
                // 客户端退出与合成器呈现移除后的画面是异步的；下一次截图不能截到
                // 上一张编辑器或输入法的最后一帧。
                pause(120);
                return;
            }
            assert!(Instant::now() < deadline, "editor did not exit");
            pause(30);
        }
    }
}
impl Drop for Editor {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn directory(name: &str) -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let root = std::env::var_os("BABRY_TEST_ARTIFACTS")
        .map(PathBuf::from)
        .unwrap_or_else(|| temp.path().to_path_buf());
    let path = root.join(name);
    std::fs::create_dir_all(&path).unwrap();
    (temp, path)
}
fn capture(output: &str) -> ShmImage {
    if Backend::detect().unwrap() == Backend::Kde {
        kde::capture(output).unwrap()
    } else {
        screencopy::capture(Some(output)).unwrap()
    }
}
fn screenshot(output: &str, path: &Path) -> ShmImage {
    let image = capture(output);
    export::save_png(
        path,
        &image,
        RectPx::new(0, 0, image.width(), image.height()),
        &[],
        &mut Fonts::new(),
    )
    .unwrap();
    image
}
fn red(p: [u8; 4]) -> bool {
    p[0] > 180 && p[1] < 110 && p[2] < 105
}
fn distance_mask(a: &ShmImage, b: &ShmImage, area: RectPx) -> (usize, usize) {
    let (mut missing, mut count) = (0, 0);
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            if !red(a.rgba(x as u32, y as u32)) {
                continue;
            }
            count += 1;
            let covered = (-2..=2).any(|dy| {
                (-2..=2).any(|dx| {
                    let (x, y) = (x + dx, y + dy);
                    x >= 0 && y >= 0 && red(b.rgba(x as u32, y as u32))
                })
            });
            missing += usize::from(!covered);
        }
    }
    (missing, count)
}

#[test]
#[ignore = "needs an interactive test Wayland compositor; sends pointer and keyboard events"]
fn preview_survives_buffer_retirement_and_rectangle_remapping() {
    let (_temp, dir) = directory("preview");
    let output = compositor::select_output(&compositor::outputs().unwrap(), None).unwrap();
    let (w, h) = (f64::from(output.width), f64::from(output.height));
    let input = Input::new((output.width, output.height));
    let mut editor = Editor::start(&dir, true);
    // 固定工具栏位置，后续切换工具不随参数行高度改变按钮坐标。
    let memory = |pid: u32| {
        std::fs::read_to_string(format!("/proc/{pid}/status"))
            .unwrap_or_default()
            .lines()
            .filter(|line| line.starts_with("VmRSS:") || line.starts_with("VmHWM:"))
            .collect::<Vec<_>>()
            .join("; ")
    };
    println!("before marks: {}", memory(editor.0.id()));
    input.drag((20., h - 33.), (100., h - 160.), 15);
    let (tx, ty) = (80., h - 185.);
    input.click(tx + 105., ty + 25.);
    for cycle in 0..3 {
        for y in [h * 0.17, h * 0.32, h * 0.47] {
            input.drag((40., y), (w - 45., y), 160);
        }
        let image = screenshot(&output.name, &dir.join(format!("pen-{cycle}.png")));
        for y in [h * 0.17, h * 0.32, h * 0.47] {
            let py = (y * f64::from(image.height()) / h).round() as i32;
            let (from, to) = (60, (image.width() - 60) as i32);
            let mut missing = 0;
            for x in from..to {
                if !(-4..=4).any(|dy| red(image.rgba(x as u32, (py + dy) as u32))) {
                    missing += 1;
                }
            }
            assert!(
                missing < 12,
                "pen cycle {cycle}: {missing} missing pixels at row {py}"
            );
        }
        for _ in 0..3 {
            input.key(44, "z", true);
        }
        pause(160);
        println!("pen cycle {cycle} after undo: {}", memory(editor.0.id()));
    }
    input.click(tx + 53., ty + 25.);
    let anchor = (50., 65.);
    input.move_to(anchor.0, anchor.1);
    pause(25);
    input.button(0x110, true);
    let large = (w - 65., h - 225.);
    let small = (w * 0.33, h * 0.34);
    let mut at = anchor;
    for cycle in 0..8 {
        for end in [large, small] {
            input.sweep(at, end, 70);
            at = end;
            pause(90);
            let image = screenshot(
                &output.name,
                &dir.join(format!(
                    "rect-{cycle}-{}.png",
                    if end == large { "large" } else { "small" }
                )),
            );
            if end == small {
                // 缩回后旧的右边/下边不能残留，排除当前矩形附近的 12 像素。
                let left = ((small.0 + 15.) / w * f64::from(image.width())) as u32;
                let right = ((large.0 + 5.) / w * f64::from(image.width())) as u32;
                let top = (85. / h * f64::from(image.height())) as u32;
                let bottom = ((large.1 + 10.) / h * f64::from(image.height())) as u32;
                let mut ghosts = 0;
                for y in top..bottom {
                    for x in left..right {
                        ghosts += usize::from(red(image.rgba(x, y)));
                    }
                }
                assert!(
                    ghosts < 24,
                    "rectangle remap {cycle}: {ghosts} stale red pixels"
                );
            }
        }
    }
    input.sweep(at, large, 80);
    input.button(0x110, false);
    pause(180);
    let preview = screenshot(&output.name, &dir.join("final-preview.png"));
    println!("after rectangle remapping: {}", memory(editor.0.id()));
    input.key(31, "s", true);
    editor.wait();
    let png = std::fs::read_dir(dir.join("export"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .max()
        .unwrap();
    let exported = ShmImage::read_png(&png).unwrap();
    assert_eq!(
        (preview.width(), preview.height()),
        (exported.width(), exported.height())
    );
    let area = RectPx::new(
        15,
        15,
        preview.width() - 30,
        (preview.height() as f64 * 0.69) as u32,
    );
    for (a, b, label) in [
        (&exported, &preview, "missing"),
        (&preview, &exported, "stale"),
    ] {
        let (bad, count) = distance_mask(a, b, area);
        assert!(count > 1000, "too few annotation pixels");
        assert!(
            bad * 500 < count,
            "{label}: {bad}/{count} annotation pixels disagree with PNG"
        );
    }
    println!(
        "preview regression passed on {:?}: {}x{} logical {}x{}, artifacts {}",
        Backend::detect().unwrap(),
        preview.width(),
        preview.height(),
        output.width,
        output.height,
        dir.display()
    );
}

fn contains_rect(outer: RectPx, inner: RectPx) -> bool {
    outer.x <= inner.x
        && outer.y <= inner.y
        && outer.right() >= inner.right()
        && outer.bottom() >= inner.bottom()
}

fn newest_export(directory: &Path) -> ShmImage {
    let newest = std::fs::read_dir(directory)
        .unwrap()
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            (path.extension()? == "png").then_some(path)
        })
        .max()
        .expect("保存目录里应有导出的 PNG");
    ShmImage::read_png(&newest).unwrap()
}

fn assert_size(image: &ShmImage, rect: RectPx, label: &str) {
    let (dw, dh) = (
        image.width().abs_diff(rect.width),
        image.height().abs_diff(rect.height),
    );
    assert!(
        dw <= 3 && dh <= 3,
        "{label}: exported {}x{}, expected {}x{} ({rect:?})",
        image.width(),
        image.height(),
        rect.width,
        rect.height
    );
}

/// niri 26.04 不返回平铺窗口位置；验证用列索引重建的焦点窗口是默认选区，
/// 且悬停到另一个窗口内的小控件时，导出尺寸真正变成该控件而不是整窗。
#[test]
#[ignore = "needs a live niri session with at least two visible windows; saves two PNGs"]
fn niri_window_seeds_and_inner_candidates_drive_selection() {
    if Backend::detect().unwrap() != Backend::Niri {
        eprintln!("skip: not a niri session");
        return;
    }
    let (_temp, dir) = directory("niri-selection");
    let output = compositor::select_output(&compositor::outputs().unwrap(), None).unwrap();
    let before = capture(&output.name);
    let seeds = compositor::window_regions(&output, (before.width(), before.height()));
    assert!(
        seeds.len() >= 2,
        "niri 应给出至少两个可见窗口种子: {seeds:?}"
    );
    let candidates = smart_select::detect(&before, &seeds);
    // 选一个位于非焦点窗口内、足够大的小控件，避免点到窗口边框或整窗。
    let target = candidates
        .iter()
        .filter(|candidate| !candidate.seed && candidate.quality >= 0.8)
        .filter(|candidate| {
            (40..=400).contains(&candidate.rect.width)
                && (24..=240).contains(&candidate.rect.height)
        })
        .filter(|candidate| {
            seeds
                .iter()
                .skip(1)
                .any(|window| contains_rect(*window, candidate.rect))
        })
        .min_by_key(|candidate| u64::from(candidate.rect.width) * u64::from(candidate.rect.height))
        .map(|candidate| candidate.rect)
        .expect("非焦点窗口内应有小控件候选");
    println!("焦点窗口 {:?}，悬停目标 {:?}", seeds[0], target);

    let input = Input::new((output.width, output.height));
    let scale = f64::from(before.width()) / f64::from(output.width);
    let export = dir.join("export");

    // 1) 首帧默认选中焦点窗口。
    let mut editor = Editor::start(&dir, false);
    pause(500);
    input.key(46, "s", true);
    editor.wait();
    assert_size(&newest_export(&export), seeds[0], "initial selection");

    // 2) 悬停到另一个窗口内的小控件，导出应为该控件。
    let mut editor = Editor::start(&dir, false);
    pause(500);
    let center = (
        f64::from(target.x + target.width as i32 / 2) / scale,
        f64::from(target.y + target.height as i32 / 2) / scale,
    );
    input.move_to(center.0, center.1);
    pause(300);
    input.key(46, "s", true);
    editor.wait();
    assert_size(&newest_export(&export), target, "hovered candidate");
    println!("niri 窗口种子与窗口内候选通过: {}", dir.display());
}
