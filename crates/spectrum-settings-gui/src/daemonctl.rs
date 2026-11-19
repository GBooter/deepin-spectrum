//! 频谱运行控制（启动 / 停止 / 状态查询）。
//!
//! ## 协议与路径（与 `spectrum-daemon/src/instance.rs` 保持一致）
//!
//! 控制通道是 `$XDG_RUNTIME_DIR/deepin-spectrum.lock` 上的 Unix socket：
//! 连上后发一行命令（`ping` / `status` / `stop`），daemon 回一行。
//! 本模块是 GUI 侧的**协议复刻**——GUI 是独立 workspace，不能直接依赖
//! `spectrum-daemon`（那会拖进 X11/Pulse 依赖树）。两处协议若分叉，
//! 表现是"GUI 以为没运行"，修改时务必两边同步。
//!
//! ## 启动方式
//!
//! 与托盘（`tray.rs`）一致：spawn 一个**分离**的 `deepin-spectrum daemon --live`
//! 进程，**不保存 Child 句柄**（§15.5 的约束：界面退出不能关闭频谱）。
//! stdout/stderr 追加到日志文件（M57：失败必须留痕）。

use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// 启动后确认存活的稳定窗口。
///
/// daemon 的控制 socket 在音频/叠加层初始化之前就已就绪，只 ping 一次会把
/// “刚起来就 panic”误判成成功；固定观察一个窗口再判定。
const START_SETTLE: Duration = Duration::from_millis(1500);

/// 静默重试次数：第一次失败可能是音频服务/合成器刚登录尚未就绪。
const START_ATTEMPTS: usize = 2;

/// 状态由“运行中”变为“未运行”的抑制窗口。
///
/// systemd 重启通常 1~2s 内完成，瞬时探测失败不应让界面按钮闪一下。
const STATUS_DOWN_DEBOUNCE: Duration = Duration::from_millis(1500);

/// 控制通道 socket 路径（与 `instance::default_lock_path` 一致）。
#[must_use]
pub fn lock_path() -> PathBuf {
    match std::env::var("XDG_RUNTIME_DIR") {
        Ok(dir) if !dir.is_empty() => PathBuf::from(dir).join("deepin-spectrum.lock"),
        _ => PathBuf::from("/tmp/deepin-spectrum.lock"),
    }
}

/// daemon 二进制的候选路径（与 daemon 里 `spawn_settings_gui` 的查找逻辑一致）。
pub(crate) fn daemon_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        out.push(dir.join("deepin-spectrum"));
    }
    out.push(PathBuf::from("/usr/bin/deepin-spectrum"));
    out.push(PathBuf::from("/usr/local/bin/deepin-spectrum"));
    out
}

/// 解析出实际存在的 `deepin-spectrum` 二进制（daemon 与托盘共用）。
pub(crate) fn daemon_binary() -> Result<PathBuf, String> {
    daemon_candidates()
        .into_iter()
        .find(|p| p.is_file())
        .ok_or_else(|| {
            "找不到 deepin-spectrum（devel 环境：先 cargo build --release \
             --features dbus,x11,pulse,tray）"
                .to_owned()
        })
}

/// 向 daemon 发一条命令并返回回复。
fn send(line: &str) -> Result<String, String> {
    let mut stream =
        UnixStream::connect(lock_path()).map_err(|_| "连接失败（daemon 未运行）".to_owned())?;
    stream
        .set_read_timeout(Some(Duration::from_millis(800)))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(Duration::from_millis(800)))
        .map_err(|e| e.to_string())?;
    writeln!(stream, "{line}").map_err(|e| e.to_string())?;
    stream.flush().map_err(|e| e.to_string())?;
    let mut reply = String::new();
    BufReader::new(stream)
        .read_line(&mut reply)
        .map_err(|e| e.to_string())?;
    Ok(reply.trim_end().to_owned())
}

/// daemon 是否在运行（`ping` 成功即算；比看 socket 文件可靠，M50）。
#[must_use]
pub fn is_running() -> bool {
    send("ping").is_ok_and(|r| r.starts_with("ok"))
}

