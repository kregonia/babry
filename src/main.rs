use ab_glyph::Font;
use babry::{
    capture, cli,
    core::History,
    editor, pin,
    selection::{self, RegionDetector, SelectionCursor},
};
use editor::Mark as NativeMark;
use image::{Rgba, RgbaImage};
use slint::{ComponentHandle, PhysicalSize, SharedPixelBuffer};
use std::{
    cell::RefCell,
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

slint::include_modules!();

#[cfg(test)]
mod ui_tests;

type SharedMarks = Rc<RefCell<History<NativeMark>>>;
type PendingMark = Rc<RefCell<Option<NativeMark>>>;
type RenderCache = Rc<RefCell<PreviewCache>>;

const PREVIEW_FRAME_INTERVAL: Duration = Duration::from_millis(11);

struct PreviewCache {
    committed: RgbaImage,
    preview: RgbaImage,
    last_frame: Instant,
}

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
    // 在编辑器抢占焦点之前确定输出，默认启动也必须只捕获这一块屏幕。
    let outputs = babry::compositor::outputs()?;
    let selected = babry::compositor::select_output(&outputs, output.as_deref())?;
    let temp = tempfile::Builder::new()
        .suffix(".png")
        .tempfile()
        .map_err(|error| error.to_string())?;
    let source = temp.path().to_path_buf();

    capture::capture(&source, &selected.name)?;
    run_editor(source, direct_edit, selected.name)
}

