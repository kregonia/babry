use babry::{
    capture, cli,
    compositor::{self, Backend},
    config, daemon, pin,
    selection::{self, RegionDetector, SelectionCursor},
    startup::StartupTrace,
};
use controller::EditorController;
use image::RgbaImage;
use slint::{ComponentHandle, PhysicalSize, SharedPixelBuffer};
use std::{
    cell::RefCell,
    path::PathBuf,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};

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
    dispatch_command(cli::parse(std::env::args())?, None)
}

fn dispatch_command(command: cli::Command, save_dir: Option<PathBuf>) -> Result<(), String> {
    match command {
        cli::Command::Help => {
            println!("{}", cli::usage());
            Ok(())
        }
        cli::Command::Region => run_capture_or_forward(false, None, save_dir),
        cli::Command::Screen => run_capture_or_forward(true, None, save_dir),
        cli::Command::Pin(path) => pin::launch(&path, None),
        cli::Command::PinSurface(path) => pin::run_surface(&path, None),
        cli::Command::OnOutput { command, name } => match *command {
            cli::Command::Region => run_capture_or_forward(false, Some(name), save_dir),
            cli::Command::Screen => run_capture_or_forward(true, Some(name), save_dir),
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
        cli::Command::Daemon => run_daemon(),
        cli::Command::DaemonStatus => daemon_status(),
        cli::Command::StopDaemon => stop_daemon(),
        cli::Command::Standalone(command) => dispatch_standalone(*command, save_dir),
        cli::Command::WithSaveDir { command, directory } => {
            dispatch_command(*command, Some(directory))
        }
    }
}

fn dispatch_standalone(command: cli::Command, save_dir: Option<PathBuf>) -> Result<(), String> {
    let save_dir = config::resolve_save_dir(save_dir.as_deref())?;
    match command {
        cli::Command::Region => run_capture(false, None, save_dir),
        cli::Command::Screen => run_capture(true, None, save_dir),
        cli::Command::OnOutput { command, name } => match *command {
            cli::Command::Region => run_capture(false, Some(name), save_dir),
            cli::Command::Screen => run_capture(true, Some(name), save_dir),
            _ => Err("--standalone 仅用于截图".into()),
        },
        _ => Err("--standalone 仅用于截图".into()),
    }
}

fn run_capture_or_forward(
    direct_edit: bool,
    output: Option<String>,
    save_dir: Option<PathBuf>,
) -> Result<(), String> {
    // 在调用端读取配置和解析相对目录，再传给后台，保持两种模式完全一致。
    let save_dir = config::resolve_save_dir(save_dir.as_deref())?;
    let request = daemon::Request::Capture {
        direct_edit,
        output: output.clone(),
        save_dir: save_dir.clone(),
    };
    if let Some(reply) = daemon::send(&request)? {
        return reply.ok.then_some(()).ok_or(reply.message);
    }
    run_capture(direct_edit, output, save_dir)
}

fn daemon_status() -> Result<(), String> {
    match daemon::send(&daemon::Request::Status)? {
        Some(reply) if reply.ok => {
            println!("{}", reply.message);
            Ok(())
        }
        Some(reply) => Err(reply.message),
        None => {
            println!("后台未运行");
            Ok(())
        }
    }
}

fn stop_daemon() -> Result<(), String> {
    match daemon::send(&daemon::Request::Stop)? {
        Some(reply) if reply.ok => {
            println!("{}", reply.message);
            Ok(())
        }
        Some(reply) => Err(reply.message),
        None => {
            println!("后台未运行");
            Ok(())
        }
    }
}

fn capture_source(
    output: Option<String>,
    mode: &'static str,
) -> Result<(RgbaImage, String, StartupTrace, Backend), String> {
    let trace = StartupTrace::new(mode);
    let backend = Backend::detect()?;
    let compositor = backend.compositor();
    let outputs = compositor.outputs()?;
    let selected = compositor::select_output(&outputs, output.as_deref())?;
    trace.mark("output");
    let base = compositor.capture(&selected.name)?;
    // 读取与解码都包含在 capture 内，只记录像素可用点，不单独拆分解码耗时。
    trace.mark("capture-ready");
    Ok((base, selected.name, trace, backend))
}

fn run_capture(direct_edit: bool, output: Option<String>, save_dir: PathBuf) -> Result<(), String> {
    let (base, output_name, trace, backend) = capture_source(output, "standalone")?;
    run_editor(base, direct_edit, output_name, trace, backend, save_dir)
}

fn run_editor(
    base: RgbaImage,
    direct_edit: bool,
    output_name: String,
    trace: StartupTrace,
    backend: Backend,
    save_dir: PathBuf,
) -> Result<(), String> {
    let app = AppWindow::new().map_err(|error| error.to_string())?;
    // KDE 的浮动窗口会被面板挤压；平铺合成器已有自己的全屏规则，不改变其行为。
    if backend == Backend::Kde {
        app.window().set_fullscreen(true);
    }
    slint::set_xdg_app_id("babry").map_err(|error| error.to_string())?;
    attach_startup_trace(&app);
    let controller = EditorController::install(&app, Arc::new(RgbaImage::new(1, 1)));
    let weak = app.as_weak();
    app.on_session_finished(move || {
        if let Some(app) = weak.upgrade() {
            app.hide().ok();
        }
    });
    configure_editor(
        &app,
        controller,
        base,
        direct_edit,
        output_name,
        trace,
        save_dir,
    )?;
    app.run().map_err(|error| error.to_string())
}

fn configure_editor(
    app: &AppWindow,
    controller: Rc<EditorController>,
    base: RgbaImage,
    direct_edit: bool,
    output_name: String,
    trace: StartupTrace,
    save_dir: PathBuf,
) -> Result<(), String> {
    if base.width() == 0 || base.height() == 0 {
        return Err("截图尺寸为空".into());
    }
    let base_image = Arc::new(base);
    controller.reset_session(app, base_image.clone());
    app.window()
        .set_size(PhysicalSize::new(base_image.width(), base_image.height()));
    app.set_screenshot(image_to_slint(&base_image));
    trace.mark("ui-prepared");
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
                drop(base_image);
                let _ = weak.upgrade_in_event_loop(move |app| {
                    if !task_cancelled.load(Ordering::Relaxed) {
                        app.set_selection_loading(false);
                        if app.get_phase() == 0 {
                            install_selection_detector(&app, detector);
                        }
                    } else {
                        drop(detector);
                        // 关闭窗口时 CV 可能还在完成当前阶段；最终缓冲区释放后再回收一次。
                        if app.get_screenshot().size().width == 0 {
                            babry::startup::reclaim_idle_memory();
                        }
                    }
                });
            })
            .map_err(|error| format!("无法启动选区分析: {error}"))?;
    }

    app.on_save(export_action(
        app,
        controller.clone(),
        ExportAction::Save,
        save_dir.clone(),
    ));
    app.on_copy(export_action(
        app,
        controller.clone(),
        ExportAction::Copy,
        save_dir.clone(),
    ));
    app.on_pin(export_action(
        app,
        controller,
        ExportAction::Pin(output_name),
        save_dir,
    ));
    FIRST_RENDER_TRACE.with(|slot| *slot.borrow_mut() = trace.enabled().then_some(trace));
    app.show().map_err(|error| error.to_string())?;
    app.invoke_focus_editor();
    Ok(())
}

