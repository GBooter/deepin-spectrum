//! 锁屏状态跟随（`dbus` feature）。
//!
//! 优先读 `org.freedesktop.login1.Session.LockedHint`；取不到时退回
//! `org.freedesktop.ScreenSaver.GetActive`。500ms 轮询一次，开销远低于
//! 音频/FFT；状态通过 [`LockMonitor::is_locked`] 提供给 daemon，主循环只在
//! 500ms 的定时检查里读取。
//!
//! 锁屏时 daemon 会清屏并停止 X11 OR 窗口的 `raise`，避免频谱盖在锁屏层上。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use zbus::Connection;
use zbus::proxy::Proxy;

/// 锁屏状态监视器。
pub struct LockMonitor {
    locked: Arc<AtomicBool>,
    alive: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl LockMonitor {
    /// 启动监视线程；D-Bus 不可用时静默退化为「永不锁屏」。
    #[must_use]
    pub fn start() -> Self {
        let locked = Arc::new(AtomicBool::new(false));
        let alive = Arc::new(AtomicBool::new(true));
        let locked_thread = Arc::clone(&locked);
        let alive_thread = Arc::clone(&alive);
        let join = std::thread::Builder::new()
            .name("deepin-spectrum-lock".to_owned())
            .spawn(move || {
                zbus::block_on(async {
                    let Ok(conn) = Connection::session().await else {
                        return;
                    };
                    let logind = Proxy::new(
                        &conn,
                        "org.freedesktop.login1",
                        "/org/freedesktop/login1/session/auto",
                        "org.freedesktop.login1.Session",
                    )
                    .await
                    .ok();
                    let screensaver = Proxy::new(
                        &conn,
                        "org.freedesktop.ScreenSaver",
                        "/org/freedesktop/ScreenSaver",
                        "org.freedesktop.ScreenSaver",
                    )
                    .await
                    .ok();

                    while alive_thread.load(Ordering::SeqCst) {
                        let mut value = None;
                        if let Some(proxy) = &logind
                            && let Ok(v) = proxy.get_property::<bool>("LockedHint").await
                        {
                            value = Some(v);
                        }
                        if value.is_none()
                            && let Some(proxy) = &screensaver
                            && let Ok(reply) = proxy.call_method("GetActive", &()).await
                            && let Ok(v) = reply.body().deserialize::<bool>()
                        {
                            value = Some(v);
                        }
                        if let Some(v) = value {
                            locked_thread.store(v, Ordering::SeqCst);
                        }
                        std::thread::sleep(Duration::from_millis(500));
                    }
                });
            })
            .ok();
        Self {
            locked,
            alive,
            join,
        }
    }

    /// 当前是否处于锁屏/屏保状态。
    #[must_use]
    pub fn is_locked(&self) -> bool {
        self.locked.load(Ordering::SeqCst)
    }
}

impl Drop for LockMonitor {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::SeqCst);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}
