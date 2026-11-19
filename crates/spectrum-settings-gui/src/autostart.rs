//! 开机自启（systemd 用户服务）。
//!
//! ## 为什么走 systemd 而不是写配置文件
//!
//! 指导文档 §11.2 明确：设置界面只操作
//! `systemctl --user enable/disable --now deepin-spectrum`，
//! 守护进程由 systemd user unit **唯一**启动（F29）。
//! 因此配置里的 `general.autostart` 只是"意图记录"，真正生效的是 systemd 的
//! 软链接状态 —— 两者不一致时以 systemd 为准（它才是开机时真正会被拉起的那个）。
//!
//! ## 安全边界
//!
//! 本模块**只**操作 `deepin-spectrum.service` 这一个单元：
//! 不碰别的服务、不改全局设置、不写 `/etc`。

use std::process::Command;

/// systemd 用户单元名。
pub const UNIT: &str = "deepin-spectrum";

/// 解析 `systemctl is-enabled` 的输出。
///
/// `static` 表示“单元没有 Install 段，无法被 enable”，**不算已启用**；
/// 若把 static 当启用，界面会显示“已开启”，但开机根本不会拉起。
fn parse_state(text: &str) -> Result<bool, String> {
    match text {
        "enabled" | "enabled-runtime" => Ok(true),
        "disabled" | "not-found" | "masked" | "static" | "" => Ok(false),
        other => Err(format!("systemctl 返回了意外状态：{other}")),
    }
}

/// 查询当前是否已启用开机自启。
///
/// # Errors
///
/// `systemctl` 不可用、用户总线不可达等。
pub fn is_enabled() -> Result<bool, String> {
    let output = Command::new("systemctl")
        .args(["--user", "is-enabled", UNIT])
        .output()
        .map_err(|e| format!("调用 systemctl 失败：{e}"))?;
    let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    parse_state(&text)
}

/// 启用 / 禁用开机自启。
///
/// 用 `--now` 让开关**立刻**与运行状态对齐（文档 §11.2 的要求）。
///
/// **例外**：如果频谱已经在运行（例如用户刚用界面上的「启动频谱」直接
/// spawn 了一个进程），启用时只 `enable`、不再 `--now`。否则 systemd 会
/// 拉起第二个 daemon，被单实例保护拒绝后触发 `Restart=on-failure` 的反复重试。
///
/// # Errors
///
/// `systemctl` 调用失败或返回非零。
pub fn set_enabled(enabled: bool) -> Result<(), String> {
    let action = if enabled { "enable" } else { "disable" };
    // 只有"启用"且"已在运行"时才跳过 --now；禁用仍要让服务停下来。
    let skip_now = enabled && crate::daemonctl::is_running();
    let mut args = vec!["--user", action];
    if !skip_now {
        args.push("--now");
    }
    args.push(UNIT);
    let output = Command::new("systemctl")
        .args(&args)
        .output()
        .map_err(|e| format!("调用 systemctl 失败：{e}"))?;
    if output.status.success() {
        return Ok(());
    }
    let err = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    Err(if err.is_empty() {
        format!(
            "systemctl {action} 失败（退出码 {:?}）",
            output.status.code()
        )
    } else {
        err
    })
}

/// 单元是否已安装（用于在界面上提示"还没装 systemd 单元"）。
#[must_use]
pub fn unit_installed() -> bool {
    [
        "/usr/lib/systemd/user/deepin-spectrum.service",
        "/usr/share/systemd/user/deepin-spectrum.service",
        "/etc/systemd/user/deepin-spectrum.service",
    ]
    .iter()
    .any(|p| std::path::Path::new(p).is_file())
}

#[cfg(test)]
mod tests {
    use super::{UNIT, is_enabled, parse_state, set_enabled, unit_installed};

    #[test]
    fn static_unit_is_not_enabled() {
        // AUD-033：static 无法被开机拉起，不能显示为已启用。
        assert_eq!(parse_state("static"), Ok(false));
        assert_eq!(parse_state("enabled"), Ok(true));
        assert_eq!(parse_state("enabled-runtime"), Ok(true));
        assert_eq!(parse_state("disabled"), Ok(false));
        assert!(parse_state("something-else").is_err());
    }

    #[test]
    fn unit_name_is_stable() {
        assert_eq!(UNIT, "deepin-spectrum");
    }

    #[test]
    fn calls_do_not_panic() {
        // 这些命令在本机可能不存在（没有 systemd 用户会话），
        // 但**绝不能** panic —— 界面只是显示不出自启状态而已。
        let _ = is_enabled();
        let _ = set_enabled(true);
        let _ = unit_installed();
    }

    /// 回归门（用户反馈"开机启动设置没用"）：勾选必须真的落到 systemctl。
    /// 用临时目录里的 mock systemctl 记录调用，避免依赖真机 systemd 用户会话。
    /// PATH 是进程级资源，用全局互斥串行化，防止与 `calls_do_not_panic` 竞争。
    #[test]
    fn set_enabled_really_calls_systemctl() {
        use std::sync::{Mutex, OnceLock};
        static PATH_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        let _guard = PATH_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();

        let dir = std::env::temp_dir().join(format!(
            "spectrum-autostart-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("calls.log");
        let mock = dir.join("systemctl");
        std::fs::write(
            &mock,
            format!(
                "#!/usr/bin/env bash\necho \"$*\" >> {}\ncase \"$*\" in\n  *is-enabled*) echo enabled ;;\nesac\nexit 0\n",
                log.display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&mock, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let old_path = std::env::var_os("PATH");
        let mut new_path = std::ffi::OsString::from(dir.as_os_str());
        new_path.push(":");
        if let Some(p) = &old_path {
            new_path.push(p);
        }
        // SAFETY: 单线程测试进程内短暂改 PATH，互斥保护，测试结束立即还原；
        // Rust 2024 起 std::env::set_var 为 unsafe。
        unsafe { std::env::set_var("PATH", &new_path) };

        // 停机状态探测：daemon 未运行时 enable 不应带 --now（见 set_enabled 文档）。
        let result = set_enabled(true);
        assert!(result.is_ok(), "systemctl 可用时应成功：{result:?}");
        let calls = std::fs::read_to_string(&log).unwrap_or_default();

        if let Some(p) = old_path {
            // SAFETY: 同上，恢复原 PATH。
            unsafe { std::env::set_var("PATH", p) };
        } else {
            // SAFETY: 同上，原本无 PATH 则移除。
            unsafe { std::env::remove_var("PATH") };
        }
        let _ = std::fs::remove_dir_all(&dir);

        assert!(
            calls.contains("enable") && calls.contains(UNIT),
            "勾选开机自启必须调用 systemctl enable {UNIT}，实际记录：{calls}"
        );
    }
}
