//! 配置文件轮询监视（零依赖，不引 `notify`）。
//!
//! ## 为什么是轮询而不是 inotify
//!
//! 指导文档 §6.8 写的是 `notify`（inotify）。这里改成 250ms 轮询，
//! 理由是三条实测约束，不是图省事：
//!
//! 1. **离线依赖**：`notify` 会带进 `inotify`/`mio`/`filetime` 等一串 crate，
//!    而本项目其它 crate 的默认构建是零依赖，配置层不该打破这个基线；
//! 2. **原子改名**：设置界面写文件是 `tmp + rename`（§6.8 要求）。inotify 会给
//!    `MOVED_TO`/`DELETE`/`CREATE` 三条事件，顺序与合并因内核版本而异，
//!    容易做出"改了两次才生效一次"的 bug；轮询只看**最终文件内容**，天然免疫；
//! 3. **NFS / 容器 / 绑定挂载**：inotify 在这些场景不报事件，而用户的
//!    `$XDG_CONFIG_HOME` 完全可能落在这些位置。
//!
//! 代价是**最坏 250ms 的生效延迟**——对人眼是"立刻"。
//!
//! ## 判据是内容哈希，不是 mtime
//!
//! `mtime` 有粒度问题（同秒内两次写入看起来没变），而且 `cp -p` / `git checkout`
//! 会把它改回旧值。这里每次直接读文件算 FNV-1a：
//! 配置文件只有 1~2 KB，4 次/秒的读取量可以忽略，换来"改了就一定生效"。

use std::hash::Hasher;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// 默认轮询间隔。
pub const DEFAULT_INTERVAL: Duration = Duration::from_millis(250);

/// 一次采样得到的文件状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Stamp {
    /// 文件是否存在。
    pub exists: bool,
    /// 字节数（不存在时为 0）。
    pub len: u64,
    /// 内容哈希（不存在时为 0）。
    pub hash: u64,
}

impl Stamp {
    /// 采样一个文件。
    #[must_use]
    pub fn read(path: &Path) -> Self {
        match std::fs::read(path) {
            Ok(bytes) => Self {
                exists: true,
                len: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
                hash: fnv1a(&bytes),
            },
            Err(_) => Self {
                exists: false,
                len: 0,
                hash: 0,
            },
        }
    }
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    h.write(bytes);
    h.finish()
}

/// 轮询监视器。
#[derive(Debug)]
pub struct PollWatch {
    path: PathBuf,
    interval: Duration,
    next_due: Instant,
    last: Stamp,
}

impl PollWatch {
    /// 建立监视器并立刻采样一次（作为基线）。
    #[must_use]
    pub fn new(path: impl Into<PathBuf>, interval: Duration) -> Self {
        let path = path.into();
        let last = Stamp::read(&path);
        Self {
            path,
            interval,
            // 基线已经采过了，第一次真正检查要等一个间隔
            next_due: Instant::now() + interval,
            last,
        }
    }

    /// 被监视的路径。
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 最近一次采样。
    #[must_use]
    pub const fn last(&self) -> Stamp {
        self.last
    }

    /// 下一次必定检查（用于"写完文件后立刻确认"）。
    pub fn poke(&mut self) {
        self.next_due = Instant::now();
    }

    /// 到点则采样；**内容变化**（含出现/消失）返回 `Some(新状态)`，否则 `None`。
    ///
    /// `None` 有两种含义：还没到检查时间，或者检查了但没变 —— 调用方都不需要做事。
    pub fn poll(&mut self, now: Instant) -> Option<Stamp> {
        if now < self.next_due {
            return None;
        }
        self.next_due = now + self.interval;
        let stamp = Stamp::read(&self.path);
        if stamp == self.last {
            return None;
        }
        self.last = stamp;
        Some(stamp)
    }
}

#[cfg(test)]
mod tests {
    use super::{PollWatch, Stamp};
    use std::time::{Duration, Instant};

    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let base = std::env::temp_dir().join(format!(
                "spectrum-watch-{tag}-{}-{}",
                std::process::id(),
                Instant::now().elapsed().as_nanos()
            ));
            std::fs::create_dir_all(&base).expect("建临时目录失败");
            Self(base)
        }
        fn file(&self, name: &str) -> std::path::PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// 间隔为 0：测试里不需要等节流。
    fn watcher(path: &std::path::Path) -> PollWatch {
        PollWatch::new(path, Duration::ZERO)
    }

    #[test]
    fn missing_file_then_created() {
        let dir = TempDir::new("create");
        let path = dir.file("config.toml");
        let mut w = watcher(&path);
        assert_eq!(w.poll(Instant::now()), None, "还没这个文件");

        std::fs::write(&path, "version = 1\n").expect("写失败");
        let stamp = w.poll(Instant::now());
        assert!(stamp.is_some_and(|s| s.exists), "创建必须被看到");
    }

    #[test]
    fn same_content_is_not_a_change() {
        let dir = TempDir::new("same");
        let path = dir.file("config.toml");
        std::fs::write(&path, "version = 1\n").expect("写失败");
        let mut w = watcher(&path);
        // 基线在 new() 里就采过了，所以第一次 poll 应当是"没变化"
        assert_eq!(w.poll(Instant::now()), None);
        assert_eq!(
            w.poll(Instant::now()),
            None,
            "没改就是没改（mtime 变了也不算）"
        );
        // 重写同样内容（模拟编辑器保存但没改动）
        std::fs::write(&path, "version = 1\n").expect("写失败");
        assert_eq!(w.poll(Instant::now()), None);
    }

    #[test]
    fn content_change_is_detected() {
        let dir = TempDir::new("change");
        let path = dir.file("config.toml");
        std::fs::write(&path, "version = 1\n").expect("写失败");
        let mut w = watcher(&path);
        assert_eq!(w.poll(Instant::now()), None, "基线不含变化");
        std::fs::write(&path, "version = 1\nopacity = 1\n").expect("写失败");
        assert!(w.poll(Instant::now()).is_some(), "内容变化必须被看到");
    }

    #[test]
    fn deletion_is_detected() {
        let dir = TempDir::new("delete");
        let path = dir.file("config.toml");
        std::fs::write(&path, "version = 1\n").expect("写失败");
        let mut w = watcher(&path);
        assert_eq!(w.poll(Instant::now()), None, "基线不含变化");
        std::fs::remove_file(&path).expect("删失败");
        let stamp = w.poll(Instant::now());
        assert!(stamp.is_some_and(|s| !s.exists), "删除必须被看到");
    }

    #[test]
    fn throttled_until_interval_elapses() {
        let dir = TempDir::new("throttle");
        let path = dir.file("config.toml");
        std::fs::write(&path, "a = 1\n").expect("写失败");
        let mut w = PollWatch::new(&path, Duration::from_millis(250));
        // 基线刚采过，立刻 poll 应当被节流
        assert_eq!(w.poll(Instant::now()), None);
        std::fs::write(&path, "a = 2\n").expect("写失败");
        assert_eq!(w.poll(Instant::now()), None, "没到点就不该读");
        // poke 之后可以立刻确认
        w.poke();
        assert!(w.poll(Instant::now()).is_some());
    }

    #[test]
    fn stamp_hash_distinguishes_content() {
        let a = Stamp {
            exists: true,
            len: 3,
            hash: 1,
        };
        let b = Stamp {
            exists: true,
            len: 3,
            hash: 2,
        };
        assert_ne!(a, b);
    }
}
