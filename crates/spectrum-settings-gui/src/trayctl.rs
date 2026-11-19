//! 系统托盘图标的开关。
//!
//! ## 为什么放进设置窗口
//!
//! 0.2.5 及之前托盘是独立的 `.desktop` 入口，装完应用菜单里会多出第二个
//! 图标。用户只需要一个入口：设置窗口。托盘改成这里的选项 —— 勾选时启动
//! `deepin-spectrum tray`，取消勾选时结束它，默认**不显示**。
//!
//! ## 状态从哪里来
//!
//! 托盘启动后把 pid 写进 `$XDG_RUNTIME_DIR/deepin-spectrum-tray.pid`；
//! 界面下次打开时读它并核对 `/proc/<pid>/cmdline` 确实是 `... tray`。
//! 不查 D-Bus 的 `org.kde.StatusNotifierItem-*`：GUI 是独立 workspace，
//! 不想为一个开关引入 zbus。
//!
//! ## 登录后保留
//!
//! 勾选时同时写 `~/.config/autostart/deepin-spectrum-tray.desktop`，
//! 取消时删掉 —— 否则"选项"只在本次登录有效，重启后图标又消失。

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// XDG 登录自启文件名。
pub const AUTOSTART_FILE: &str = "deepin-spectrum-tray.desktop";

/// 发出 `SIGTERM` 后等待托盘退出的上限。
const STOP_TIMEOUT: Duration = Duration::from_millis(800);

/// 启动后的观察窗口：D-Bus 不可用等情况下托盘会立刻退出，必须能报出来。
const START_PROBE: Duration = Duration::from_millis(200);

fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|dir| !dir.is_empty())
        .map_or_else(std::env::temp_dir, PathBuf::from)
}

/// 托盘 pid 文件路径（放 `$XDG_RUNTIME_DIR`，登出随目录清理）。
#[must_use]
pub fn pid_path() -> PathBuf {
    runtime_dir().join("deepin-spectrum-tray.pid")
}

fn log_path() -> PathBuf {
    runtime_dir().join("deepin-spectrum-tray.log")
}

fn autostart_path() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .map(|dir| dir.join("autostart").join(AUTOSTART_FILE))
}

/// 托盘是否正在运行（pid 文件存在且进程确实是 `deepin-spectrum tray`）。
#[must_use]
pub fn is_running() -> bool {
    read_pid().is_some_and(process_alive)
}

fn read_pid() -> Option<u32> {
    std::fs::read_to_string(pid_path())
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// 核对 `/proc/<pid>/cmdline`：只看 `/proc/<pid>` 是否存在会误判 pid 复用。
fn process_alive(pid: u32) -> bool {
    let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
        return false;
    };
    let mut has_bin = false;
    let mut has_tray = false;
    for part in cmdline.split(|byte| *byte == 0) {
        if part.ends_with(b"deepin-spectrum") {
            has_bin = true;
        }
        if part == b"tray" {
            has_tray = true;
        }
    }
    has_bin && has_tray
}

/// 打开 / 关闭托盘图标。
///
/// `true`：没有实例就启动 `deepin-spectrum tray`，并写入登录自启文件；
/// `false`：结束托盘进程并删除自启文件。
///
/// # Errors
///
/// 找不到二进制、spawn 失败、托盘启动后立即退出、或进程无法停止。
pub fn set_enabled(enabled: bool) -> Result<(), String> {
    if enabled { enable() } else { disable() }
}

fn enable() -> Result<(), String> {
    let bin = crate::daemonctl::daemon_binary()?;
    if !is_running() {
        spawn_tray(&bin)?;
    }
    write_autostart(&bin)
}

fn spawn_tray(bin: &Path) -> Result<(), String> {
    // M57：stdout/stderr 不能设 null，否则"点了没反应"时无从查起。
    let log = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .append(true)
        .open(log_path())
        .map_err(|e| format!("打开托盘日志失败：{e}"))?;
    let log_err = log
        .try_clone()
        .map_err(|e| format!("复制日志句柄失败：{e}"))?;
    let child = Command::new(bin)
        .arg("tray")
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(log_err)
        .spawn()
        .map_err(|e| format!("启动托盘失败：{e}"))?;
    // 先写 pid 再做存活探测，避免托盘还没来得及自己写（`tray.rs` 启动时
    // 也会写同一个文件，值是同一个 pid）就被误判为启动失败。
    std::fs::write(pid_path(), child.id().to_string())
        .map_err(|e| format!("写入托盘 pid 失败：{e}"))?;
    std::thread::sleep(START_PROBE);
    if !is_running() {
        let _ = std::fs::remove_file(pid_path());
        return Err(format!("托盘启动后立即退出，详见 {}", log_path().display()));
    }
    Ok(())
}

fn disable() -> Result<(), String> {
    let mut failure = None;
    if let Some(pid) = read_pid() {
        if process_alive(pid) {
            let _ = Command::new("kill").arg(pid.to_string()).status();
            let deadline = Instant::now() + STOP_TIMEOUT;
            while process_alive(pid) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(50));
            }
            if process_alive(pid) {
                failure = Some(format!(
                    "托盘进程 {pid} 未在 {}ms 内退出",
                    STOP_TIMEOUT.as_millis()
                ));
            }
        }
        let _ = std::fs::remove_file(pid_path());
    }
    remove_autostart()?;
    failure.map_or(Ok(()), Err)
}

fn write_autostart(bin: &Path) -> Result<(), String> {
    let Some(path) = autostart_path() else {
        // 既没有 XDG_CONFIG_HOME 也没有 HOME：只本次登录生效，不算失败。
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建自启目录失败：{e}"))?;
    }
    std::fs::write(&path, desktop_entry(bin)).map_err(|e| format!("写入托盘自启文件失败：{e}"))
}

fn remove_autostart() -> Result<(), String> {
    let Some(path) = autostart_path() else {
        return Ok(());
    };
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(format!("删除托盘自启文件失败：{err}")),
    }
}

/// XDG autostart 文件内容。
///
/// `Exec` 用引号包住：开发目录名里带空格，不引会被拆成两个参数。
#[must_use]
pub fn desktop_entry(bin: &Path) -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=频谱工坊（托盘）\n\
         Name[en]=Spectrum Atelier (Tray)\n\
         Comment=在系统托盘中启动或停止任务栏音频频谱\n\
         Comment[en]=Start or stop the taskbar audio spectrum from the tray\n\
         Exec=\"{}\" tray\n\
         Icon=deepin-spectrum\n\
         Terminal=false\n\
         X-GNOME-Autostart-enabled=true\n",
        bin.display()
    )
}

#[cfg(test)]
mod tests {
    use super::{desktop_entry, process_alive};
    use std::path::Path;

    #[test]
    fn autostart_entry_runs_tray_subcommand() {
        let text = desktop_entry(Path::new("/usr/bin/deepin-spectrum"));
        assert!(text.starts_with("[Desktop Entry]"));
        assert!(
            text.contains("Exec=\"/usr/bin/deepin-spectrum\" tray"),
            "自启文件必须启动 tray 子命令：{text}"
        );
        assert!(text.contains("X-GNOME-Autostart-enabled=true"));
    }

    #[test]
    fn test_binary_is_not_a_tray() {
        // 防止 process_alive 只看"进程存在"就返回 true。
        assert!(!process_alive(std::process::id()));
    }
}
