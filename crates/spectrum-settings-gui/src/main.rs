//! `deepin-spectrum-settings`：设置界面（Slint，GPL-3.0-only）。
//!
//! 用法：
//!     deepin-spectrum-settings [--config <文件>]
//!
//! 不带参数时使用默认路径 `~/.config/deepin-spectrum/config.toml`，
//! 与 daemon 读的是同一个文件 —— 改完 250ms 内热生效，不需要重启频谱。

use std::path::PathBuf;
use std::process::ExitCode;

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

    match spectrum_settings_gui::run(&config) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("deepin-spectrum-settings: {err}");
            ExitCode::FAILURE
        }
    }
}
