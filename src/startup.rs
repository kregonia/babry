use std::time::Instant;

/// glibc 会保留已释放的大块堆页；仅在后台会话结束后归还空闲页，不触碰活跃像素或字体。
/// musl 和其他平台由各自分配器管理，不假设存在 glibc API。
pub fn reclaim_idle_memory() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        unsafe extern "C" {
            fn malloc_trim(pad: usize) -> i32;
        }
        // SAFETY: glibc malloc_trim 没有指针参数，允许与其他线程的分配并行调用。
        unsafe {
            malloc_trim(0);
        }
    }
}

/// 按会话记录启动阶段；AfterRendering 表示 CPU/GPU 场景提交前，不能视为屏幕呈现时间。
#[derive(Clone)]
pub struct StartupTrace {
    start: Instant,
    enabled: bool,
    mode: &'static str,
}

impl StartupTrace {
    pub fn new(mode: &'static str) -> Self {
        Self {
            start: Instant::now(),
            enabled: std::env::var_os("BABRY_TRACE_STARTUP").is_some(),
            mode,
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn mark(&self, stage: &str) {
        if self.enabled {
            eprintln!(
                "{}",
                serde_json::json!({
                    "event": "babry-startup",
                    "mode": self.mode,
                    "stage": stage,
                    "elapsed_ms": self.start.elapsed().as_secs_f64() * 1000.0,
                    "pid": std::process::id(),
                })
            );
        }
    }
}
