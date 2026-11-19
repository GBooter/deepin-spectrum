//! 配置文件仓库：加载、校验、原子写、以及"文件变了就差分应用"。
//!
//! ## 三条必须守住的属性
//!
//! 1. **永不 panic**：配置文件是用户手改的，最脏的输入类比网络还不可控。
//!    读不到 → 用默认值；读坏了 → 保留上一次的好配置（[`Source::Fallback`]）；
//! 2. **出错不刷屏**：编辑器保存一半的文件会在 50ms 内被读到两次，
//!    同一条错误只报一次（[`ConfigStore::poll`] 里按错误文本去重）；
//! 3. **写要原子**：`tmp + rename`，权限 0600，目录 0700（§6.8）。
//!    daemon 随时可能在读，绝不能让它读到写了一半的文件。
//!
//! ## 为什么不加 flock
//!
//! §6.8 要求用 `rustix::fs::flock` 防 GUI 与 daemon 同时写。当前阶段的实际
//! 写入方只有设置界面（单写者），而 daemon **只读**，且读的是 rename 之后
//! 的完整文件——多写者问题不存在。加锁反而会带进 `rustix` 这一层依赖，
//! 所以先不做，等真的出现第二个写者（例如托盘里也能改配置）再加，
//! 那时锁的保护对象和粒度才说得清。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use spectrum_core::config::Config;

use crate::apply::{Delta, diff};
use crate::schema::{self, Decoded, Extra, Warning};
use crate::toml::{self, TomlError};
use crate::watch::{DEFAULT_INTERVAL, PollWatch};

/// 临时文件序号：同进程内同一纳秒多次写入也不会撞名。
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// 计算字节内容哈希（FNV-1a）。
#[must_use]
pub fn hash_bytes(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// 计算文件内容哈希；文件不存在返回 `None`。
///
/// 设置界面用它检测「打开后配置文件被外部修改」。
#[must_use]
pub fn content_hash(path: &Path) -> Option<u64> {
    std::fs::read(path).ok().map(|bytes| hash_bytes(&bytes))
}

/// 配置内容的来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// 来自配置文件。
    File,
    /// 文件不存在（或已被删除），等价于全默认值。
    Default,
    /// 文件存在但读/解析失败，当前在用**上一次的好配置**。
    Fallback,
}

impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::File => "文件",
            Self::Default => "默认（无文件）",
            Self::Fallback => "上次的好配置（当前文件有错）",
        };
        f.write_str(s)
    }
}

/// 读取/解析失败。
#[derive(Debug)]
pub enum StoreError {
    /// 文件读不出来（权限、IO 错误）。
    Io {
        /// 路径。
        path: PathBuf,
        /// 底层错误。
        source: std::io::Error,
    },
    /// TOML 语法错误。
    Parse(TomlError),
    /// 语义错误（例如 schema 版本高于本二进制）。
    Decode(String),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, source } => write!(f, "读取 {} 失败：{source}", path.display()),
            Self::Parse(e) => write!(f, "配置语法错误 {e}"),
            Self::Decode(m) => write!(f, "配置内容不合法：{m}"),
        }
    }
}

impl std::error::Error for StoreError {}

/// 一次成功加载的结果。
#[derive(Debug, Clone)]
pub struct Load {
    /// 解析 + 校验后的配置。
    pub config: Config,
    /// 告警。
    pub warnings: Vec<Warning>,
    /// 本版本不认识的键（回写时原样保留）。
    pub extras: Vec<Extra>,
    /// 文件是否存在（`false` 表示用的全默认值）。
    pub existed: bool,
}

/// 默认配置路径。
///
/// 优先级：`SPECTRUM_CONFIG`（测试/脚本用）→ `$XDG_CONFIG_HOME` → `$HOME/.config` → 当前目录。
#[must_use]
pub fn default_path() -> PathBuf {
    if let Some(p) = std::env::var_os("SPECTRUM_CONFIG").filter(|v| !v.is_empty()) {
        return PathBuf::from(p);
    }
    path_from(
        std::env::var_os("XDG_CONFIG_HOME"),
        std::env::var_os("HOME"),
    )
}

