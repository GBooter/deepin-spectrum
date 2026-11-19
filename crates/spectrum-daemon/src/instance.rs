//! 单实例保护 + 控制通道。
//!
//! ## 为什么必须有
//!
//! 两个 daemon 同时跑会各自建一块覆盖同一区域的窗口，各自按自己的帧率上屏 →
//! 用户看到的是**两块画面互相覆盖、闪动**。这种故障很难自诊断（两个进程都"正常"），
//! 所以必须在启动时挡住。
//!
//! ## 实现方式：`flock` guard + `Unix socket`
//!
//! 光靠 `UnixListener::bind` 有 TOCTOU：两个进程可能同时看到 `AddrInUse`、
//! 同时判断对端是僵尸、同时删除并重新绑定，结果两个都成功。
//!
//! 因此先用 `flock(LOCK_EX|LOCK_NB)` 锁住同目录的 `*.guard` 文件：
//! 只有拿到锁的进程才能检查/清理/绑定 socket。guard 文件**不删除**——
//! 删除会让下一个进程创建新 inode 并成功加锁，重新引入竞态。
//!
//! socket 仍承担控制通道（ping/status/stop）与“真有实例在跑”的探测。
//!
//! ## 顺手做成控制通道
//!
//! 托盘需要"停止 / 重启频谱"，而它**不能**靠 `kill` 去猜 PID：
//! 猜错会杀掉别的进程，且杀 `SIGKILL` 会让 X 窗口残留到连接断开才消失。
//!
//! 既然占位 socket 已经存在且唯一，就让它承载一行命令：
//!
//! ```text
//! ping    → ok                 （存活探测；托盘据此显示"运行中/已停止"）
//! status  → ok running <pid>
//! stop    → ok stopping        （主循环优雅退出，会销毁叠加窗）
//! ```
//!
//! 好处：不需要 PID 文件、不需要额外端口、权限天然正确（同一用户）。
//!
//! ## 不做什么
//!
//! 不写 PID 文件、**不杀旧进程**（`stop` 是请求退出，不是 `kill`）。
//! 用户自己启动的进程不该被另一个进程悄悄杀掉 ——
//! 拒绝启动并说明原因，把决定权留给用户。

use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// `flock(2)` 常量（Linux/Unix 通用值）。
const LOCK_EX: i32 = 2;
const LOCK_NB: i32 = 4;

// 只声明一个 C ABI 函数，避免为一个锁引入 libc 依赖。
unsafe extern "C" {
    fn flock(fd: i32, operation: i32) -> i32;
}

/// 进程间互斥锁：`flock` 锁住 guard 文件；结构体存活期间锁一直有效。
struct InstanceLock {
    _file: File,
}

impl InstanceLock {
    /// 尝试获取独占锁；`Ok(None)` 表示已被其它进程持有。
    fn acquire(path: &Path) -> io::Result<Option<Self>> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        // SAFETY: `file` 在调用期间保持打开，fd 有效；`flock` 只操作该 fd。
        let rc = unsafe { flock(file.as_raw_fd(), LOCK_EX | LOCK_NB) };
        if rc == 0 {
            return Ok(Some(Self { _file: file }));
        }
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::WouldBlock {
            Ok(None)
        } else {
            Err(err)
        }
    }
}

/// 控制通道的单行命令。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlCommand {
    /// 存活探测。
    Ping,
    /// 查询状态。
    Status,
    /// 请求优雅退出。
    Stop,
}

impl ControlCommand {
    /// 解析一行命令（大小写不敏感，容忍首尾空白）。
    #[must_use]
    pub fn parse(line: &str) -> Option<Self> {
        match line.trim().to_ascii_lowercase().as_str() {
            "ping" => Some(Self::Ping),
            "status" => Some(Self::Status),
            "stop" => Some(Self::Stop),
            _ => None,
        }
    }

    /// 线上表示。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ping => "ping",
            Self::Status => "status",
            Self::Stop => "stop",
        }
    }
}

/// 主循环与信号线程共享的状态。
///
/// 只用原子布尔：主循环每帧读一次，写入来自控制线程，
/// 不需要锁，也不会有优先级反转。
#[derive(Debug, Default)]
pub struct ControlState {
    stop: AtomicBool,
}

