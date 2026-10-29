//! Babry native Wayland editor.
//!
//! The GUI path is an SCTK layer-shell surface backed by wl_shm. The daemon is
//! only a Unix-socket launcher: it does not initialize Wayland while idle.

use babry::{cli, compositor, config, kde, native_pin, screencopy, smart_select};
use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Instant,
};

mod editor;
mod scene;

#[path = "daemon.rs"]
mod daemon;

fn main() -> Result<(), String> {
    if let Err(error) = dispatch(cli::parse(std::env::args())?) {
        let _ = Command::new("notify-send")
            .args(["--app-name", "babry", "Babry", &error])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        eprintln!("babry: {error}");
        return Err(error);
    }
    Ok(())
}

fn dispatch(command: cli::Command) -> Result<(), String> {
    match command {
        cli::Command::Help => {
            println!("{}", cli::usage());
            Ok(())
        }
        cli::Command::ListOutputs => list_outputs(),
        cli::Command::Daemon => run_daemon(),
        cli::Command::DaemonStatus => daemon_status(),
        cli::Command::StopDaemon => stop_daemon(),
        cli::Command::Pin(path) => native_pin(&path, None),
        cli::Command::PinSurface(path) => native_pin(&path, None),
        cli::Command::Region => capture_or_forward(false, None, None),
        cli::Command::Screen => capture_or_forward(true, None, None),
        cli::Command::OnOutput { command, name } => match *command {
            cli::Command::Region => capture_or_forward(false, Some(name), None),
            cli::Command::Screen => capture_or_forward(true, Some(name), None),
            cli::Command::Pin(path) => native_pin(&path, Some(name)),
            cli::Command::PinSurface(path) => native_pin(&path, Some(name)),
            _ => Err("--output 组合无效".into()),
        },
        cli::Command::Standalone(command) => dispatch_standalone(*command, None),
        cli::Command::WithSaveDir { command, directory } => {
            dispatch_wrapped(*command, Some(directory))
        }
        cli::Command::DebugSelection { source, directory } => debug_selection(&source, &directory),
    }
}

fn dispatch_wrapped(command: cli::Command, save_dir: Option<PathBuf>) -> Result<(), String> {
    match command {
        cli::Command::Standalone(command) => dispatch_standalone(*command, save_dir),
        cli::Command::Region => capture_or_forward(false, None, save_dir),
        cli::Command::Screen => capture_or_forward(true, None, save_dir),
        cli::Command::OnOutput { command, name } => match *command {
            cli::Command::Region => capture_or_forward(false, Some(name), save_dir),
            cli::Command::Screen => capture_or_forward(true, Some(name), save_dir),
            _ => Err("--save-dir 组合无效".into()),
        },
        _ => Err("--save-dir 仅用于截图".into()),
    }
}

fn dispatch_standalone(command: cli::Command, save_dir: Option<PathBuf>) -> Result<(), String> {
    match command {
        cli::Command::Region => capture_session(false, None, save_dir),
        cli::Command::Screen => capture_session(true, None, save_dir),
        cli::Command::OnOutput { command, name } => match *command {
            cli::Command::Region => capture_session(false, Some(name), save_dir),
            cli::Command::Screen => capture_session(true, Some(name), save_dir),
            _ => Err("--standalone 仅用于截图".into()),
        },
        _ => Err("--standalone 仅用于截图".into()),
    }
}

fn capture_or_forward(
    direct_edit: bool,
    output: Option<String>,
    save_dir: Option<PathBuf>,
) -> Result<(), String> {
    let directory = config::resolve_save_dir(save_dir.as_deref())?;
    let request = daemon::Request::Capture {
        direct_edit,
        output: output.clone(),
        save_dir: directory.clone(),
    };
    if let Some(reply) = daemon::send(&request)? {
        return reply.ok.then_some(()).ok_or(reply.message);
    }
    capture_session(direct_edit, output, Some(directory))
}

fn capture_session(
    direct_edit: bool,
    output: Option<String>,
    save_dir: Option<PathBuf>,
) -> Result<(), String> {
    let started = Instant::now();
    let trace = std::env::var_os("BABRY_TRACE").is_some();
    let outputs = compositor::outputs()?;
    let selected = compositor::select_output(&outputs, output.as_deref())?;
    let source = match screencopy::capture(Some(&selected.name)) {
        Ok(image) => image,
        // KDE 只提供 ScreenShot2；像素经 pipe 直接读入 memfd。
        Err(error) if compositor::Backend::detect()? == compositor::Backend::Kde => {
            kde::capture(&selected.name).map_err(|_| error)?
        }
        Err(error) => return Err(error),
    };
    if trace {
        eprintln!(
            "babry: 截图完成 {}x{} {:.1}ms",
            source.width(),
            source.height(),
            started.elapsed().as_secs_f64() * 1000.0
        );
    }
    let directory = config::resolve_save_dir(save_dir.as_deref())?;
    let full = babry::core::RectPx::new(0, 0, source.width(), source.height());
    let source = Arc::new(source);
    let (candidates, selection) = if direct_edit {
        (None, full)
    } else {
        let seeds = compositor::window_regions(&selected, (source.width(), source.height()));
        // 智能选区分析与 Wayland 初始化并行；首帧先显示合成器给出的焦点窗口。
        let (sender, receiver) = std::sync::mpsc::channel();
        let image = source.clone();
        let detect_seeds = seeds.clone();
        thread::spawn(move || {
            let started = Instant::now();
            let candidates = smart_select::detect(&image, &detect_seeds);
            if trace {
                eprintln!(
                    "babry: 智能选区 {} 个候选 {:.1}ms",
                    candidates.len(),
                    started.elapsed().as_secs_f64() * 1000.0
                );
            }
            let _ = sender.send(candidates);
        });
        let selection = seeds
            .iter()
            .copied()
            .max_by_key(|rect| u64::from(rect.width) * u64::from(rect.height))
            .unwrap_or(full);
        (Some(receiver), selection)
    };
    editor::NativeEditor::run(editor::Session {
        source,
        direct_edit,
        save_dir: directory,
        output: Some(selected.name.clone()),
        logical: (selected.width, selected.height),
        selection,
        candidates,
    })
}