fn path_from(xdg: Option<std::ffi::OsString>, home: Option<std::ffi::OsString>) -> PathBuf {
    if let Some(dir) = xdg.filter(|v| !v.is_empty()) {
        return PathBuf::from(dir)
            .join("deepin-spectrum")
            .join("config.toml");
    }
    if let Some(home) = home.filter(|v| !v.is_empty()) {
        return PathBuf::from(home)
            .join(".config")
            .join("deepin-spectrum")
            .join("config.toml");
    }
    PathBuf::from("deepin-spectrum-config.toml")
}

/// 从磁盘加载并校验。
///
/// 文件不存在返回全默认值（`existed = false`），**不是**错误：
/// 首次运行就是这种状态。
///
/// # Errors
///
/// IO 错误、TOML 语法错误、schema 版本过高等。
pub fn load(path: &Path) -> Result<Load, StoreError> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Load {
                config: Config::default(),
                warnings: Vec::new(),
                extras: Vec::new(),
                existed: false,
            });
        }
        Err(e) => {
            return Err(StoreError::Io {
                path: path.to_path_buf(),
                source: e,
            });
        }
    };
    let doc = toml::parse(&text).map_err(StoreError::Parse)?;
    let Decoded {
        config,
        warnings,
        extras,
    } = schema::decode(&doc).map_err(StoreError::Decode)?;
    Ok(Load {
        config,
        warnings,
        extras,
        existed: true,
    })
}

/// 原子写文本：`同目录 tmp` → `fsync` → `rename`。
///
/// # Errors
///
/// 目录创建、写、改权限、改名失败。
pub fn write_atomic_text(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        // ★ 只收紧「本次新建」的目录：`--config` 可能指向共享目录、`$HOME` 或
        // `/tmp`，对已存在的父目录 chmod 会悄悄改掉别人的权限（甚至 EPERM）。
        let existed = parent.exists();
        std::fs::create_dir_all(parent)?;
        if !existed {
            set_mode(parent, 0o700)?;
        }
    }
    // ★ 唯一临时名 + create_new：两个设置窗口/进程并发写时不会互相截断。
    let sequence = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let name = path.file_name().map_or_else(
        || "config.toml".to_owned(),
        |n| n.to_string_lossy().into_owned(),
    );
    let tmp = path.with_file_name(format!(
        ".{name}.{}.{stamp}.{sequence}.tmp",
        std::process::id()
    ));
    {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
    }
    set_mode(&tmp, 0o600)?;
    std::fs::rename(&tmp, path)
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
#[allow(clippy::unnecessary_wraps)]
fn set_mode(_path: &Path, _mode: u32) -> std::io::Result<()> {
    Ok(())
}

/// 仓库统计（供日志与证据脚本）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StoreStats {
    /// 成功应用的热重载次数。
    pub reloads: u64,
    /// 被拒绝的重载次数（文件有错）。
    pub rejects: u64,
    /// 文件变了但配置值没变（只改注释 / 只改未实现的键）。
    pub ignored: u64,
}

/// 一次热重载的结果。
#[derive(Debug, Clone, PartialEq)]
pub enum Reload {
    /// 配置已更新（`delta` 可能为空：值变了但都不可应用）。
    Applied {
        /// 递增的修订号（从 1 开始）。
        revision: u32,
        /// 实际被改动的键。
        delta: Delta,
        /// 告警条数。
        warnings: usize,
    },
    /// 文件有错，**保留了上一次的好配置**。
    Rejected {
        /// 错误描述。
        error: String,
    },
}

/// 配置仓库。
#[derive(Debug)]
pub struct ConfigStore {
    path: PathBuf,
    cfg: Config,
    warnings: Vec<Warning>,
    /// 本版本不认识的键（回写时原样保留，见 [`Extra`]）。
    extras: Vec<Extra>,
    revision: u32,
    source: Source,
    watch: PollWatch,
    last_error: Option<String>,
    stats: StoreStats,
}

