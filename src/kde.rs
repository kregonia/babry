//! 按需加载系统 sd-bus，保留 KWin ScreenShot2 截图语义，不引入异步 D-Bus 运行时。
use crate::shm_image::{ChannelOrder, ShmImage};
use libc::{c_char, c_int, c_void};
use libloading::Library;
use std::{
    ffi::{CStr, CString},
    io::Read,
    os::fd::AsRawFd,
    ptr,
};

#[repr(C)]
struct BusError {
    name: *const c_char,
    message: *const c_char,
    need_free: c_int,
}
type Bus = c_void;
type Message = c_void;
struct Api {
    _lib: Library,
    open: unsafe extern "C" fn(*mut *mut Bus) -> c_int,
    close: unsafe extern "C" fn(*mut Bus) -> *mut Bus,
    unref: unsafe extern "C" fn(*mut Message) -> *mut Message,
    new_call: unsafe extern "C" fn(
        *mut Bus,
        *mut *mut Message,
        *const c_char,
        *const c_char,
        *const c_char,
        *const c_char,
    ) -> c_int,
    append: unsafe extern "C" fn(*mut Message, *const c_char, ...) -> c_int,
    call: unsafe extern "C" fn(
        *mut Bus,
        *mut Message,
        u64,
        *mut BusError,
        *mut *mut Message,
    ) -> c_int,
    enter: unsafe extern "C" fn(*mut Message, c_char, *const c_char) -> c_int,
    exit: unsafe extern "C" fn(*mut Message) -> c_int,
    read: unsafe extern "C" fn(*mut Message, *const c_char, ...) -> c_int,
    peek: unsafe extern "C" fn(*mut Message, *mut c_char, *mut *const c_char) -> c_int,
    skip: unsafe extern "C" fn(*mut Message, *const c_char) -> c_int,
    free_error: unsafe extern "C" fn(*mut BusError) -> *mut BusError,
}
impl Api {
    fn load() -> Result<Self, String> {
        // SAFETY: symbols below use the public systemd sd-bus C ABI; library outlives function pointers.
        unsafe {
            let lib = Library::new("libsystemd.so.0")
                .map_err(|e| format!("KDE 截图需要系统 libsystemd: {e}"))?;
            macro_rules! sym {
                ($s:literal) => {
                    *lib.get(concat!($s, "\0").as_bytes())
                        .map_err(|e| e.to_string())?
                };
            }
            Ok(Self {
                open: sym!("sd_bus_open_user"),
                close: sym!("sd_bus_unref"),
                unref: sym!("sd_bus_message_unref"),
                new_call: sym!("sd_bus_message_new_method_call"),
                append: sym!("sd_bus_message_append"),
                call: sym!("sd_bus_call"),
                enter: sym!("sd_bus_message_enter_container"),
                exit: sym!("sd_bus_message_exit_container"),
                read: sym!("sd_bus_message_read"),
                peek: sym!("sd_bus_message_peek_type"),
                skip: sym!("sd_bus_message_skip"),
                free_error: sym!("sd_bus_error_free"),
                _lib: lib,
            })
        }
    }
    fn check(result: c_int) -> Result<c_int, String> {
        if result < 0 {
            Err(format!(
                "KWin D-Bus: {}",
                std::io::Error::from_raw_os_error(-result)
            ))
        } else {
            Ok(result)
        }
    }
}
struct Session {
    api: Api,
    bus: *mut Bus,
    request: *mut Message,
    reply: *mut Message,
}
impl Drop for Session {
    fn drop(&mut self) {
        unsafe {
            (self.api.unref)(self.reply);
            (self.api.unref)(self.request);
            (self.api.close)(self.bus);
        }
    }
}
impl Session {
    fn new(
        destination: &CStr,
        path: &CStr,
        interface: &CStr,
        method: &CStr,
    ) -> Result<Self, String> {
        let api = Api::load()?;
        let mut session = Self {
            api,
            bus: ptr::null_mut(),
            request: ptr::null_mut(),
            reply: ptr::null_mut(),
        };
        unsafe {
            Api::check((session.api.open)(&mut session.bus))?;
            Api::check((session.api.new_call)(
                session.bus,
                &mut session.request,
                destination.as_ptr(),
                path.as_ptr(),
                interface.as_ptr(),
                method.as_ptr(),
            ))?;
        }
        Ok(session)
    }
    fn call(&mut self) -> Result<(), String> {
        let mut error = BusError {
            name: ptr::null(),
            message: ptr::null(),
            need_free: 0,
        };
        unsafe {
            let result = (self.api.call)(
                self.bus,
                self.request,
                5_000_000,
                &mut error,
                &mut self.reply,
            );
            let description = if !error.message.is_null() {
                Some(CStr::from_ptr(error.message).to_string_lossy().into_owned())
            } else {
                None
            };
            (self.api.free_error)(&mut error);
            if result < 0 {
                return Err(description.unwrap_or_else(|| {
                    format!("KWin D-Bus: {}", std::io::Error::from_raw_os_error(-result))
                }));
            }
        }
        Ok(())
    }
}
pub fn active_output() -> Result<String, String> {
    let mut session = Session::new(
        c"org.kde.KWin",
        c"/KWin",
        c"org.kde.KWin",
        c"activeOutputName",
    )?;
    session.call()?;
    unsafe {
        let mut name: *const c_char = ptr::null();
        Api::check((session.api.read)(session.reply, c"s".as_ptr(), &mut name))?;
        if name.is_null() {
            return Err("KWin 没有返回焦点输出".into());
        }
        Ok(CStr::from_ptr(name).to_string_lossy().into_owned())
    }
}
pub fn capture(name: &str) -> Result<ShmImage, String> {
    let mut session = Session::new(
        c"org.kde.KWin.ScreenShot2",
        c"/org/kde/KWin/ScreenShot2",
        c"org.kde.KWin.ScreenShot2",
        c"CaptureScreen",
    )?;
    let name = CString::new(name).map_err(|e| e.to_string())?;
    let (mut reader, writer) = std::io::pipe().map_err(|e| e.to_string())?;
    // KWin 写 pipe 与 D-Bus 回应分别完成；先读取元数据再按 stride 读取，限制异常结果分配。
    unsafe {
        Api::check((session.api.append)(
            session.request,
            c"sa{sv}h".as_ptr(),
            name.as_ptr(),
            1u32,
            c"native-resolution".as_ptr(),
            c"b".as_ptr(),
            1 as c_int,
            writer.as_raw_fd(),
        ))?;
    }
    session.call()?;
    drop(writer);
    let (mut width, mut height, mut stride, mut format) = (0u32, 0u32, 0u32, 0u32);
    let mut kind = String::new();
    unsafe {
        Api::check((session.api.enter)(
            session.reply,
            b'a' as c_char,
            c"{sv}".as_ptr(),
        ))?;
        while Api::check((session.api.enter)(
            session.reply,
            b'e' as c_char,
            c"sv".as_ptr(),
        ))? > 0
        {
            let mut key: *const c_char = ptr::null();
            Api::check((session.api.read)(session.reply, c"s".as_ptr(), &mut key))?;
            if key.is_null() {
                return Err("KWin 元数据键为空".into());
            }
            let key = CStr::from_ptr(key).to_string_lossy();
            let mut ty = 0 as c_char;
            let mut contents: *const c_char = ptr::null();
            Api::check((session.api.peek)(session.reply, &mut ty, &mut contents))?;
            if contents.is_null() {
                return Err("KWin 元数据类型为空".into());
            }
            Api::check((session.api.enter)(session.reply, b'v' as c_char, contents))?;
            match key.as_ref() {
                "width" | "height" | "stride" | "format" => {
                    let mut value = 0u32;
                    Api::check((session.api.read)(session.reply, c"u".as_ptr(), &mut value))?;
                    match key.as_ref() {
                        "width" => width = value,
                        "height" => height = value,
                        "stride" => stride = value,
                        _ => format = value,
                    }
                }
                "type" => {
                    let mut value: *const c_char = ptr::null();
                    Api::check((session.api.read)(session.reply, c"s".as_ptr(), &mut value))?;
                    if !value.is_null() {
                        kind = CStr::from_ptr(value).to_string_lossy().into_owned()
                    }
                }
                _ => {
                    Api::check((session.api.skip)(session.reply, contents))?;
                }
            }
            Api::check((session.api.exit)(session.reply))?;
            Api::check((session.api.exit)(session.reply))?;
        }
    }
    if kind != "raw"
        || format != 16
        || width == 0
        || height == 0
        || u64::from(stride) < u64::from(width) * 4
        || u64::from(stride) * u64::from(height) > 512 * 1024 * 1024
    {
        return Err(format!(
            "KWin 截图格式无效: {kind}, format={format}, {width}x{height}, stride={stride}"
        ));
    }
    // QImage::Format_RGBX8888：沿用 KWin 的行跨度直接读进 memfd，再原地交换通道。
    let mut image = ShmImage::new(width, height, stride)?;
    reader
        .read_exact(image.bytes_mut())
        .map_err(|e| format!("KWin 像素不完整: {e}"))?;
    image.normalize(ChannelOrder::Rgba, true, false);
    Ok(image)
}