fn run_editor(source: PathBuf, direct_edit: bool, output_name: String) -> Result<(), String> {
    let base = image::open(&source)
        .map_err(|error| error.to_string())?
        .to_rgba8();
    if base.width() == 0 || base.height() == 0 {
        return Err("截图尺寸为空".to_string());
    }

    let base_image = Arc::new(base);
    let app = AppWindow::new().map_err(|error| error.to_string())?;
    slint::set_xdg_app_id("babry").map_err(|error| error.to_string())?;
    app.window()
        .set_size(PhysicalSize::new(base_image.width(), base_image.height()));

    let base_pixels = image_to_slint(base_image.as_ref());
    app.set_screenshot(base_pixels.clone());
    app.set_committed_image(base_pixels.clone());
    app.set_preview_image(base_pixels);
    let text_font_families = Rc::new(register_annotation_fonts());
    set_text_font_family(&app, "", &text_font_families);
    if direct_edit {
        app.set_sx(0.0);
        app.set_sy(0.0);
        app.set_sw(1.0);
        app.set_sh(1.0);
        app.set_phase(2);
    } else {
        app.set_selection_loading(true);
        let weak = app.as_weak();
        let snapshot = base_image.clone();
        // 与界面共享原图，不为工作线程复制整屏；分析期间仍可手动拖框。
        std::thread::Builder::new()
            .name("babry-selection".into())
            .spawn(move || {
                let detector = RegionDetector::new(&snapshot);
                let _ = weak.upgrade_in_event_loop(move |app| {
                    app.set_selection_loading(false);
                    if app.get_phase() == 0 {
                        install_selection_detector(&app, detector);
                    }
                });
            })
            .map_err(|error| format!("无法启动选区分析: {error}"))?;
    }

    let committed: SharedMarks = Rc::new(RefCell::new(History::default()));
    let pending: PendingMark = Rc::new(RefCell::new(None));
    let cache: RenderCache = Rc::new(RefCell::new(PreviewCache {
        committed: base_image.as_ref().clone(),
        preview: base_image.as_ref().clone(),
        last_frame: Instant::now() - PREVIEW_FRAME_INTERVAL,
    }));

    {
        let weak = app.as_weak();
        let pending = pending.clone();
        let cache = cache.clone();
        app.on_begin_mark(
            move |kind, x, y, color, width, effect_mode, effect_strength| {
                let rgba = slint_color_to_rgba(color);
                let mark_kind = if kind == 8 && effect_mode != 0 {
                    9
                } else {
                    kind
                };
                let width = u32::try_from(width).unwrap_or(1);
                let mut mark = NativeMark::new(mark_kind, x, y, x, y, rgba, width);
                if mark_kind == 9 {
                    mark.set_effect_strength(u32::try_from(effect_strength).unwrap_or(1));
                }
                *pending.borrow_mut() = Some(mark);
                let mut cache = cache.borrow_mut();
                cache.preview = cache.committed.clone();
                cache.last_frame = Instant::now() - PREVIEW_FRAME_INTERVAL;
                if let Some(app) = weak.upgrade() {
                    upload_preview(&app, &mut cache, pending.borrow().as_ref(), true);
                }
            },
        );
    }

    {
        let weak = app.as_weak();
        let pending = pending.clone();
        let cache = cache.clone();
        app.on_update_mark(move |x, y| {
            if let Some(mark) = pending.borrow_mut().as_mut() {
                mark.update_endpoint(x, y);
            }
            if let Some(app) = weak.upgrade() {
                upload_preview(
                    &app,
                    &mut cache.borrow_mut(),
                    pending.borrow().as_ref(),
                    false,
                );
            }
        });
    }

    {
        let weak = app.as_weak();
        let base = base_image.clone();
        let pending = pending.clone();
        let families = text_font_families.clone();
        app.on_begin_text(move |x, y, color, size, normalized_width| {
            let Some(app) = weak.upgrade() else { return };
            let mut mark = NativeMark::new(10, x, y, x, y, slint_color_to_rgba(color), 1);
            mark.set_text_style(
                slint_color_to_rgba(color),
                text_size_in_image(&app, base.height(), size),
                normalized_width * base.width() as f32,
            );
            *pending.borrow_mut() = Some(mark);
            set_text_font_family(&app, "", &families);
        });
    }

    {
        let weak = app.as_weak();
        let pending = pending.clone();
        let families = text_font_families.clone();
        app.on_update_text(move |text| {
            if let Some(mark) = pending.borrow_mut().as_mut()
                && mark.kind == 10
            {
                mark.set_text(text.to_string());
            }
            // 输入阶段由透明 TextInput 显示文字，避免与图像预览重复叠加。
            if let Some(app) = weak.upgrade() {
                set_text_font_family(&app, &text, &families);
            }
        });
    }

    {
        let weak = app.as_weak();
        let base = base_image.clone();
        let pending = pending.clone();
        app.on_update_text_style(move |color, size, normalized_width| {
            let Some(app) = weak.upgrade() else { return };
            if let Some(mark) = pending.borrow_mut().as_mut()
                && mark.kind == 10
            {
                mark.set_text_style(
                    slint_color_to_rgba(color),
                    text_size_in_image(&app, base.height(), size),
                    normalized_width * base.width() as f32,
                );
            }
        });
    }

    {
        let weak = app.as_weak();
        let base = base_image.clone();
        let committed = committed.clone();
        let pending = pending.clone();
        let cache = cache.clone();
        app.on_commit_text(move |text| {
            if let Some(mut mark) = pending.borrow_mut().take()
                && mark.kind == 10
            {
                mark.set_text(text.to_string());
                if !mark.text.trim().is_empty() {
                    committed.borrow_mut().push(mark);
                }
            }
            if let Some(app) = weak.upgrade() {
                rebuild_committed(&app, &base, &committed, &cache);
            }
        });
    }

    {
        let weak = app.as_weak();
        let pending = pending.clone();
        let cache = cache.clone();
        app.on_cancel_text(move || {
            if pending
                .borrow()
                .as_ref()
                .is_some_and(|mark| mark.kind == 10)
            {
                pending.borrow_mut().take();
            }
            let mut cache = cache.borrow_mut();
            cache.preview = cache.committed.clone();
            if let Some(app) = weak.upgrade() {
                app.set_preview_image(image_to_slint(&cache.preview));
            }
        });
    }

    {
        let weak = app.as_weak();
        let base = base_image.clone();
        let committed = committed.clone();
        let pending = pending.clone();
        let cache = cache.clone();
        app.on_commit_mark(move || {
            if let Some(mark) = pending.borrow_mut().take() {
                let distance = (mark.x2 - mark.x1).hypot(mark.y2 - mark.y1);
                if matches!(mark.kind, 1 | 7 | 8 | 9) || distance > 0.003 {
                    committed.borrow_mut().push(mark);
                }
            }
            if let Some(app) = weak.upgrade() {
                rebuild_committed(&app, &base, &committed, &cache);
            }
        });
    }

    {
        let weak = app.as_weak();
        let pending = pending.clone();
        let cache = cache.clone();
        app.on_cancel_mark(move || {
            pending.borrow_mut().take();
            let mut cache = cache.borrow_mut();
            cache.preview = cache.committed.clone();
            if let Some(app) = weak.upgrade() {
                app.set_preview_image(image_to_slint(&cache.preview));
            }
        });
    }

    {
        let weak = app.as_weak();
        let base = base_image.clone();
        let committed = committed.clone();
        let pending = pending.clone();
        let cache = cache.clone();
        app.on_undo(move || {
            pending.borrow_mut().take();
            committed.borrow_mut().undo();
            if let Some(app) = weak.upgrade() {
                rebuild_committed(&app, &base, &committed, &cache);
            }
        });
    }

    {
        let weak = app.as_weak();
        let base = base_image.clone();
        let committed = committed.clone();
        let pending = pending.clone();
        let cache = cache.clone();
        app.on_redo(move || {
            pending.borrow_mut().take();
            committed.borrow_mut().redo();
            if let Some(app) = weak.upgrade() {
                rebuild_committed(&app, &base, &committed, &cache);
            }
        });
    }

    let save_action = make_export_action(&app, &source, &committed, base_image.clone(), false);
    let copy_action = make_export_action(&app, &source, &committed, base_image.clone(), true);
    let pin_action = make_pin_action(&app, &source, &committed, base_image, output_name);

    app.on_save(save_action);
    app.on_copy(copy_action);
    app.on_pin(pin_action);

    {
        let weak = app.as_weak();
        app.on_cancel(move || {
            if let Some(app) = weak.upgrade() {
                app.hide().ok();
            }
        });
    }

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
    // 如果指针已移动而尚未按下，结果到达时立即更新；不覆盖正在拖动的选区。
    app.invoke_refresh_selection();
}