impl ControlState {
    /// 请求退出（控制线程调用）。
    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    /// 是否已被请求退出（主循环调用）。
    #[must_use]
    pub fn stop_requested(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }
}

/// 另一个实例已在运行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlreadyRunning {
    /// 占位文件路径（供提示用）。
    pub path: PathBuf,
}

impl std::fmt::Display for AlreadyRunning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "已有另一个 deepin-spectrum 在运行（{}）；\
             同时运行两个会让两块叠加层互相覆盖、画面闪动。\
             先结束旧的进程再启动。",
            self.path.display()
        )
    }
}

impl std::error::Error for AlreadyRunning {}

/// 持有单实例占位；`Drop` 时清理。
pub struct SingleInstance {
    path: PathBuf,
    /// 进程间互斥锁；必须比 socket 活得久（Drop 顺序由字段声明保证）。
    _guard: InstanceLock,
    /// 只为持有 socket；`Drop` 时靠它释放监听。
    listener: UnixListener,
    /// 控制线程的存活标志。
    alive: Arc<AtomicBool>,
    /// 主循环读的状态。
    control: Arc<ControlState>,
    /// 控制线程句柄（`Drop` 时 join，确保不会在文件删除后还 accept）。
    thread: Option<std::thread::JoinHandle<()>>,
}

impl SingleInstance {
    /// 尝试占位。
    ///
    /// # Errors
    ///
    /// 已有实例在运行时返回 [`AlreadyRunning`]；文件系统错误返回 [`io::Error`]。
    pub fn acquire(path: impl AsRef<Path>) -> Result<Self, Box<dyn std::error::Error>> {
        let path = path.as_ref().to_path_buf();
        // `Unix socket` 路径上限是 `SUN_LEN`（约 108 字节）。超了 bind 会返回
        // `InvalidInput: path must be shorter than SUN_LEN` —— 报错信息完全看不出
        // 是"路径太长"，所以在这里提前给出可读提示。
        if path.as_os_str().len() >= 100 {
            return Err(Box::new(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "占位路径过长（{} 字节，上限约 100）：{}",
                    path.as_os_str().len(),
                    path.display()
                ),
            )));
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // 先拿 flock：拿不到就是有实例在跑，且消除了“同时清理僵尸 socket”的竞态。
        let guard_path = path.with_extension("guard");
        let Some(guard) = InstanceLock::acquire(&guard_path)? else {
            return Err(Box::new(AlreadyRunning { path }));
        };

        let listener = match UnixListener::bind(&path) {
            Ok(listener) => listener,
            Err(err) if err.kind() == io::ErrorKind::AddrInUse => {
                // 有人在监听 → 真实例；没人监听 → 崩溃留下的僵尸文件
                if UnixStream::connect(&path).is_ok() {
                    return Err(Box::new(AlreadyRunning { path }));
                }
                // 僵尸文件：删掉重试一次。若这次仍失败，说明有竞态，如实报错。
                match std::fs::remove_file(&path) {
                    Ok(()) => {}
                    Err(remove_err) if remove_err.kind() == io::ErrorKind::NotFound => {}
                    Err(remove_err) => return Err(Box::new(remove_err)),
                }
                UnixListener::bind(&path)?
            }
            Err(err) => return Err(Box::new(err)),
        };

        // 控制通道：非阻塞 accept，靠 alive 标志退出。
        // 用非阻塞是为了 `Drop` 时能及时 join，而不是卡在 accept 上。
        listener.set_nonblocking(true)?;
        let alive = Arc::new(AtomicBool::new(true));
        let control = Arc::new(ControlState::default());
        let thread = {
            let listener = listener.try_clone()?;
            let alive = Arc::clone(&alive);
            let control = Arc::clone(&control);
            std::thread::Builder::new()
                .name("deepin-spectrum-control".to_string())
                .spawn(move || control_loop(&listener, &alive, &control))?
        };

        Ok(Self {
            path,
            _guard: guard,
            listener,
            alive,
            control,
            thread: Some(thread),
        })
    }

    /// 占位文件路径。
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 主循环要读的控制状态。
    #[must_use]
    pub fn control(&self) -> Arc<ControlState> {
        Arc::clone(&self.control)
    }
}

