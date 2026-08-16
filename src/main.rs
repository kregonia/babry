mod capture;
mod editor;

use editor::Mark as NativeMark;
use image::{Rgba, RgbaImage};
use slint::{ComponentHandle, PhysicalSize, SharedPixelBuffer};
use std::{
    cell::RefCell,
    path::PathBuf,
    rc::Rc,
    time::{Duration, Instant},
};

slint::include_modules!();

type SharedMarks = Rc<RefCell<Vec<NativeMark>>>;
type PendingMark = Rc<RefCell<Option<NativeMark>>>;
type RenderCache = Rc<RefCell<PreviewCache>>;

const PREVIEW_FRAME_INTERVAL: Duration = Duration::from_millis(11);

struct PreviewCache {
    committed: RgbaImage,
    preview: RgbaImage,
    last_frame: Instant,
}

fn main() -> Result<(), slint::PlatformError> {
    if let Err(error) = run() {
        eprintln!("babry: {error}");
        std::process::exit(1);
    }
    Ok(())
}

fn run() -> Result<(), String> {
    let temp = tempfile::Builder::new()
        .suffix(".png")
        .tempfile()
        .map_err(|error| error.to_string())?;
    let source = temp.path().to_path_buf();
    capture::capture(&source)?;

    let base = image::open(&source)
        .map_err(|error| error.to_string())?
        .to_rgba8();
    let base_image = Rc::new(base);
    let app = AppWindow::new().map_err(|error| error.to_string())?;
    app.window()
        .set_size(PhysicalSize::new(base_image.width(), base_image.height()));

    let base_pixels = image_to_slint(base_image.as_ref());
    app.set_screenshot(base_pixels.clone());
    app.set_committed_image(base_pixels.clone());
    app.set_preview_image(base_pixels);

    let committed: SharedMarks = Rc::new(RefCell::new(Vec::new()));
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
        app.on_begin_mark(move |kind, x, y, color, width| {
            let rgba = slint_color_to_rgba(color);
            *pending.borrow_mut() = Some(NativeMark::new(kind, x, y, x, y, rgba, width as u32));
            let mut cache = cache.borrow_mut();
            cache.preview = cache.committed.clone();
            cache.last_frame = Instant::now() - PREVIEW_FRAME_INTERVAL;
            if let Some(app) = weak.upgrade() {
                upload_preview(&app, &mut cache, pending.borrow().as_ref(), true);
            }
        });
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
        let committed = committed.clone();
        let pending = pending.clone();
        let cache = cache.clone();
        app.on_commit_mark(move || {
            if let Some(mark) = pending.borrow_mut().take() {
                let distance = (mark.x2 - mark.x1).hypot(mark.y2 - mark.y1);
                if mark.kind == 1 || distance > 0.003 {
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
            committed.borrow_mut().pop();
            if let Some(app) = weak.upgrade() {
                rebuild_committed(&app, &base, &committed, &cache);
            }
        });
    }

    let save_action = {
        let weak = app.as_weak();
        let source = source.clone();
        let committed = committed.clone();
        move |copy_to_clipboard: bool| {
            let Some(app) = weak.upgrade() else { return };
            let output = output_path();
            let selection = (app.get_sx(), app.get_sy(), app.get_sw(), app.get_sh());
            let marks = committed.borrow();
            match editor::export(&source, &output, selection, &marks) {
                Ok(()) => {
                    if copy_to_clipboard && let Err(error) = capture::copy_png(&output) {
                        eprintln!("{error}");
                        return;
                    }
                    eprintln!("已保存: {}", output.display());
                    app.hide().ok();
                }
                Err(error) => eprintln!("导出失败: {error}"),
            }
        }
    };
    let save_action = Rc::new(save_action);

    {
        let action = save_action.clone();
        app.on_save(move || action(false));
    }
    {
        let action = save_action.clone();
        app.on_copy(move || action(true));
    }
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
    cache.committed = editor::render_marks(base.clone(), &marks.borrow());
    cache.preview = cache.committed.clone();
    let image = image_to_slint(&cache.committed);
    app.set_committed_image(image.clone());
    app.set_preview_image(image);
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