// 候选切换只写回归一化坐标，界面不需要了解检测来源和评分。
fn set_selection(app: &AppWindow, detector: &RegionDetector, rect: babry::core::RectPx) {
    let (sx, sy, sw, sh) = rect.to_normalized(detector.image_width(), detector.image_height());
    app.set_sx(sx);
    app.set_sy(sy);
    app.set_sw(sw);
    app.set_sh(sh);
}

fn make_export_action(
    app: &AppWindow,
    source: &Path,
    committed: &SharedMarks,
    _base: Arc<RgbaImage>,
    copy_to_clipboard: bool,
) -> impl Fn() + 'static {
    let weak = app.as_weak();
    let source = source.to_path_buf();
    let committed = committed.clone();
    move || {
        let Some(app) = weak.upgrade() else { return };
        let output = output_path();
        let selection = (app.get_sx(), app.get_sy(), app.get_sw(), app.get_sh());
        let marks = committed.borrow();
        match editor::export(&source, &output, selection, marks.entries()) {
            Ok(()) => {
                if copy_to_clipboard && let Err(error) = capture::copy_png(&output) {
                    capture::notify(&format!("复制失败: {error}"));
                    eprintln!("{error}");
                    return;
                }
                capture::notify(if copy_to_clipboard {
                    "截图已复制到剪贴板"
                } else {
                    "截图已保存"
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

fn make_pin_action(
    app: &AppWindow,
    source: &Path,
    committed: &SharedMarks,
    _base: Arc<RgbaImage>,
    output_name: String,
) -> impl Fn() + 'static {
    let weak = app.as_weak();
    let source = source.to_path_buf();
    let committed = committed.clone();
    move || {
        let Some(app) = weak.upgrade() else { return };
        let output = output_path();
        let selection = (app.get_sx(), app.get_sy(), app.get_sw(), app.get_sh());
        let marks = committed.borrow();
        match editor::export(&source, &output, selection, marks.entries()) {
            Ok(()) => match pin::launch(&output, Some(output_name.clone())) {
                Ok(()) => {
                    capture::notify("截图已贴到屏幕");
                    eprintln!("已贴图: {}", output.display());
                    app.hide().ok();
                }
                Err(error) => eprintln!("贴图失败: {error}"),
            },
            Err(error) => {
                capture::notify(&format!("导出失败: {error}"));
                eprintln!("导出失败: {error}");
            }
        }
    }
}

fn upload_preview(
    app: &AppWindow,
    cache: &mut PreviewCache,
    pending: Option<&NativeMark>,
    force: bool,
) {
    let now = Instant::now();
    if !force && now.duration_since(cache.last_frame) < PREVIEW_FRAME_INTERVAL {
        return;
    }

    cache.preview = cache.committed.clone();
    if let Some(mark) = pending {
        editor::render_mark_onto(&mut cache.preview, mark);
    }
    app.set_preview_image(image_to_slint(&cache.preview));
    cache.last_frame = now;
}

fn rebuild_committed(app: &AppWindow, base: &RgbaImage, marks: &SharedMarks, cache: &RenderCache) {
    let mut cache = cache.borrow_mut();
    let history = marks.borrow();
    cache.committed = editor::render_marks(base.clone(), history.entries());
    cache.preview = cache.committed.clone();
    let image = image_to_slint(&cache.committed);
    app.set_committed_image(image.clone());
    app.set_preview_image(image);
}

// 将导出使用的字体注册到 Slint，记录每个字体集合索引对应的字体家族。
fn register_annotation_fonts() -> Vec<String> {
    use slint::fontique_010::{fontique, shared_collection};
    let mut collection = shared_collection();
    editor::annotation_fonts()
        .iter()
        .map(|entry| {
            let data = fontique::Blob::new(Arc::new(entry.font.font_data().to_vec()));
            let registered = collection.register_fonts(data, None);
            registered
                .iter()
                .find(|(_, fonts)| fonts.iter().any(|font| font.index() == entry.index))
                .and_then(|(id, _)| collection.family_name(*id))
                .unwrap_or_default()
                .to_string()
        })
        .collect()
}

fn set_text_font_family(app: &AppWindow, text: &str, families: &[String]) {
    if let Some(index) = editor::annotation_font_index(text)
        && let Some(family) = families.get(index)
    {
        app.set_text_font_family(family.into());
    }
}

// 逻辑字号按截图在窗口中的显示比例换算，保持高分屏输入与导出尺寸一致。
fn text_size_in_image(app: &AppWindow, image_height: u32, size: i32) -> f32 {
    let logical_height = app.window().size().height as f32 / app.window().scale_factor();
    size.max(1) as f32 * image_height as f32 / logical_height.max(1.0)
}

fn image_to_slint(image: &RgbaImage) -> slint::Image {
    let pixels = SharedPixelBuffer::clone_from_slice(image.as_raw(), image.width(), image.height());
    slint::Image::from_rgba8(pixels)
}

fn slint_color_to_rgba(color: slint::Color) -> Rgba<u8> {
    let argb = color.to_argb_u8();
    Rgba([argb.red, argb.green, argb.blue, argb.alpha])
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