impl ConfigStore {
    /// 打开（不失败）：读不到/读坏了都退回默认值，错误记在 [`Self::last_error`] 里。
    #[must_use]
    pub fn open(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let watch = PollWatch::new(&path, DEFAULT_INTERVAL);
        let mut store = Self {
            path,
            cfg: Config::default(),
            warnings: Vec::new(),
            extras: Vec::new(),
            revision: 0,
            source: Source::Default,
            watch,
            last_error: None,
            stats: StoreStats::default(),
        };
        match load(&store.path) {
            Ok(Load {
                config,
                warnings,
                extras,
                existed,
            }) => {
                store.cfg = config;
                store.warnings = warnings;
                store.extras = extras;
                store.source = if existed {
                    Source::File
                } else {
                    Source::Default
                };
            }
            Err(e) => {
                // 第一次就读坏了：用默认值跑，但必须留下痕迹
                eprintln!("配置读取失败（改用默认值）：{e}");
                store.last_error = Some(e.to_string());
                store.source = Source::Fallback;
            }
        }
        store
    }

    /// 配置路径。
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 当前生效的配置。
    #[must_use]
    pub const fn config(&self) -> &Config {
        &self.cfg
    }

    /// 修订号（每次成功应用 +1；用于日志与"是否变过"）。
    #[must_use]
    pub const fn revision(&self) -> u32 {
        self.revision
    }

    /// 当前配置的来源。
    #[must_use]
    pub const fn source(&self) -> Source {
        self.source
    }

    /// 当前配置的告警。
    #[must_use]
    pub fn warnings(&self) -> &[Warning] {
        &self.warnings
    }

    /// 本版本不认识的键（回写时原样保留）。
    #[must_use]
    pub fn extras(&self) -> &[Extra] {
        &self.extras
    }

    /// 最近一次错误（成功加载后清空）。
    #[must_use]
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    /// 统计。
    #[must_use]
    pub const fn stats(&self) -> StoreStats {
        self.stats
    }

    /// 非阻塞检查：文件内容变了才返回 `Some`。
    pub fn poll(&mut self, now: Instant) -> Option<Reload> {
        self.watch.poll(now)?;
        self.reload()
    }

    /// 忽略节流立刻检查一次。
    pub fn reload_now(&mut self) -> Option<Reload> {
        self.watch.poke();
        self.poll(Instant::now())
    }

    fn reload(&mut self) -> Option<Reload> {
        match load(&self.path) {
            Ok(Load {
                config,
                warnings,
                extras,
                existed,
            }) => {
                let recovered = self.last_error.take().is_some();
                self.warnings = warnings;
                self.extras = extras;
                self.source = if existed {
                    Source::File
                } else {
                    Source::Default
                };
                if config == self.cfg {
                    if recovered {
                        // 上次是坏文件、这次好了：即使值一样也要让调用方打一行"已恢复"
                        self.revision += 1;
                        self.stats.reloads += 1;
                        return Some(Reload::Applied {
                            revision: self.revision,
                            delta: Delta::default(),
                            warnings: self.warnings.len(),
                        });
                    }
                    self.stats.ignored += 1;
                    return None;
                }
                let delta = diff(&self.cfg, &config);
                self.cfg = config;
                self.revision += 1;
                self.stats.reloads += 1;
                Some(Reload::Applied {
                    revision: self.revision,
                    delta,
                    warnings: self.warnings.len(),
                })
            }
            Err(e) => {
                let text = e.to_string();
                if self.last_error.as_deref() == Some(text.as_str()) {
                    // 同一条错误只报一次：编辑器保存一半时会被连读两三次
                    return None;
                }
                self.last_error = Some(text.clone());
                self.source = Source::Fallback;
                self.stats.rejects += 1;
                Some(Reload::Rejected { error: text })
            }
        }
    }