/// 离线诊断：输出候选框 JSON 和叠加图，便于在真实截图上评估智能选区。
fn debug_selection(source: &Path, directory: &Path) -> Result<(), String> {
    let image = babry::shm_image::ShmImage::read_png(source)?;
    let started = Instant::now();
    let candidates = smart_select::detect(&image, &[]);
    let elapsed = started.elapsed().as_secs_f64() * 1000.0;
    std::fs::create_dir_all(directory).map_err(|e| e.to_string())?;
    let list: Vec<_> = candidates
        .iter()
        .map(|c| {
            serde_json::json!({
                "x": c.rect.x, "y": c.rect.y, "width": c.rect.width, "height": c.rect.height,
                "quality": c.quality, "seed": c.seed,
            })
        })
        .collect();
    let report = serde_json::json!({
        "width": image.width(), "height": image.height(),
        "elapsed_ms": elapsed, "candidates": list,
    });
    std::fs::write(
        directory.join("candidates.json"),
        serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    let mut pixmap = tiny_skia::Pixmap::new(image.width(), image.height()).ok_or("图像尺寸无效")?;
    let row = image.width() as usize * 4;
    for (y, line) in pixmap.data_mut().chunks_exact_mut(row).enumerate() {
        image.read_row(0, y as i32, line);
    }
    for c in &candidates {
        let mut paint = tiny_skia::Paint::default();
        let good = c.quality >= 0.72;
        paint.set_color_rgba8(
            if good { 40 } else { 255 },
            if good { 220 } else { 80 },
            60,
            200,
        );
        if let Some(rect) = tiny_skia::Rect::from_xywh(
            c.rect.x as f32 + 0.5,
            c.rect.y as f32 + 0.5,
            c.rect.width as f32 - 1.0,
            c.rect.height as f32 - 1.0,
        ) {
            let path = tiny_skia::PathBuilder::from_rect(rect);
            let stroke = tiny_skia::Stroke {
                width: 1.0,
                ..Default::default()
            };
            pixmap.stroke_path(
                &path,
                &paint,
                &stroke,
                tiny_skia::Transform::identity(),
                None,
            );
        }
    }
    let file =
        std::fs::File::create(directory.join("candidates.png")).map_err(|e| e.to_string())?;
    let mut encoder =
        png::Encoder::new(std::io::BufWriter::new(file), image.width(), image.height());
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()
        .and_then(|mut writer| writer.write_image_data(pixmap.data()))
        .map_err(|e| e.to_string())?;
    println!("{} 个候选，{elapsed:.1}ms", candidates.len());
    Ok(())
}

fn list_outputs() -> Result<(), String> {
    for output in compositor::outputs()? {
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

fn native_pin(path: &Path, output: Option<String>) -> Result<(), String> {
    native_pin::run_surface(path, output)
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

fn run_daemon() -> Result<(), String> {
    let server = daemon::Server::bind()?;
    let busy = Arc::new(AtomicBool::new(false));
    let current = std::env::current_exe().map_err(|error| error.to_string())?;
    let worker_busy = busy.clone();
    server.run(|request| match request {
        daemon::Request::Status => daemon::Reply::ok(if busy.load(Ordering::Acquire) {
            "后台运行中：正在处理截图"
        } else {
            "后台运行中：空闲"
        }),
        daemon::Request::Stop => daemon::Reply::ok("后台正在退出"),
        daemon::Request::Capture {
            direct_edit,
            output,
            save_dir,
        } => {
            if busy
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                return daemon::Reply::error("后台正在处理上一张截图，请先保存或取消");
            }
            let mut command = vec!["--standalone".to_string()];
            if direct_edit {
                command.push("--screen".into());
            }
            if let Some(output) = output {
                command.extend(["--output".into(), output]);
            }
            command.extend(["--save-dir".into(), save_dir.to_string_lossy().into_owned()]);
            let busy = worker_busy.clone();
            match Command::new(&current)
                .args(command)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()
            {
                Ok(mut child) => {
                    thread::spawn(move || {
                        let _ = child.wait();
                        busy.store(false, Ordering::Release);
                    });
                    daemon::Reply::ok("截图请求已接收")
                }
                Err(error) => {
                    worker_busy.store(false, Ordering::Release);
                    daemon::Reply::error(format!("无法启动截图会话: {error}"))
                }
            }
        }
    })
}
