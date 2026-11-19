//! `spectrum-settings`：配置模型的文件化 + 热重载 + 命令行入口。
//!
//! ## 分层
//!
//! | 模块 | 职责 |
//! |---|---|
//! | [`toml`] | 零依赖 TOML 子集编解码（严格、带行号、截断不 panic） |
//! | [`schema`] | 文档 ↔ [`spectrum_core::config::Config`]，并登记**每个键的生效范围** |
//! | [`apply`] | 配置 → 运行时参数（样式/分析器/静音/开关），以及两次配置的差分 |
//! | [`watch`] | 250ms 轮询（按内容哈希判变化） |
//! | [`store`] | 加载 / 原子写 / 坏文件回退 / 差分应用 |
//! | [`writer`] | 合并写盘（滑块连续改动只落盘一次） |
//! | [`cli`] | `deepin-spectrum settings` 的实现 |
//! | [`presets`] | 20 套内置频谱预置 + 用户自定义方案的命名存储 |
//!
//! ## 设计取向（与指导文档的两处有意偏差）
//!
//! 1. **不引 `toml`/`serde`/`notify`**：保持默认构建零外部依赖，理由见 [`toml`] 与 [`watch`]；
//! 2. **不引 `rustix::fs::flock`**：当前只有设置界面一个写者，daemon 只读，
//!    且写是原子改名，暂无多写者问题；等第二个写者出现再加（见 [`store`] 文档）。
//!
//! 两处偏差都在 `docs/设置与热重载设计.md` 里连同替代方案与放弃的能力一起记录了。
//!
//! ## 热重载的语义（用户视角）
//!
//! > 改 `~/.config/deepin-spectrum/config.toml` 并保存 → 最迟 250ms 后画面变化，
//! > 不重启、不闪窗、不中断采集。
//!
//! 内部保证：
//!
//! - 只重建必须重建的东西（配色改动不碰分析器，柱数改动不碰窗口）；
//! - 文件写坏时**保留上一次的好配置**，并在日志里说清楚哪一行错了；
//! - 同一条错误只报一次（编辑器保存一半会连读两三次）。

// 配置值全部经 `Config::validate` 钳制到有限安全区间（opacity 0.05~1.0、
// bands 16~128、fft_size 为 2 的幂），此后的 f32↔u8/u16 换算不会溢出。
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
// 配置测试刻意要精确比较浮点常量（钳制前后必须逐位相等），不适合改成近似比较。
#![cfg_attr(test, allow(clippy::float_cmp))]

pub mod apply;
pub mod cli;
pub mod presets;
pub mod schema;
pub mod store;
pub mod toml;
pub mod watch;
pub mod writer;

pub use apply::{ApplyPlan, Delta, diff, plan};
pub use schema::{Scope, Warning};
pub use store::{ConfigStore, Reload, Source};

/// 阶段标记，供 `doctor` 与证据脚本断言。
pub const SETTINGS_STAGE: &str = "phase-4-config-hot-reload";