/// 请求 daemon 优雅退出（它会销毁叠加窗，不留残影）。
///
/// 发出 `stop` 后**等它真正退出**（最多 1s）再返回：否则界面会先乐观地
/// 变成"未运行"，状态轮询又可能读到一个还没退干净的实例，把按钮闪回
/// "运行中"再变回来。daemon 正常退出是毫秒级，这个等待通常只有 50~100ms。
///
/// # Errors
///
/// 连接失败（本就没运行）或读超时。
pub fn stop() -> Result<(), String> {
    let reply = send("stop")?;
    if !reply.starts_with("ok") {
        return Err(format!("daemon 回复异常：{reply}"));
    }
    let deadline = Instant::now() + Duration::from_millis(800);
    while Instant::now() < deadline {
        if !is_running() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    // AUD-033：超时仍未停止不能返回 Ok，否则界面会乐观地显示"已停止"，
    // 而实际 daemon 还在上屏。
    Err("停止超时：daemon 在 800ms 内没有退出".to_owned())
}

/// GUI 拉起 daemon 时的日志文件（M57：失败必须留痕）。
fn log_path() -> PathBuf {
    match lock_path().parent() {
        Some(dir) => dir.join("deepin-spectrum-gui-daemon.log"),
        None => PathBuf::from("/tmp/deepin-spectrum-gui-daemon.log"),
    }
}

/// 启动 daemon（分离进程，`--live` 采集真实音频）。
///
/// `config` 是界面正在编辑的配置文件路径；传 `Some` 时 daemon 会读同一份配置，
/// 否则界面里改的键和 daemon 实际生效的键可能不是同一个文件。
///
/// # Errors
///
/// 找不到 daemon 二进制，或 spawn 失败。
pub fn start(config: Option<&Path>) -> Result<(), String> {
    let bin = daemon_binary()?;
    let log = log_path();
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
        .map_err(|e| format!("打开日志 {} 失败：{e}", log.display()))?;
    let log_err = log_file
        .try_clone()
        .map_err(|e| format!("复制日志句柄失败：{e}"))?;
    build_command(&bin, config)
        .stdin(Stdio::null())
        .stdout(log_file)
        .stderr(log_err)
        .spawn()
        .map_err(|e| format!("启动 daemon 失败：{e}"))?;
    // 不保存 Child：界面退出不能关闭频谱（§15.5）
    Ok(())
}

/// 启动并确认 daemon 在稳定窗口后仍然存活。
///
/// 第一次失败会**静默重试一次**（覆盖音频服务/合成器刚登录尚未就绪的瞬态
/// 失败），仍失败才返回可读原因，供界面提示一次；不弹窗、不反复报错。
///
/// # Errors
///
/// 找不到二进制、spawn 失败，或重试后仍未存活（错误文本含日志末行与路径）。
pub fn start_verified(config: Option<&Path>) -> Result<(), String> {
    let log = log_path();
    for attempt in 0..START_ATTEMPTS {
        // 只读取本次启动新写入的日志，避免把上一次会话的旧错误当成原因。
        let offset = std::fs::metadata(&log).map_or(0, |meta| meta.len());
        start(config)?;
        std::thread::sleep(START_SETTLE);
        if is_running() {
            return Ok(());
        }
        if attempt + 1 < START_ATTEMPTS {
            std::thread::sleep(Duration::from_millis(300));
        } else {
            let reason = last_meaningful_line(&read_log_since(&log, offset), 160)
                .unwrap_or_else(|| "进程启动后立即退出".to_owned());
            return Err(format!("{reason}（日志：{}）", log.display()));
        }
    }
    Err("启动失败（未知原因）".to_owned())
}

/// 读取日志文件从 `offset` 开始的新内容（最多 64 KiB）。
fn read_log_since(path: &Path, offset: u64) -> String {
    let Ok(mut file) = std::fs::File::open(path) else {
        return String::new();
    };
    let len = file.metadata().map_or(0, |meta| meta.len());
    if len <= offset {
        return String::new();
    }
    let start = offset.max(len.saturating_sub(64 * 1024));
    if file.seek(SeekFrom::Start(start)).is_err() {
        return String::new();
    }
    let mut bytes = Vec::new();
    if file.read_to_end(&mut bytes).is_err() {
        return String::new();
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// 取最后一行有诊断价值的日志：跳过正常启动噪声和 `RUST_BACKTRACE` 提示。
fn last_meaningful_line(text: &str, max_chars: usize) -> Option<String> {
    text.lines()
        .map(str::trim)
        .rfind(|line| {
            !line.is_empty()
                && !line.starts_with("配置 ")
                && !line.starts_with("geometry ")
                && !line.starts_with("pipeline:")
                && !line.starts_with("overlay wayland")
                && !line.starts_with("  Wayland scale")
                && !line.starts_with("note: run with `RUST_BACKTRACE=1`")
        })
        .map(|line| truncate_chars(line, max_chars))
}

/// 按字符截断（中文路径/错误不会被切在半个 UTF-8 字符上）。
fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max_chars).collect();
    out.push('…');
    out
}

/// 组装 daemon 启动命令（单独拆出来便于断言参数）。
fn build_command(bin: &Path, config: Option<&Path>) -> Command {
    let mut cmd = Command::new(bin);
    cmd.args(["daemon", "--live"]);
    if let Some(path) = config {
        cmd.arg("--config").arg(path);
    }
    cmd
}

/// 状态回调：`(是否运行, 状态文案)`。
pub type StatusCallback = Arc<dyn Fn(bool, &str) + Send + Sync>;

/// “未运行”状态去抖：瞬时探测失败不改变界面，持续失败才上报。
///
/// systemd 服务在瞬态故障后会自己拉起；如果界面每 500ms 就把按钮闪成
/// “未运行”，用户会以为又坏了。这里只抑制“运行中 → 未运行”的下降沿，
/// 恢复方向立即上报。
#[derive(Debug, Default)]
struct DownDebounce {
    since: Option<Instant>,
}

impl DownDebounce {
    /// `Some(true)` = 运行中；`Some(false)` = 已持续未运行；`None` = 保持原状态。
    fn observe(&mut self, running: bool, now: Instant) -> Option<bool> {
        if running {
            self.since = None;
            return Some(true);
        }
        let since = *self.since.get_or_insert(now);
        (now.saturating_duration_since(since) >= STATUS_DOWN_DEBOUNCE).then_some(false)
    }
}

/// 后台状态轮询线程（500ms 一次，结果经回调送到主线程）。
///
/// 500ms 而不是 2s：按钮的启用状态直接绑在 `daemon_running` 上，
/// 轮询太慢会让用户点完「启动/停止」后觉得按钮"半天没反应"。
pub fn spawn_status_watcher(callback: StatusCallback) -> StatusWatcher {
    let stop = Arc::new(AtomicBool::new(false));
    let handle_stop = Arc::clone(&stop);
    let handle = std::thread::Builder::new()
        .name("spectrum-gui-status".to_owned())
        .spawn(move || {
            let mut debounce = DownDebounce::default();
            loop {
                if handle_stop.load(Ordering::SeqCst) {
                    break;
                }
                // 状态文本只关心"是否运行"，不显示 pid（用户不需要看进程号）
                let running = is_running();
                if let Some(state) = debounce.observe(running, Instant::now()) {
                    let text = if state { "运行中" } else { "未运行" };
                    callback(state, text);
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        })
        .ok();
    StatusWatcher { stop, handle }
}

/// 状态轮询线程的句柄；丢弃即停止。
pub struct StatusWatcher {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Drop for StatusWatcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DownDebounce, build_command, daemon_candidates, is_running, last_meaningful_line,
        lock_path, truncate_chars,
    };
    use std::path::Path;
    use std::time::{Duration, Instant};

    #[test]
    fn start_command_forwards_config_path() {
        let cmd = build_command(
            Path::new("/usr/bin/deepin-spectrum"),
            Some(Path::new("/tmp/x.toml")),
        );
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            ["daemon", "--live", "--config", "/tmp/x.toml"],
            "界面编辑的配置必须原样传给 daemon"
        );
        let plain = build_command(Path::new("/usr/bin/deepin-spectrum"), None);
        let plain_args: Vec<String> = plain
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(plain_args, ["daemon", "--live"]);
    }

    #[test]
    fn lock_path_prefers_xdg_runtime_dir() {
        let path = lock_path();
        let text = path.to_string_lossy();
        assert!(
            text.ends_with("deepin-spectrum.lock"),
            "socket 路径应以 deepin-spectrum.lock 结尾：{text}"
        );
    }

    #[test]
    fn daemon_candidates_cover_dev_and_install_paths() {
        let cands = daemon_candidates();
        assert!(
            cands
                .iter()
                .any(|p| p.to_string_lossy().contains("/usr/bin")),
            "应有 /usr/bin 候选（安装路径）"
        );
        assert!(cands.iter().all(|p| p.ends_with("deepin-spectrum")));
    }

    #[test]
    fn status_query_never_panics_without_daemon() {
        // daemon 多半没跑：is_running 只应返回 false，绝不能 panic
        let _ = is_running();
    }

    #[test]
    fn transient_downtime_does_not_flap_the_ui() {
        let t0 = Instant::now();
        let mut debounce = DownDebounce::default();
        assert_eq!(debounce.observe(true, t0), Some(true));
        // systemd 正在重启：1.5s 内的失败先不上报
        assert_eq!(
            debounce.observe(false, t0 + Duration::from_millis(500)),
            None
        );
        assert_eq!(
            debounce.observe(false, t0 + Duration::from_millis(1200)),
            None
        );
        // 持续未运行超过去抖窗口才上报
        assert_eq!(
            debounce.observe(false, t0 + Duration::from_millis(2100)),
            Some(false)
        );
        // 恢复方向立即上报
        assert_eq!(
            debounce.observe(true, t0 + Duration::from_millis(2200)),
            Some(true)
        );
    }

    #[test]
    fn last_log_line_skips_normal_startup_noise() {
        let log = "配置 /home/u/.config/deepin-spectrum/config.toml（文件）\n\
                   geometry 2048x53 @ (0, 0)\n\
                   pipeline: 72 频段 / hop 1024 帧 / sample_rate 48000\n\
                   overlay wayland 2048x53 (scale 1)，位置 2，任务栏内\n\
                   thread 'deepin-spectrum-wayland' panicked at wayland-client:\n\
                   Missing event_created_child specialization\n\
                   note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace\n";
        assert_eq!(
            last_meaningful_line(log, 160).as_deref(),
            Some("Missing event_created_child specialization")
        );
    }

    #[test]
    fn log_line_truncation_is_utf8_safe() {
        let text = "汉".repeat(200);
        let out = truncate_chars(&text, 160);
        assert_eq!(out.chars().count(), 161, "160 个字符 + 省略号");
        assert!(out.ends_with('…'));
    }
}
