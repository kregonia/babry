use super::{AppWindow, image_to_slint};
use babry::{
    document::EditorDocument,
    editor::{self, Mark},
};
use image::{Rgba, RgbaImage};
use slint::ComponentHandle;
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

const PREVIEW_INTERVAL: Duration = Duration::from_millis(16);

/// 只持有界面弱引用；图像数据由文档统一管理。
pub struct EditorController {
    pub document: RefCell<EditorDocument>,
    families: RefCell<HashMap<usize, String>>,
    preview_timer: slint::Timer,
    dirty: Cell<bool>,
    last_frame: Cell<Instant>,
    selection_cancelled: RefCell<Arc<AtomicBool>>,
}

impl EditorController {
    pub fn install(app: &AppWindow, source: Arc<RgbaImage>) -> Rc<Self> {
        let controller = Rc::new(Self {
            document: RefCell::new(EditorDocument::new(source)),
            families: RefCell::new(HashMap::new()),
            preview_timer: slint::Timer::default(),
            dirty: Cell::new(false),
            last_frame: Cell::new(Instant::now() - PREVIEW_INTERVAL),
            selection_cancelled: RefCell::new(Arc::new(AtomicBool::new(false))),
        });
        {
            let weak = app.as_weak();
            let state = controller.clone();
            app.on_editing_started(move || {
                if let Some(app) = weak.upgrade() {
                    state.document.borrow_mut().select(selection(&app));
                    state.cancel_selection();
                    // 阶段切换后释放候选回调所持有的检测器。
                    app.on_hover_selection(|_, _| {});
                    app.on_cycle_selection(|_, _, _| {});
                }
            });
        }
        {
            let weak = app.as_weak();
            let state = controller.clone();
            app.on_selection_changed(move |x, y, width, height| {
                let Some(app) = weak.upgrade() else { return };
                state.stop_preview();
                state.document.borrow_mut().select((x, y, width, height));
                state.publish_selection(&app);
            });
        }
        {
            let weak = app.as_weak();
            let state = controller.clone();
            app.on_begin_mark(move |kind, x, y, color, width, effect_mode, strength| {
                let Some(app) = weak.upgrade() else { return };
                let kind = if kind == 8 && effect_mode != 0 {
                    9
                } else {
                    kind
                };
                let mut mark = Mark::new(
                    kind,
                    x,
                    y,
                    x,
                    y,
                    rgba(color),
                    u32::try_from(width).unwrap_or(1),
                );
                if matches!(kind, 8 | 9) {
                    mark.set_effect_strength(u32::try_from(strength).unwrap_or(1));
                }
                let mut document = state.document.borrow_mut();
                document.select(selection(&app));
                if kind == 11 {
                    mark.number = document.next_number();
                    mark.font_size = text_size(
                        &app,
                        document.source().height(),
                        app.get_text_font_size().round() as i32,
                    );
                }
                document.begin(mark);
                drop(document);
                state.dirty.set(true);
                state.flush_preview(&app);
            });
        }
        {
            let weak = app.as_weak();
            let state = controller.clone();
            app.on_update_mark(move |x, y| {
                if let Some(mark) = state.document.borrow_mut().pending_mut() {
                    mark.update_endpoint(x, y);
                }
                if let Some(app) = weak.upgrade() {
                    state.request_preview(&app);
                }
            });
        }
        {
            let weak = app.as_weak();
            let state = controller.clone();
            app.on_commit_mark(move || {
                state.stop_preview();
                let changed = state.document.borrow_mut().commit();
                if let Some(app) = weak.upgrade() {
                    state.publish_committed(&app, changed);
                }
            });
        }
        {
            let weak = app.as_weak();
            let state = controller.clone();
            app.on_cancel_mark(move || {
                state.stop_preview();
                state.document.borrow_mut().cancel();
                if let Some(app) = weak.upgrade() {
                    clear_preview(&app);
                }
            });
        }
        {
            let weak = app.as_weak();
            let state = controller.clone();
            app.on_begin_text(move |x, y, color, size, width| {
                let Some(app) = weak.upgrade() else { return };
                let mut document = state.document.borrow_mut();
                document.select(selection(&app));
                let mut mark = Mark::new(10, x, y, x, y, rgba(color), 1);
                mark.set_text_style(
                    rgba(color),
                    text_size(&app, document.source().height(), size),
                    width * document.source().width() as f32,
                );
                document.begin(mark);
                drop(document);
                state.set_font(&app, "");
            });
        }
        {
            let weak = app.as_weak();
            let state = controller.clone();
            app.on_update_text(move |text| {
                if let Some(mark) = state.document.borrow_mut().pending_mut()
                    && mark.kind == 10
                {
                    mark.set_text(text.to_string());
                }
                if let Some(app) = weak.upgrade() {
                    state.set_font(&app, &text);
                }
            });
        }
        {
            let weak = app.as_weak();
            let state = controller.clone();
            app.on_update_text_style(move |color, size, width| {
                let Some(app) = weak.upgrade() else { return };
                let mut document = state.document.borrow_mut();
                let image_size = document.source().dimensions();
                if let Some(mark) = document.pending_mut()
                    && mark.kind == 10
                {
                    mark.set_text_style(
                        rgba(color),
                        text_size(&app, image_size.1, size),
                        width * image_size.0 as f32,
                    );
                }
            });
        }
        {
            let weak = app.as_weak();
            let state = controller.clone();
            app.on_commit_text(move |text| {
                let mut document = state.document.borrow_mut();
                if let Some(mark) = document.pending_mut()
                    && mark.kind == 10
                {
                    mark.set_text(text.to_string());
                }
                let changed = document.commit();
                drop(document);
                if let Some(app) = weak.upgrade() {
                    state.publish_committed(&app, changed);
                }
            });
        }
        {
            let weak = app.as_weak();
            let state = controller.clone();
            app.on_cancel_text(move || {
                state.document.borrow_mut().cancel();
                if let Some(app) = weak.upgrade() {
                    clear_preview(&app);
                }
            });
        }
        {
            let weak = app.as_weak();
            let state = controller.clone();
            app.on_undo(move || {
                state.stop_preview();
                let changed = state.document.borrow_mut().undo();
                if let Some(app) = weak.upgrade() {
                    state.publish_committed(&app, changed);
                }
            });
        }
        {
            let weak = app.as_weak();
            let state = controller.clone();
            app.on_redo(move || {
                state.stop_preview();
                let changed = state.document.borrow_mut().redo();
                if let Some(app) = weak.upgrade() {
                    state.publish_committed(&app, changed);
                }
            });
        }
        {
            let weak = app.as_weak();
            let state = controller.clone();
            app.on_cancel(move || {
                state.cancel_selection();
                state.stop_preview();
                if let Some(app) = weak.upgrade() {
                    app.invoke_session_finished();
                }
            });
        }
        controller
    }

