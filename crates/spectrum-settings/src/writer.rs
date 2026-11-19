//! 把高频改动合并成一次落盘。
//!
//! ## 为什么需要它
//!
//! 设置界面的滑块在拖动时会连续触发 `changed`（每秒几十次）。
//! 如果每次都写文件 + 原子改名，一秒能写几十次——既浪费，也把 daemon 的
//! 热重载轮询搅得鸡飞狗跳（每次写都触发一次内容变化）。
//!
//! 这里的做法是**合并**：窗口期内只保留**最后一次**改动，落盘一次。
//! 窗口默认 300ms（比 daemon 的 250ms 轮询略长，保证一次拖动只写一次、
//! daemon 只应用一次）。
//!
//! ## 与 daemon 的关系
//!
//! 写盘线程只管写文件；daemon 通过内容哈希轮询发现变化并热应用。
//! 两边**不直接通信**，因此设置界面与 daemon 之间没有时序耦合——
//! 界面关了、daemon 还在跑，两者都照常工作。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use spectrum_core::config::Config;

use crate::schema::Extra;
use crate::schema::encode_with_extras;
use crate::store::{content_hash, hash_bytes, write_atomic_text};

/// 一次落盘的结果回调（成功 / 失败）。
pub type WriteCallback = Arc<dyn Fn(&Result<(), String>) + Send + Sync>;

struct Shared {
    /// 待写配置（`None` = 没有待写）。
    pending: Mutex<Option<Config>>,
    /// 有新配置时唤醒写线程。
    signal: Condvar,
    /// 线程退出标志。
    stop: AtomicBool,
    /// 正在写（`flush` 用它判断"是否写完了"）。
    writing: AtomicBool,
    /// 打开界面时的文件哈希；每次成功写入后更新。外部修改会导致不匹配。
    expected_hash: Mutex<Option<u64>>,
}