impl Drop for SingleInstance {
    fn drop(&mut self) {
        // 先让控制线程退出并 join，再删文件 ——
        // 否则线程可能仍在 accept 一个刚被删掉的路径（虽无害，但日志会乱）
        self.alive.store(false, Ordering::SeqCst);
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
        // 只清理自己的占位；失败无从处理（进程正在退出），忽略即可。
        let _ = std::fs::remove_file(&self.path);
        // listener 在结构体里，Drop 时自动关闭
        let _ = &self.listener;
    }
}

impl std::fmt::Debug for SingleInstance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SingleInstance")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

/// 接受控制连接并回一行结果。
///
/// 注意：单实例检查里的"存活探测"也是 `connect` 然后**立刻关闭**，
/// 这种连接读到的是 EOF，不能报错、也不能阻塞。
fn control_loop(listener: &UnixListener, alive: &AtomicBool, control: &ControlState) {
    while alive.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _addr)) => handle_client(stream, control),
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => {
                // 监听本身出问题（极少见）时不要忙等
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

fn handle_client(stream: UnixStream, control: &ControlState) {
    // 超时保护：客户端不发数据也不能把控制线程吊死
    let _ = stream.set_read_timeout(Some(Duration::from_millis(300)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(300)));
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    });
    let mut line = String::new();
    // EOF（存活探测）→ 直接返回，不回复
    if reader.read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }
    let Some(cmd) = ControlCommand::parse(&line) else {
        reply(stream, "err unknown-command");
        return;
    };
    match cmd {
        ControlCommand::Ping => reply(stream, "ok"),
        ControlCommand::Status => reply(stream, &format!("ok running {}", std::process::id())),
        ControlCommand::Stop => {
            control.request_stop();
            reply(stream, "ok stopping");
        }
    }
}

fn reply(stream: UnixStream, line: &str) {
    let mut stream = stream;
    let _ = writeln!(stream, "{line}");
    let _ = stream.flush();
}

/// 向运行中的实例发一条命令，返回回复行。
///
/// # Errors
///
/// 实例未运行（连接失败）或读写超时时返回错误。
pub fn send_command(path: impl AsRef<Path>, cmd: ControlCommand) -> io::Result<String> {
    let mut stream = UnixStream::connect(path.as_ref())?;
    stream.set_read_timeout(Some(Duration::from_millis(800)))?;
    stream.set_write_timeout(Some(Duration::from_millis(800)))?;
    writeln!(stream, "{}", cmd.as_str())?;
    stream.flush()?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    Ok(line.trim_end().to_string())
}

/// 是否有实例在跑（`ping` 成功即算）。
///
/// 比"看 socket 文件在不在"可靠：僵尸文件会让后者误判为"在跑"。
#[must_use]
pub fn is_running(path: impl AsRef<Path>) -> bool {
    send_command(path, ControlCommand::Ping).is_ok_and(|r| r.starts_with("ok"))
}

/// 默认占位路径：优先 `XDG_RUNTIME_DIR`（用户私有、重启自动清空），
/// 退到 `/tmp`。
#[must_use]
pub fn default_lock_path() -> PathBuf {
    match std::env::var("XDG_RUNTIME_DIR") {
        Ok(dir) if !dir.is_empty() => PathBuf::from(dir).join("deepin-spectrum.lock"),
        _ => PathBuf::from("/tmp/deepin-spectrum.lock"),
    }
}

#[cfg(test)]
mod tests {
    use super::{AlreadyRunning, InstanceLock, SingleInstance, default_lock_path};