    pub fn selection_cancellation(&self) -> Arc<AtomicBool> {
        self.selection_cancelled.borrow().clone()
    }

    fn cancel_selection(&self) {
        self.selection_cancelled
            .borrow()
            .store(true, Ordering::Relaxed);
    }

    /// 后台复用控制器及字体注册；每次截图使用独立取消标记，旧分析不能污染新会话。
    pub fn reset_session(&self, app: &AppWindow, source: Arc<RgbaImage>) {
        self.release_session(app);
        *self.selection_cancelled.borrow_mut() = Arc::new(AtomicBool::new(false));
        *self.document.borrow_mut() = EditorDocument::new(source);
        self.last_frame.set(Instant::now() - PREVIEW_INTERVAL);
    }

    /// 空闲时仅保留界面和字体缓存，释放截图、编辑历史、画布及检测器闭包。
    pub fn release_session(&self, app: &AppWindow) {
        self.cancel_selection();
        self.stop_preview();
        app.on_hover_selection(|_, _| {});
        app.on_cycle_selection(|_, _, _| {});
        app.invoke_reset_session();
        app.set_selection_loading(false);
        app.set_screenshot(Default::default());
        app.set_committed_image(Default::default());
        app.set_has_committed(false);
        app.set_next_number(1);
        clear_preview(app);
        *self.document.borrow_mut() = EditorDocument::new(Arc::new(RgbaImage::new(1, 1)));
    }