// 窗口和 Rc 只驻留 UI 线程；后台线程通过事件循环提交截图结果，空闲没有轮询定时器。
struct DaemonRuntime {
    app: AppWindow,
    controller: Rc<EditorController>,
}
thread_local! {
    static DAEMON_RUNTIME: RefCell<Option<DaemonRuntime>> = const { RefCell::new(None) };
    static FIRST_RENDER_TRACE: RefCell<Option<StartupTrace>> = const { RefCell::new(None) };
}

fn attach_startup_trace(app: &AppWindow) {
    if std::env::var_os("BABRY_TRACE_STARTUP").is_none() {
        return;
    }
    if let Err(error) = app.window().set_rendering_notifier(|state, _| {
        if matches!(state, slint::RenderingState::AfterRendering) {
            FIRST_RENDER_TRACE.with(|slot| {
                if let Some(trace) = slot.borrow_mut().take() {
                    trace.mark("first-render");
                }
            });
        }
    }) {
        eprintln!("启动首帧埋点不可用: {error}");
    }
}

fn run_daemon() -> Result<(), String> {
    let server = daemon::Server::bind()?;
    let app = AppWindow::new().map_err(|error| error.to_string())?;
    slint::set_xdg_app_id("babry").map_err(|error| error.to_string())?;
    attach_startup_trace(&app);
    app.window().set_size(PhysicalSize::new(1, 1));
    let controller = EditorController::install(&app, Arc::new(RgbaImage::new(1, 1)));
    let busy = Arc::new(AtomicBool::new(false));
    let weak = app.as_weak();
    let session_busy = busy.clone();
    let session_controller = controller.clone();
    app.on_session_finished(move || {
        if let Some(app) = weak.upgrade() {
            app.hide().ok();
            session_controller.release_session(&app);
            FIRST_RENDER_TRACE.with(|slot| {
                slot.borrow_mut().take();
            });
            // 等当前回调栈和 Slint 旧属性值释放后再回收堆页，不添加空闲轮询。
            slint::Timer::single_shot(
                std::time::Duration::ZERO,
                babry::startup::reclaim_idle_memory,
            );
        }
        session_busy.store(false, Ordering::Release);
    });
    let weak = app.as_weak();
    app.window().on_close_requested(move || {
        if let Some(app) = weak.upgrade() {
            app.invoke_session_finished();
        }
        slint::CloseRequestResponse::KeepWindowShown
    });
    DAEMON_RUNTIME.with(|runtime| {
        *runtime.borrow_mut() = Some(DaemonRuntime {
            app: app.clone_strong(),
            controller,
        })
    });
    // UI 创建完成后才启动监听；客户端不会收到“已就绪”却访问尚未初始化的窗口。
    thread::Builder::new()
        .name("babry-daemon-ipc".into())
        .spawn(move || {
            let listener_busy = busy;
            let result = server.run(|request| match request {
                daemon::Request::Status => {
                    daemon::Reply::ok(if listener_busy.load(Ordering::Acquire) {
                        "后台运行中：正在处理截图"
                    } else {
                        "后台运行中：空闲"
                    })
                }
                daemon::Request::Stop => daemon::Reply::ok("后台正在退出"),
                daemon::Request::Capture {
                    direct_edit,
                    output,
                    save_dir,
                } => {
                    if listener_busy
                        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                        .is_err()
                    {
                        return daemon::Reply::error("后台正在处理上一张截图，请先保存或取消");
                    }
                    let worker_busy = listener_busy.clone();
                    let spawned =
                        thread::Builder::new()
                            .name("babry-capture".into())
                            .spawn(move || {
                                let result = capture_source(output, "daemon");
                                let completed_busy = worker_busy.clone();
                                if slint::invoke_from_event_loop(move || {
                                    DAEMON_RUNTIME.with(|runtime| {
                                        let runtime = runtime.borrow();
                                        let Some(runtime) = runtime.as_ref() else {
                                            completed_busy.store(false, Ordering::Release);
                                            return;
                                        };
                                        let result = result.and_then(
                                            |(base, output_name, trace, backend)| {
                                                if backend == Backend::Kde {
                                                    runtime.app.window().set_fullscreen(true);
                                                }
                                                configure_editor(
                                                    &runtime.app,
                                                    runtime.controller.clone(),
                                                    base,
                                                    direct_edit,
                                                    output_name,
                                                    trace,
                                                    save_dir,
                                                )
                                            },
                                        );
                                        if let Err(error) = result {
                                            eprintln!("babry daemon: {error}");
                                            capture::notify(&error);
                                            runtime.app.invoke_session_finished();
                                        }
                                    });
                                })
                                .is_err()
                                {
                                    worker_busy.store(false, Ordering::Release);
                                }
                            });
                    match spawned {
                        Ok(_) => daemon::Reply::ok("截图请求已接收"),
                        Err(error) => {
                            listener_busy.store(false, Ordering::Release);
                            daemon::Reply::error(format!("无法启动截图线程: {error}"))
                        }
                    }
                }
            });
            if let Err(error) = result {
                eprintln!("babry daemon: {error}");
            }
            // Server 返回前已经发送停止确认并关闭 socket，避免进程退出抢先截断响应。
            let _ = slint::invoke_from_event_loop(|| {
                let _ = slint::quit_event_loop();
            });
        })
        .map_err(|error| format!("无法启动后台监听线程: {error}"))?;
    let result = slint::run_event_loop_until_quit().map_err(|error| error.to_string());
    DAEMON_RUNTIME.with(|runtime| {
        if let Some(runtime) = runtime.borrow_mut().take() {
            runtime.controller.release_session(&runtime.app);
        }
    });
    result
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
    save_dir: PathBuf,
) -> impl Fn() + 'static {
    let weak = app.as_weak();
    move || {
        let Some(app) = weak.upgrade() else { return };
        let document = controller.document.borrow();
        let result = config::output_path(&save_dir).and_then(|output| {
            document.export(&output)?;
            match &action {
                ExportAction::Save => (),
                ExportAction::Copy => capture::copy_png(&output)?,
                ExportAction::Pin(name) => pin::launch(&output, Some(name.clone()))?,
            }
            Ok(output)
        });
        drop(document);
        match result {
            Ok(output) => {
                capture::notify(match action {
                    ExportAction::Save => "截图已保存",
                    ExportAction::Copy => "截图已复制到剪贴板",
                    ExportAction::Pin(_) => "截图已贴到屏幕",
                });
                eprintln!("已保存: {}", output.display());
                app.invoke_session_finished();
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
