//! `spectrum-daemon` 库：主循环流水线与 doctor 输出。
//!
//! 二进制（`deepin-spectrum`）只管解析参数、装配实现；
//! **所有可测逻辑都在这里**，不依赖窗口系统与音频设备。
//!
//! 分层：
//!
//! - [`pipeline`]：PCM → 分析 → 渲染，纯逻辑、零 I/O，可离线单测
//! - [`instance`]：单实例保护（两个 daemon 会互相覆盖画面）
//! - [`reconnect`]：采集断流后的退避重连策略（纯逻辑，可离线单测）
//! - [`icon`]：应用图标（现场生成的 ARGB 频谱柱，托盘与桌面图标共用）
//! - `tray`：系统托盘（`StatusNotifierItem` + `DBusMenu`，需 `tray` feature）
//!
//! 采集（`spectrum_core::pulse`）与上屏（`spectrum_platform::x11`）在二进制里装配，
//! 这样"频谱算得对不对"不需要真实声卡与 X server 就能验证。

pub mod icon;
pub mod instance;
pub mod pipeline;
pub mod reconnect;
#[cfg(feature = "tray")]
pub mod tray;

pub use pipeline::{FrameOutcome, Pipeline, PipelineConfig};

/// 阶段标记，供 `doctor` 输出与回归脚本断言。
pub const DAEMON_STAGE: &str = "phase-3-wiring";