    fn request_preview(self: &Rc<Self>, app: &AppWindow) {
        self.dirty.set(true);
        let elapsed = self.last_frame.get().elapsed();
        if elapsed >= PREVIEW_INTERVAL {
            self.flush_preview(app);
        } else if !self.preview_timer.running() {
            let weak = app.as_weak();
            let state = Rc::downgrade(self);
            self.preview_timer.start(
                slint::TimerMode::SingleShot,
                PREVIEW_INTERVAL - elapsed,
                move || {
                    if let (Some(app), Some(state)) = (weak.upgrade(), state.upgrade()) {
                        state.flush_preview(&app);
                    }
                },
            );
        }
    }

    pub fn flush_preview(&self, app: &AppWindow) {
        if !self.dirty.replace(false) {
            return;
        }
        let document = self.document.borrow();
        if let Some(patch) = document.preview() {
            let (x, y, w, h) = patch
                .region
                .to_normalized(document.source().width(), document.source().height());
            app.set_preview_x(x);
            app.set_preview_y(y);
            app.set_preview_width(w);
            app.set_preview_height(h);
            app.set_preview_image(image_to_slint(&patch.image));
            app.set_has_preview(true);
        } else {
            clear_preview(app);
        }
        self.last_frame.set(Instant::now());
    }

    fn stop_preview(&self) {
        self.preview_timer.stop();
        self.dirty.set(false);
    }

    fn publish_selection(&self, app: &AppWindow) {
        let document = self.document.borrow();
        let (x, y, w, h) = document
            .region()
            .to_normalized(document.source().width(), document.source().height());
        app.set_sx(x);
        app.set_sy(y);
        app.set_sw(w);
        app.set_sh(h);
        drop(document);
        self.publish_committed(app, true);
    }

    fn publish_committed(&self, app: &AppWindow, changed: bool) {
        clear_preview(app);
        if !changed {
            return;
        }
        let document = self.document.borrow();
        app.set_next_number(document.next_number().min(i32::MAX as u32) as i32);
        if let Some(image) = document.committed() {
            let (x, y, w, h) = document
                .region()
                .to_normalized(document.source().width(), document.source().height());
            app.set_canvas_x(x);
            app.set_canvas_y(y);
            app.set_canvas_width(w);
            app.set_canvas_height(h);
            app.set_committed_image(image_to_slint(image));
            app.set_has_committed(true);
        } else {
            app.set_committed_image(Default::default());
            app.set_has_committed(false);
        }
    }

    fn set_font(&self, app: &AppWindow, text: &str) {
        use slint::fontique_010::{fontique, shared_collection};
        let Some((index, entry)) = editor::annotation_font_for(text) else {
            return;
        };
        let mut families = self.families.borrow_mut();
        let family = families.entry(index).or_insert_with(|| {
            let mut collection = shared_collection();
            // AnnotationFont::clone 仅克隆 FontArc；Blob 与导出共用同一字体字节。
            let registered =
                collection.register_fonts(fontique::Blob::new(Arc::new(entry.clone())), None);
            registered
                .iter()
                .find(|(_, fonts)| fonts.iter().any(|font| font.index() == entry.index))
                .and_then(|(id, _)| collection.family_name(*id))
                .unwrap_or_default()
                .to_string()
        });
        app.set_text_font_family(family.as_str().into());
    }
}

fn clear_preview(app: &AppWindow) {
    app.set_has_preview(false);
    app.set_preview_image(Default::default());
}
fn selection(app: &AppWindow) -> (f32, f32, f32, f32) {
    (app.get_sx(), app.get_sy(), app.get_sw(), app.get_sh())
}
fn rgba(color: slint::Color) -> Rgba<u8> {
    let c = color.to_argb_u8();
    Rgba([c.red, c.green, c.blue, c.alpha])
}
fn text_size(app: &AppWindow, height: u32, size: i32) -> f32 {
    let logical_height = app.window().size().height as f32 / app.window().scale_factor();
    size.max(1) as f32 * height as f32 / logical_height.max(1.0)
}