    /// 每个测试用独立路径，避免并行执行时互相踩。
    ///
    /// 故意**不用** `std::env::temp_dir()`：本环境 `TMPDIR` 指向项目内长路径，
    /// 会超过 `Unix socket` 的 `SUN_LEN` 上限（约 108 字节）。用 `/tmp` 下的短名。
    fn temp_path(tag: &str) -> std::path::PathBuf {
        let path = std::path::PathBuf::from(format!("/tmp/sp-{tag}-{}.lock", std::process::id()));
        // 清理上次残留（若失败不影响断言，acquire 自己也会处理僵尸）
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("guard"));
        path
    }

    #[test]
    fn flock_guard_is_exclusive() {
        let path = temp_path("flock");
        let guard_path = path.with_extension("guard");
        let first = InstanceLock::acquire(&guard_path)
            .expect("加锁不应失败")
            .expect("第一次应拿到锁");
        assert!(
            InstanceLock::acquire(&guard_path)
                .expect("第二次加锁调用本身不应失败")
                .is_none(),
            "同一文件同时只允许一个 flock"
        );
        drop(first);
        assert!(
            InstanceLock::acquire(&guard_path)
                .expect("释放后加锁不应失败")
                .is_some(),
            "释放后应能重新加锁"
        );
    }

    #[test]
    fn second_acquire_is_rejected() {
        let path = temp_path("dup");
        let first = SingleInstance::acquire(&path).expect("第一次应成功");
        let err = SingleInstance::acquire(&path).expect_err("第二次应被拒绝");
        let already = err
            .downcast_ref::<AlreadyRunning>()
            .expect("错误类型应为 AlreadyRunning");
        assert_eq!(already.path, path);
        drop(first);
        // 释放后应能再次获取
        let third = SingleInstance::acquire(&path).expect("释放后应能重新获取");
        assert_eq!(third.path(), path.as_path());
    }

    #[test]
    fn stale_socket_is_reclaimed() {
        let path = temp_path("stale");
        // 模拟崩溃：`UnixListener` 在 Drop 时**不会**删除 socket 文件，
        // 所以 bind 之后直接 drop 就得到"文件还在、没人监听"的僵尸状态。
        //
        // 注意不能用 `mem::forget` 来模拟：那样 fd 仍打开、仍在监听，
        // connect 会成功，等于"真的有实例在跑"，测出来的结论正好相反。
        {
            let listener = std::os::unix::net::UnixListener::bind(&path).expect("bind");
            drop(listener);
        }
        assert!(path.exists(), "僵尸 socket 文件应存在");
        assert!(
            std::os::unix::net::UnixStream::connect(&path).is_err(),
            "僵尸文件应连不上（否则就不是僵尸）"
        );
        // 没人 connect 得上 → 应被回收
        let got = SingleInstance::acquire(&path).expect("僵尸占位应被回收");
        assert_eq!(got.path(), path.as_path());
    }

    #[test]
    fn drop_removes_placeholder() {
        let path = temp_path("cleanup");
        {
            let _guard = SingleInstance::acquire(&path).expect("acquire");
            assert!(path.exists());
        }
        assert!(!path.exists(), "退出后占位文件应被清理");
    }

    #[test]
    fn control_ping_status_and_stop() {
        use super::{ControlCommand, is_running, send_command};

        let path = temp_path("control");
        let guard = SingleInstance::acquire(&path).expect("acquire");
        let control = guard.control();

        assert!(is_running(&path), "ping 应成功");
        let status = send_command(&path, ControlCommand::Status).expect("status");
        assert!(
            status.starts_with("ok running "),
            "status 回复格式不符：{status}"
        );

        assert!(!control.stop_requested(), "初始不应被请求退出");
        let reply = send_command(&path, ControlCommand::Stop).expect("stop");
        assert_eq!(reply, "ok stopping");
        assert!(control.stop_requested(), "收到 stop 后主循环应看到退出请求");
    }

    #[test]
    fn unknown_command_is_rejected() {
        use super::ControlCommand;

        assert_eq!(ControlCommand::parse("ping"), Some(ControlCommand::Ping));
        assert_eq!(
            ControlCommand::parse("  STOP \n"),
            Some(ControlCommand::Stop),
            "应大小写不敏感且容忍空白"
        );
        assert_eq!(ControlCommand::parse("rm -rf /"), None);
        assert_eq!(ControlCommand::parse(""), None);
    }

    #[test]
    fn is_running_is_false_without_instance() {
        use super::is_running;

        let path = temp_path("absent");
        assert!(!is_running(&path), "没有实例时应返回 false");
    }

    #[test]
    fn default_path_is_absolute() {
        let path = default_lock_path();
        assert!(path.is_absolute(), "占位路径必须是绝对路径：{path:?}");
        assert!(path.to_string_lossy().contains("deepin-spectrum"));
    }
}
