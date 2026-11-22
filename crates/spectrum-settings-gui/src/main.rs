//! `deepin-spectrum-settings`：设置界面（Slint，GPL-3.0-only）。
//!
//! 用法：
//!     deepin-spectrum-settings [--config <文件>]
//!
//! 不带参数时使用默认路径 `~/.config/deepin-spectrum/config.toml`，
//! 与 daemon 读的是同一个文件 —— 改完 250ms 内热生效，不需要重启频谱。

use std::path::PathBuf;
use std::process::{Command, ExitCode, ExitStatus};

/// 渲染器回退后的子进程标记，防止自我递归重启。
const RENDERER_RETRY_ENV: &str = "DEEPIN_SPECTRUM_GUI_RENDERER_RETRY";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut config = spectrum_settings::store::default_path();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--config" => {
                let Some(value) = args.get(index + 1) else {
                    eprintln!("deepin-spectrum-settings: --config 需要文件路径");
                    return ExitCode::FAILURE;
                };
                config = PathBuf::from(value);
                index += 2;
            }
            "-h" | "--help" => {
                println!(
                    "deepin-spectrum-settings —— 任务栏音频频谱设置\n\n\
                     用法：deepin-spectrum-settings [--config <文件>]\n\
                     改完自动保存并立即生效（≤250ms），不需要重启频谱。\n"
                );
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("deepin-spectrum-settings: 未知参数 {other}");
                return ExitCode::FAILURE;
            }
        }
    }

    if let Some(code) = retry_with_software_renderer() {
        return code;
    }

    match spectrum_settings_gui::run(&config) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("deepin-spectrum-settings: {err}");
            ExitCode::FAILURE
        }
    }
}

/// 先按 femtovg（GPU）启动，失败时用软件渲染器重试一次。
///
/// UOS 20（Mesa 19.2 / llvmpipe）上 femtovg 走 GLX 建上下文会抛
/// `GLXBadContextTag`，winit 随后在设置窗口主题时 flush 请求直接 panic，
/// 桌面表现就是“点图标毫无反应”。femtovg 的失败发生在建窗阶段、无法在
/// 进程内捕获（panic 一旦发生就结束进程），因此这里用子进程试跑：
/// 正常退出（用户关窗）就直接返回；非正常退出则换软件渲染器重开。
/// `SLINT_BACKEND` 已被外部显式指定时尊重用户选择，不做干预。
fn retry_with_software_renderer() -> Option<ExitCode> {
    if std::env::var_os("SLINT_BACKEND").is_some()
        || std::env::var_os(RENDERER_RETRY_ENV).is_some()
    {
        return None;
    }
    if let Ok(status) = spawn_self_with("winit-femtovg")
        && status.success()
    {
        return Some(exit_code(status));
    }
    eprintln!("deepin-spectrum-settings: OpenGL 渲染不可用，改用软件渲染重试");
    Some(
        spawn_self_with("winit-software")
            .map(exit_code)
            .unwrap_or(ExitCode::FAILURE),
    )
}

/// 以指定 Slint 渲染器重新拉起自己（参数原样透传）。
fn spawn_self_with(renderer: &str) -> std::io::Result<ExitStatus> {
    let exe = std::env::current_exe()?;
    Command::new(exe)
        .args(std::env::args_os().skip(1))
        .env("SLINT_BACKEND", renderer)
        .env(RENDERER_RETRY_ENV, "1")
        .status()
}

/// 把子进程退出状态映射为退出码；被信号终止（如 SIGABRT）按失败处理。
fn exit_code(status: ExitStatus) -> ExitCode {
    status
        .code()
        .and_then(|code| u8::try_from(code).ok())
        .map_or(ExitCode::FAILURE, ExitCode::from)
}
