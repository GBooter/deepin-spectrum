//! `deepin-spectrum-settings`：与 `deepin-spectrum settings` 完全等价的独立入口。
//!
//! 存在的意义：`.desktop` 可以不经主二进制直接拉起设置（F29 只打包设置界面的
//! `.desktop`），以及在 daemon 起不来的机器上单独做配置校验。
//!
//! GUI 形态待 §11.1 许可决策（Slint `GPL-3.0-only` / GTK4 / 商业许可）落定后接入；
//! 届时这里换成一个真正的窗口，而**参数模型、热生效、原子写都由库层复用**。

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match spectrum_settings::cli::run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("deepin-spectrum-settings: {err}");
            ExitCode::FAILURE
        }
    }
}