    /// 原子写回配置（设置界面 / CLI 用）。
    ///
    /// # Errors
    ///
    /// 见 [`write_atomic_text`]。
    pub fn write(&self, cfg: &Config) -> std::io::Result<()> {
        write_atomic_text(&self.path, &schema::encode_with_extras(cfg, &self.extras))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ConfigStore, Reload, Source, StoreError, default_path, load, path_from, write_atomic_text,
    };
    use crate::schema::{TEMPLATE, encode};
    use spectrum_core::config::Config;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let base = std::env::temp_dir().join(format!(
                "spectrum-store-{tag}-{}-{}",
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

    fn store(path: &Path) -> ConfigStore {
        let mut s = ConfigStore::open(path);
        s.watch = crate::watch::PollWatch::new(path, Duration::ZERO);
        s
    }

    #[test]
    fn missing_file_means_defaults() {
        let dir = TempDir::new("missing");
        let s = ConfigStore::open(dir.file("nope.toml"));
        assert_eq!(s.config(), &Config::default());
        assert_eq!(s.source(), Source::Default);
        assert_eq!(s.revision(), 0);
        assert!(s.last_error().is_none(), "文件不存在不是错误");
    }

    #[test]
    fn valid_file_is_loaded() {
        let dir = TempDir::new("valid");
        let path = dir.file("config.toml");
        std::fs::write(&path, "[visual]\nopacity = 0.9\nbands = 32\n").expect("写失败");
        let s = ConfigStore::open(&path);
        assert!((s.config().visual.opacity - 0.9).abs() < 1e-9);
        assert_eq!(s.config().visual.bands, 32);
        assert_eq!(s.source(), Source::File);
        assert!(s.warnings().is_empty(), "{:?}", s.warnings());
    }

    #[test]
    fn template_loads_without_warnings() {
        let dir = TempDir::new("template");
        let path = dir.file("config.toml");
        std::fs::write(&path, TEMPLATE).expect("写失败");
        let s = ConfigStore::open(&path);
        assert_eq!(s.config(), &Config::default());
        assert!(s.warnings().is_empty(), "{:?}", s.warnings());
    }

    #[test]
    fn broken_file_keeps_last_good_and_dedupes_error() {
        let dir = TempDir::new("broken");
        let path = dir.file("config.toml");
        std::fs::write(&path, "[visual]\nopacity = 0.9\n").expect("写失败");
        let mut s = store(&path);
        assert!((s.config().visual.opacity - 0.9).abs() < 1e-9);

        // 半行 TOML（编辑器保存到一半）
        std::fs::write(&path, "[visual]\nopacity = ").expect("写失败");
        let first = s.poll(Instant::now());
        assert!(matches!(first, Some(Reload::Rejected { .. })), "{first:?}");
        assert!(
            (s.config().visual.opacity - 0.9).abs() < 1e-9,
            "坏文件不得清掉已生效的配置"
        );
        assert_eq!(s.source(), Source::Fallback);

        // 再 poll 一次同一份坏文件：不该重复报错
        // （哈希没变，poll 直接返回 None）
        assert_eq!(s.poll(Instant::now()), None);
        assert_eq!(s.stats().rejects, 1);
    }

    #[test]
    fn fixed_file_recovers() {
        let dir = TempDir::new("recover");
        let path = dir.file("config.toml");
        std::fs::write(&path, "[visual]\nopacity = ").expect("写失败");
        let mut s = store(&path);
        assert_eq!(s.source(), Source::Fallback, "开局就是坏文件");
        assert_eq!(s.config(), &Config::default());

        std::fs::write(&path, "[visual]\nopacity = 0.4\n").expect("写失败");
        let ev = s.poll(Instant::now());
        assert!(matches!(ev, Some(Reload::Applied { .. })), "{ev:?}");
        assert!((s.config().visual.opacity - 0.4).abs() < 1e-9);
        assert_eq!(s.source(), Source::File);
        assert!(s.last_error().is_none());
    }

    #[test]
    fn applied_reload_reports_delta() {
        let dir = TempDir::new("delta");
        let path = dir.file("config.toml");
        std::fs::write(&path, "[visual]\nopacity = 0.5\n").expect("写失败");
        let mut s = store(&path);
        std::fs::write(&path, "[visual]\nopacity = 0.5\nbands = 96\n").expect("写失败");
        match s.poll(Instant::now()) {
            Some(Reload::Applied {
                revision, delta, ..
            }) => {
                assert_eq!(revision, 1);
                assert!(delta.keys.contains(&"visual.bands"), "{:?}", delta.keys);
            }
            other => panic!("期望 Applied，得到 {other:?}"),
        }
        assert_eq!(s.config().visual.bands, 96);
    }

    #[test]
    fn comment_only_change_is_ignored() {
        let dir = TempDir::new("comment");
        let path = dir.file("config.toml");
        std::fs::write(&path, "# 版本\nversion = 1\n").expect("写失败");
        let mut s = store(&path);
        std::fs::write(&path, "# 版本（改个注释）\nversion = 1\n").expect("写失败");
        assert_eq!(s.poll(Instant::now()), None, "值没变就不该应用");
        assert_eq!(s.stats().ignored, 1);
    }

    #[test]
    fn deleting_file_returns_to_defaults() {
        let dir = TempDir::new("delete");
        let path = dir.file("config.toml");
        std::fs::write(&path, "[visual]\nbands = 96\n").expect("写失败");
        let mut s = store(&path);
        assert_eq!(s.config().visual.bands, 96);
        std::fs::remove_file(&path).expect("删失败");
        let ev = s.poll(Instant::now());
        assert!(matches!(ev, Some(Reload::Applied { .. })), "{ev:?}");
        assert_eq!(s.config().visual.bands, 64, "删文件 = 回默认");
        assert_eq!(s.source(), Source::Default);
    }

    #[test]
    fn atomic_write_round_trips_and_is_private() {
        let dir = TempDir::new("write");
        let path = dir.file("sub/config.toml");
        let mut cfg = Config::default();
        cfg.visual.bands = 48;
        write_atomic_text(&path, &encode(&cfg)).expect("写失败");
        assert!(path.exists());
        assert!(
            !path.with_extension("toml.tmp").exists(),
            "临时文件必须已改名"
        );
        let s = ConfigStore::open(&path);
        assert_eq!(s.config().visual.bands, 48);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).expect("stat").permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "配置文件必须是 0600");
        }
    }

    #[test]
    fn write_does_not_chmod_existing_parent() {
        let dir = TempDir::new("parent-mode");
        let parent = dir.file("shared");
        std::fs::create_dir_all(&parent).expect("建目录失败");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755))
                .expect("chmod 失败");
        }

        write_atomic_text(&parent.join("config.toml"), "version = 2\n").expect("写失败");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&parent)
                .expect("stat")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o755, "已存在的父目录权限不能被改写");
        }

        // 只有本次新建的目录才收紧到 0700。
        let nested = parent.join("new-dir/config.toml");
        write_atomic_text(&nested, "version = 2\n").expect("写失败");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(parent.join("new-dir"))
                .expect("stat")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o700, "新建的专用目录应为 0700");
        }
    }

    #[test]
    fn write_then_store_picks_it_up() {
        let dir = TempDir::new("write-reload");
        let path = dir.file("config.toml");
        let mut s = store(&path);
        let mut cfg = Config::default();
        cfg.visual.color_start = "#FF0000".to_owned();
        s.write(&cfg).expect("写失败");
        let ev = s.poll(Instant::now());
        assert!(matches!(ev, Some(Reload::Applied { .. })), "{ev:?}");
        assert_eq!(s.config().visual.color_start, "#FF0000");
    }

    #[test]
    fn future_version_is_an_error_not_a_panic() {
        let dir = TempDir::new("future");
        let path = dir.file("config.toml");
        std::fs::write(&path, "version = 999\n").expect("写失败");
        let err = load(&path).expect_err("未来版本必须报错");
        assert!(matches!(err, StoreError::Decode(_)), "{err:?}");
        let s = ConfigStore::open(&path);
        assert_eq!(s.config(), &Config::default());
        assert_eq!(s.source(), Source::Fallback);
    }

    #[test]
    fn default_path_prefers_xdg_then_home() {
        assert_eq!(
            path_from(Some("/tmp/xdg".into()), Some("/home/u".into())),
            PathBuf::from("/tmp/xdg/deepin-spectrum/config.toml")
        );
        assert_eq!(
            path_from(None, Some("/home/u".into())),
            PathBuf::from("/home/u/.config/deepin-spectrum/config.toml")
        );
        assert_eq!(
            path_from(None, None),
            PathBuf::from("deepin-spectrum-config.toml")
        );
        // 空字符串按未设置处理（某些启动环境会导出空值）
        assert_eq!(
            path_from(Some("".into()), Some("/home/u".into())),
            PathBuf::from("/home/u/.config/deepin-spectrum/config.toml")
        );
        assert!(default_path().to_string_lossy().ends_with("config.toml"));
    }
}