/// 合并写盘器。
pub struct ConfigWriter {
    shared: Arc<Shared>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl ConfigWriter {
    /// 启动写盘线程（不做外部修改检测）。
    ///
    /// `extras` 是本版本不认识的键（回写时原样保留）。
    #[must_use]
    pub fn spawn(
        path: PathBuf,
        extras: Vec<Extra>,
        delay: Duration,
        on_written: Option<WriteCallback>,
    ) -> Self {
        Self::spawn_checked(path, extras, delay, None, on_written)
    }

    /// 启动写盘线程，并在每次写入前检查文件是否被外部修改。
    ///
    /// `expected_hash` 应传入界面**打开时**的文件内容哈希；写盘成功后自动更新。
    /// 不匹配时拒绝写入并通过 `on_written` 报告错误，避免把用户手改的配置整份覆盖。
    #[must_use]
    pub fn spawn_checked(
        path: PathBuf,
        extras: Vec<Extra>,
        delay: Duration,
        expected_hash: Option<u64>,
        on_written: Option<WriteCallback>,
    ) -> Self {
        let shared = Arc::new(Shared {
            pending: Mutex::new(None),
            signal: Condvar::new(),
            stop: AtomicBool::new(false),
            writing: AtomicBool::new(false),
            expected_hash: Mutex::new(expected_hash),
        });
        let thread_shared = Arc::clone(&shared);
        let handle = std::thread::Builder::new()
            .name("spectrum-settings-writer".to_owned())
            .spawn(move || {
                writer_loop(thread_shared, path, extras, delay, on_written);
            })
            .ok();
        Self { shared, handle }
    }

    /// 提交一次改动（同一次提交会覆盖上一次未落盘的改动）。
    ///
    /// # Panics
    ///
    /// 内部互斥锁被 poison（只有另一个线程在持锁时 panic 才会发生）时 panic。
    pub fn submit(&self, cfg: Config) {
        {
            let mut guard = self.shared.pending.lock().unwrap();
            *guard = Some(cfg);
        }
        self.shared.signal.notify_one();
    }

    /// 等待当前待写配置落盘完成（超时 2s）。
    ///
    /// # Panics
    ///
    /// 内部互斥锁被 poison 时 panic。
    pub fn flush(&self) {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            {
                let guard = self.shared.pending.lock().unwrap();
                if guard.is_none() && !self.shared.writing.load(Ordering::SeqCst) {
                    return;
                }
            }
            if Instant::now() >= deadline {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// 停线程（先落盘再退出）。
    pub fn stop(mut self) {
        self.flush();
        self.shared.stop.store(true, Ordering::SeqCst);
        self.shared.signal.notify_all();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for ConfigWriter {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        self.shared.signal.notify_all();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// 写盘线程主体。
///
/// 参数按值传入是**必需的**：这是线程入口，捕获的数据必须 `'static + Send`，
// 不能借用调用方的栈上数据。
#[allow(clippy::needless_pass_by_value)]
fn writer_loop(
    shared: Arc<Shared>,
    path: PathBuf,
    extras: Vec<Extra>,
    delay: Duration,
    on_written: Option<WriteCallback>,
) {
    loop {
        // 1) 等一份待写配置
        let first = {
            let mut guard = shared.pending.lock().unwrap();
            loop {
                if let Some(cfg) = guard.take() {
                    break Some(cfg);
                }
                if shared.stop.load(Ordering::SeqCst) {
                    break None;
                }
                let (next, _timeout) = shared
                    .signal
                    .wait_timeout(guard, Duration::from_millis(100))
                    .unwrap();
                guard = next;
            }
        };
        let Some(mut cfg) = first else {
            break;
        };

        // 2) 合并窗口：这段时间里的改动只保留最后一个
        std::thread::sleep(delay);
        {
            let mut guard = shared.pending.lock().unwrap();
            if let Some(newer) = guard.take() {
                cfg = newer;
            }
        }

        // 3) 落盘（原子）；写前检测外部修改，避免整份覆盖用户手改的内容。
        let text = encode_with_extras(&cfg, &extras);
        let expected = *shared
            .expected_hash
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        shared.writing.store(true, Ordering::SeqCst);
        let result = if content_hash(&path) == expected {
            write_atomic_text(&path, &text)
                .map(|()| {
                    *shared
                        .expected_hash
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) =
                        Some(hash_bytes(text.as_bytes()));
                })
                .map_err(|e| e.to_string())
        } else {
            Err("配置文件已被外部修改，已停止写入；请重新打开设置界面".to_owned())
        };
        shared.writing.store(false, Ordering::SeqCst);

        if let Some(cb) = &on_written {
            cb(&result);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ConfigWriter;
    use crate::store::content_hash;
    use spectrum_core::config::Config;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let base = std::env::temp_dir().join(format!(
                "spectrum-writer-{tag}-{}-{}",
                std::process::id(),
                Instant::now().elapsed().as_nanos()
            ));
            std::fs::create_dir_all(&base).expect("建临时目录失败");
            Self(base)
        }
        fn file(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn rapid_submissions_are_coalesced_to_the_last_value() {
        let dir = TempDir::new("coalesce");
        let path = dir.file("config.toml");
        let write_count = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&write_count);
        let writer = ConfigWriter::spawn(
            path.clone(),
            Vec::new(),
            Duration::from_millis(60),
            Some(Arc::new(move |_| {
                counter.fetch_add(1, Ordering::SeqCst);
            })),
        );

        for i in 16..96 {
            let mut cfg = Config::default();
            cfg.visual.bands = i;
            writer.submit(cfg);
        }
        writer.flush();
        writer.stop();

        let text = std::fs::read_to_string(&path).expect("读失败");
        assert!(
            text.contains("bands = 95"),
            "最后一次改动应当落盘：\n{text}"
        );
        assert!(
            write_count.load(Ordering::SeqCst) <= 4,
            "16 次快速改动应合并成个位数次落盘，实际 {} 次",
            write_count.load(Ordering::SeqCst)
        );
    }

    #[test]
    fn write_error_is_reported_to_callback() {
        let dir = TempDir::new("error");
        // 把配置文件放在**普通文件**下面 → 建目录失败 → 写失败
        let blocker = dir.file("blocker");
        std::fs::write(&blocker, b"x").expect("写失败");
        let path = blocker.join("config.toml");
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let writer = ConfigWriter::spawn(
            path,
            Vec::new(),
            Duration::from_millis(10),
            Some(Arc::new(move |result| {
                if let Err(e) = result {
                    sink.lock().unwrap().push(e.clone());
                }
            })),
        );
        writer.submit(Config::default());
        writer.flush();
        writer.stop();
        let errors = seen.lock().unwrap();
        assert_eq!(errors.len(), 1, "应当报一次写失败：{errors:?}");
    }

    #[test]
    fn external_change_blocks_write() {
        let dir = TempDir::new("conflict");
        let path = dir.file("config.toml");
        std::fs::write(&path, "version = 2\n[visual]\nopacity = 0.5\n").expect("写失败");
        let expected = content_hash(&path);

        let errors: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&errors);
        let writer = ConfigWriter::spawn_checked(
            path.clone(),
            Vec::new(),
            Duration::from_millis(10),
            expected,
            Some(Arc::new(move |result| {
                if let Err(e) = result {
                    sink.lock().unwrap().push(e.clone());
                }
            })),
        );

        // 模拟用户在界面打开后手工编辑配置文件
        std::fs::write(&path, "version = 2\n[visual]\nopacity = 0.9\n").expect("写失败");
        let mut cfg = Config::default();
        cfg.visual.opacity = 0.3;
        writer.submit(cfg);
        writer.flush();
        writer.stop();

        assert_eq!(errors.lock().unwrap().len(), 1, "外部修改必须拒绝写入");
        let text = std::fs::read_to_string(&path).expect("读失败");
        assert!(
            text.contains("opacity = 0.9"),
            "用户手改的内容不能被覆盖：\n{text}"
        );
    }

    #[test]
    fn unknown_keys_are_preserved_on_write() {
        let dir = TempDir::new("extras");
        let path = dir.file("config.toml");
        std::fs::write(
            &path,
            "version = 1\n[visual]\nopacity = 0.5\nfuture_key = 7\n",
        )
        .expect("写失败");
        let store = crate::store::ConfigStore::open(&path);
        assert_eq!(store.extras().len(), 1, "未来键应被收进 extras");
        let mut cfg = store.config().clone();
        cfg.visual.opacity = 0.8;
        store.write(&cfg).expect("写失败");
        let text = std::fs::read_to_string(&path).expect("读失败");
        assert!(text.contains("future_key = 7"), "未来键被抹掉了：\n{text}");
        assert!(text.contains("opacity = 0.8"), "新值没写进去：\n{text}");
    }
}
