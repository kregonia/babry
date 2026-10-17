use serde_json::{Value, json};
use std::{
    fs::{self, File, OpenOptions},
    hash::{DefaultHasher, Hash, Hasher},
    io::{self, BufRead, BufReader, Write},
    os::unix::{
        fs::{DirBuilderExt, MetadataExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    time::Duration,
};

const PROTOCOL: u32 = 2;
const MAX_MESSAGE: u64 = 4096;
const TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    Capture {
        direct_edit: bool,
        output: Option<String>,
        save_dir: PathBuf,
    },
    Status,
    Stop,
}

impl Request {
    fn to_json(&self) -> Result<Value, String> {
        Ok(match self {
            Self::Capture {
                direct_edit,
                output,
                save_dir,
            } => {
                let directory = save_dir
                    .to_str()
                    .ok_or("后台保存目录必须是有效 UTF-8 路径")?;
                json!({"version": PROTOCOL, "command": if *direct_edit {"screen"} else {"region"}, "output": output, "save_dir": directory})
            }
            Self::Status => json!({"version": PROTOCOL, "command": "status"}),
            Self::Stop => json!({"version": PROTOCOL, "command": "stop"}),
        })
    }

    fn from_json(value: Value) -> Result<Self, String> {
        if value["version"].as_u64() != Some(u64::from(PROTOCOL)) {
            return Err("后台协议版本不匹配，请重启 Babry 后台".into());
        }
        match value["command"].as_str() {
            Some("region" | "screen") => {
                let output = match &value["output"] {
                    Value::Null => None,
                    Value::String(name)
                        if !name.is_empty() && name.len() <= 256 && !name.contains('\0') =>
                    {
                        Some(name.clone())
                    }
                    _ => return Err("后台请求的显示器名称无效".into()),
                };
                let directory = value["save_dir"]
                    .as_str()
                    .filter(|path| !path.is_empty() && path.len() <= 3072 && !path.contains('\0'))
                    .map(PathBuf::from)
                    .filter(|path| path.is_absolute())
                    .ok_or("后台请求的保存目录必须是绝对路径")?;
                Ok(Self::Capture {
                    direct_edit: value["command"] == "screen",
                    output,
                    save_dir: directory,
                })
            }
            Some("status") => Ok(Self::Status),
            Some("stop") => Ok(Self::Stop),
            _ => Err("未知后台请求".into()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Reply {
    pub ok: bool,
    pub message: String,
}

impl Reply {
    pub fn ok(message: impl Into<String>) -> Self {
        Self {
            ok: true,
            message: message.into(),
        }
    }
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            message: message.into(),
        }
    }
}

/// 一个 Wayland 会话使用一套 socket 和文件锁，支持同一用户的多个图形会话。
pub fn socket_path() -> Result<PathBuf, String> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").ok_or("后台模式需要 XDG_RUNTIME_DIR")?;
    let display = std::env::var_os("WAYLAND_DISPLAY").ok_or("后台模式需要 Wayland 会话")?;
    let mut hash = DefaultHasher::new();
    display.hash(&mut hash);
    std::env::var_os("NIRI_SOCKET").hash(&mut hash);
    std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").hash(&mut hash);
    Ok(PathBuf::from(runtime)
        .join(format!("babry-{:x}", hash.finish()))
        .join("control.sock"))
}

/// 只在 socket 不存在或服务确实不再监听时回退；超时或已发送请求不会再次发起截图。
pub fn send(request: &Request) -> Result<Option<Reply>, String> {
    let Ok(path) = socket_path() else {
        return Ok(None);
    };
    send_to(&path, request)
}

fn send_to(path: &Path, request: &Request) -> Result<Option<Reply>, String> {
    let mut stream = match UnixStream::connect(path) {
        Ok(stream) => stream,
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(format!("无法连接 Babry 后台: {error}")),
    };
    set_timeouts(&stream)?;
    let value = request.to_json()?;
    if value.to_string().len() as u64 >= MAX_MESSAGE {
        return Err("后台请求过大，目录或显示器名称过长".into());
    }
    write_value(&mut stream, &value)?;
    let value = read_value(&stream)?;
    let ok = value["ok"].as_bool().ok_or("后台响应无效")?;
    let message = value["message"]
        .as_str()
        .ok_or("后台响应缺少说明")?
        .to_string();
    Ok(Some(Reply { ok, message }))
}

pub struct Server {
    listener: UnixListener,
    _lock: File,
    path: PathBuf,
    socket_identity: (u64, u64),
}

impl Server {
    pub fn bind() -> Result<Self, String> {
        Self::bind_at(socket_path()?)
    }

    fn bind_at(path: PathBuf) -> Result<Self, String> {
        let directory = path.parent().ok_or("后台 socket 路径无效")?;
        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700);
        match builder.create(directory) {
            Ok(()) => (),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => (),
            Err(error) => return Err(format!("无法创建后台目录: {error}")),
        }
        let metadata = fs::symlink_metadata(directory).map_err(|error| error.to_string())?;
        let parent = fs::metadata(directory.parent().ok_or("后台目录缺少父目录")?)
            .map_err(|error| error.to_string())?;
        if !metadata.is_dir()
            || metadata.uid() != parent.uid()
            || metadata.permissions().mode() & 0o077 != 0
        {
            return Err("后台目录必须是当前会话用户拥有的私有目录（权限 0700）".into());
        }
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(directory.join("daemon.lock"))
            .map_err(|error| error.to_string())?;
        lock.try_lock()
            .map_err(|_| "Babry 后台已经运行".to_string())?;
        // 锁持有期间才能清理崩溃遗留 socket，不能删除另一实例的监听端点。
        match fs::remove_file(&path) {
            Ok(()) => (),
            Err(error) if error.kind() == io::ErrorKind::NotFound => (),
            Err(error) => return Err(format!("无法清理旧后台 socket: {error}")),
        }
        let listener =
            UnixListener::bind(&path).map_err(|error| format!("无法监听后台 socket: {error}"))?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
            .map_err(|error| error.to_string())?;
        let metadata = fs::metadata(&path).map_err(|error| error.to_string())?;
        Ok(Self {
            listener,
            _lock: lock,
            path,
            socket_identity: (metadata.dev(), metadata.ino()),
        })
    }

    /// 每条连接最多一条有界消息；阻塞等待 socket，不使用空闲轮询定时器。
    pub fn run(self, mut handle: impl FnMut(Request) -> Reply) -> Result<(), String> {
        for stream in self.listener.incoming() {
            let mut stream = stream.map_err(|error| format!("后台监听失败: {error}"))?;
            if set_timeouts(&stream).is_err() {
                continue;
            }
            let (reply, stop) = match read_value(&stream).and_then(Request::from_json) {
                Ok(request) => {
                    let stop = request == Request::Stop;
                    let reply = handle(request);
                    let stop = stop && reply.ok;
                    (reply, stop)
                }
                Err(error) => (Reply::error(error), false),
            };
            let _ = write_value(
                &mut stream,
                &json!({"ok": reply.ok, "message": reply.message}),
            );
            if stop {
                return Ok(());
            }
        }
        Ok(())
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if fs::symlink_metadata(&self.path)
            .is_ok_and(|metadata| (metadata.dev(), metadata.ino()) == self.socket_identity)
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn set_timeouts(stream: &UnixStream) -> Result<(), String> {
    stream
        .set_read_timeout(Some(TIMEOUT))
        .and_then(|()| stream.set_write_timeout(Some(TIMEOUT)))
        .map_err(|error| error.to_string())
}

fn read_value(stream: &UnixStream) -> Result<Value, String> {
    use std::io::Read;
    let mut line = String::new();
    BufReader::new(stream.take(MAX_MESSAGE + 1))
        .read_line(&mut line)
        .map_err(|error| format!("读取后台消息失败: {error}"))?;
    if line.len() as u64 > MAX_MESSAGE || !line.ends_with('\n') {
        return Err("后台消息过大或不完整".into());
    }
    serde_json::from_str(&line).map_err(|error| format!("后台消息无效: {error}"))
}

fn write_value(stream: &mut UnixStream, value: &Value) -> Result<(), String> {
    writeln!(stream, "{value}").map_err(|error| format!("发送后台消息失败: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_round_trips_output_and_recovers_after_shutdown() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("private/control.sock");
        let server = Server::bind_at(path.clone()).unwrap();
        assert!(Server::bind_at(path.clone()).is_err());
        let thread = std::thread::spawn(move || {
            server
                .run(|request| match request {
                    Request::Capture {
                        direct_edit: true,
                        output: Some(name),
                        save_dir,
                    } if name == "DP-2" && save_dir == Path::new("/tmp/截图 目录") => {
                        Reply::ok("accepted")
                    }
                    Request::Stop => Reply::ok("stopped"),
                    _ => Reply::error("unexpected"),
                })
                .unwrap()
        });
        let reply = send_to(
            &path,
            &Request::Capture {
                direct_edit: true,
                output: Some("DP-2".into()),
                save_dir: "/tmp/截图 目录".into(),
            },
        )
        .unwrap()
        .unwrap();
        assert!(reply.ok);
        assert_eq!(reply.message, "accepted");
        assert!(send_to(&path, &Request::Stop).unwrap().unwrap().ok);
        thread.join().unwrap();
        assert!(!path.exists());
        let server = Server::bind_at(path.clone()).unwrap();
        drop(server);
        assert!(send_to(&path, &Request::Status).unwrap().is_none());
    }

    #[test]
    fn stale_socket_recovers_and_malformed_client_does_not_stop_service() {
        let directory = tempfile::tempdir().unwrap();
        let private = directory.path().join("private");
        fs::DirBuilder::new().mode(0o700).create(&private).unwrap();
        let path = private.join("control.sock");
        // 模拟进程被强制结束后残留的 socket 文件。
        drop(UnixListener::bind(&path).unwrap());
        assert!(path.exists());
        let server = Server::bind_at(path.clone()).unwrap();
        let worker = std::thread::spawn(move || {
            server
                .run(|request| match request {
                    Request::Stop => Reply::ok("stopped"),
                    _ => Reply::ok("alive"),
                })
                .unwrap()
        });
        let mut malformed = UnixStream::connect(&path).unwrap();
        set_timeouts(&malformed).unwrap();
        malformed.write_all(b"not json\n").unwrap();
        assert_eq!(read_value(&malformed).unwrap()["ok"], false);
        assert!(send_to(&path, &Request::Status).unwrap().unwrap().ok);
        assert!(send_to(&path, &Request::Stop).unwrap().unwrap().ok);
        worker.join().unwrap();
    }

    #[test]
    fn rejects_invalid_requests_and_non_private_directories() {
        for value in [
            json!({"version": 99, "command": "region"}),
            json!({"version": PROTOCOL, "command": "bad"}),
            json!({"version": PROTOCOL, "command": "region", "output": ""}),
            json!({"version": PROTOCOL, "command": "region", "save_dir": "relative"}),
            json!({"version": PROTOCOL, "command": "region", "save_dir": ""}),
            json!({"version": PROTOCOL, "command": "region"}),
        ] {
            assert!(Request::from_json(value).is_err());
        }
        let directory = tempfile::tempdir().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(Server::bind_at(directory.path().join("control.sock")).is_err());
    }
}
