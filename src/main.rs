use babry::{
    capture, cli, pin,
    selection::{self, RegionDetector, SelectionCursor},
};
use controller::EditorController;
use image::RgbaImage;
use slint::{ComponentHandle, PhysicalSize, SharedPixelBuffer};
use std::{cell::RefCell, path::PathBuf, rc::Rc, sync::Arc};

slint::include_modules!();
mod controller;
#[cfg(test)]
mod ui_tests;

fn main() -> Result<(), slint::PlatformError> {
    if let Err(error) = dispatch() {
        capture::notify(&error);
        eprintln!("babry: {error}");
        std::process::exit(1);
    }
    Ok(())
}

fn dispatch() -> Result<(), String> {
    let command = cli::parse(std::env::args())?;
    match command {
        cli::Command::Help => {
            println!("{}", cli::usage());
            Ok(())
        }
        cli::Command::Region => run_capture(false, None),
        cli::Command::Screen => run_capture(true, None),
        cli::Command::Pin(path) => pin::launch(&path, None),
        cli::Command::PinSurface(path) => pin::run_surface(&path, None),
        cli::Command::OnOutput { command, name } => match *command {
            cli::Command::Region => run_capture(false, Some(name)),
            cli::Command::Screen => run_capture(true, Some(name)),
            cli::Command::Pin(path) => pin::launch(&path, Some(name)),
            cli::Command::PinSurface(path) => pin::run_surface(&path, Some(name)),
            _ => unreachable!(),
        },
        cli::Command::ListOutputs => {
            for output in babry::compositor::outputs()? {
                println!(
                    "{}: {}x{} at ({}, {}) scale {}{}",
                    output.name,
                    output.width,
                    output.height,
                    output.x,
                    output.y,
                    output.scale,
                    if output.focused { " [focused]" } else { "" }
                );
            }
            Ok(())
        }
        cli::Command::DebugSelection { source, directory } => {
            selection::write_diagnostics(&source, &directory)
        }
    }
}

fn run_capture(direct_edit: bool, output: Option<String>) -> Result<(), String> {
    let outputs = babry::compositor::outputs()?;
    let selected = babry::compositor::select_output(&outputs, output.as_deref())?;
    let temp = tempfile::Builder::new()
        .suffix(".png")
        .tempfile()
        .map_err(|error| error.to_string())?;
    capture::capture(temp.path(), &selected.name)?;
    let base = image::open(temp.path())
        .map_err(|error| error.to_string())?
        .into_rgba8();
    // 解码后不再使用临时文件，保存/复制/贴图统一使用会话中的内存原图。
    drop(temp);
    run_editor(base, direct_edit, selected.name)
}

fn run_editor(base: RgbaImage, direct_edit: bool, output_name: String) -> Result<(), String> {
    if base.width() == 0 || base.height() == 0 {
        return Err("截图尺寸为空".into());
    }
    let base_image = Arc::new(base);
    let app = AppWindow::new().map_err(|error| error.to_string())?;
    slint::set_xdg_app_id("babry").map_err(|error| error.to_string())?;
    app.window()
        .set_size(PhysicalSize::new(base_image.width(), base_image.height()));
    app.set_screenshot(image_to_slint(&base_image));
    // 选区阶段不加载标注字体，也不分配 committed / preview 全屏副本。
    let controller = EditorController::install(&app, base_image.clone());
    if direct_edit {
        app.set_sx(0.0);
        app.set_sy(0.0);
        app.set_sw(1.0);
        app.set_sh(1.0);
        // 全屏截图也默认不选中标注工具；点击工具栏按钮后开始编辑。
        app.set_phase(2);
        app.invoke_editing_started();
    } else {
        app.set_selection_loading(true);
        let weak = app.as_weak();
        let cancelled = controller.selection_cancellation();
        let task_cancelled = cancelled.clone();
        std::thread::Builder::new()
            .name("babry-selection".into())
            .spawn(move || {
                let detector = RegionDetector::new_cancellable(&base_image, &task_cancelled);
                let _ = weak.upgrade_in_event_loop(move |app| {
                    app.set_selection_loading(false);
                    if app.get_phase() == 0
                        && !task_cancelled.load(std::sync::atomic::Ordering::Relaxed)
                    {
                        install_selection_detector(&app, detector);
                    }
                });
            })
            .map_err(|error| format!("无法启动选区分析: {error}"))?;
    }

    app.on_save(export_action(&app, controller.clone(), ExportAction::Save));
    app.on_copy(export_action(&app, controller.clone(), ExportAction::Copy));
    app.on_pin(export_action(
        &app,
        controller,
        ExportAction::Pin(output_name),
    ));
    app.run().map_err(|error| error.to_string())
}

fn install_selection_detector(app: &AppWindow, detector: RegionDetector) {
    let detector = Rc::new(detector);
    let cursor = Rc::new(RefCell::new(SelectionCursor::default()));
    let weak = app.as_weak();
    let hover_detector = detector.clone();
    let hover_cursor = cursor.clone();
    app.on_hover_selection(move |x, y| {
        let Some(app) = weak.upgrade() else { return };
        let region = hover_cursor.borrow_mut().hover(&hover_detector, x, y);
        set_selection(&app, &hover_detector, region);
    });
    let weak = app.as_weak();
    app.on_cycle_selection(move |x, y, direction| {
        let Some(app) = weak.upgrade() else { return };
        let region = cursor.borrow_mut().step(&detector, x, y, direction);
        set_selection(&app, &detector, region);
    });
    app.invoke_refresh_selection();
}

fn set_selection(app: &AppWindow, detector: &RegionDetector, rect: babry::core::RectPx) {
    let (sx, sy, sw, sh) = rect.to_normalized(detector.image_width(), detector.image_height());
    app.set_sx(sx);
    app.set_sy(sy);
    app.set_sw(sw);
    app.set_sh(sh);
}

enum ExportAction {
    Save,
    Copy,
    Pin(String),
}
fn export_action(
    app: &AppWindow,
    controller: Rc<EditorController>,
    action: ExportAction,
) -> impl Fn() + 'static {
    let weak = app.as_weak();
    move || {
        let Some(app) = weak.upgrade() else { return };
        let output = output_path();
        let document = controller.document.borrow();
        let result = document.export(&output).and_then(|()| match &action {
            ExportAction::Save => Ok(()),
            ExportAction::Copy => capture::copy_png(&output),
            ExportAction::Pin(name) => pin::launch(&output, Some(name.clone())),
        });
        match result {
            Ok(()) => {
                capture::notify(match action {
                    ExportAction::Save => "截图已保存",
                    ExportAction::Copy => "截图已复制到剪贴板",
                    ExportAction::Pin(_) => "截图已贴到屏幕",
                });
                eprintln!("已保存: {}", output.display());
                app.hide().ok();
            }
            Err(error) => {
                capture::notify(&format!("导出失败: {error}"));
                eprintln!("导出失败: {error}");
            }
        }
    }
}

fn image_to_slint(image: &RgbaImage) -> slint::Image {
    let pixels = SharedPixelBuffer::clone_from_slice(image.as_raw(), image.width(), image.height());
    slint::Image::from_rgba8(pixels)
}

fn output_path() -> PathBuf {
    let pictures = std::env::var_os("XDG_PICTURES_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|path| PathBuf::from(path).join("Pictures")))
        .unwrap_or_else(|| PathBuf::from("."));
    let _ = std::fs::create_dir_all(&pictures);
    pictures.join(format!(
        "babry-{}.png",
        chrono::Local::now().format("%Y%m%d-%H%M%S")
    ))
}
