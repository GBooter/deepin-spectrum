//! `spectrum-core`：纯逻辑层，不含任何窗口系统或音频系统依赖（§5.1）。
//!
//! 阶段 1 交付模块：
//!
//! - [`analyzer`]：FFT、`build_edges` 去重（F56）、dB 归一化、attack/release 平滑、峰值保持
//! - [`capture`]：静音门限（F24）与 ringbuf 0.6 消费端语义（F54）的纯逻辑模型
//! - [`config`]：配置模型、默认值、非法值回退与版本迁移
//! - [`dock_signal`]：D-Bus 任务栏信号 → 几何 / 重排决策的纯逻辑桥（零依赖）
//! - [`state`]：`Idle` / `Running` / `FadeOut` 显示状态机
//! - [`geometry`]：任务栏运行时几何跟踪（高度自适应、节流合并、非法值回退）
//! - `pulse`：`PulseAudio` monitor 采集流（**需 `capture-pulse` feature**），
//!   把「读满一个固定 hop」封装成可复用类型，供 `spectrum-daemon` 主循环调用
//!
//! ## 依赖策略
//!
//! 默认构建**零外部依赖**（内置 radix-2 FFT + 纯逻辑采集模型），便于离线 / CI 编译。
//! `fft-realfft` feature 切换到 `realfft 3.5.1`（数值等价、快约 11×）；
//! `capture-pulse` feature 接入真实 `libpulse-binding 2.30.1` 采集流。
//! 两个 feature 都在 `--all-features` 质量门下验证，上层 `Analyzer` 不感知后端差异。

// DSP 与几何换算中 `usize/u32/u64 <-> f32 <-> i32` 的窄化/精度转换是算法固有需求：
// 所有输入都先经 `AnalyzerConfig::validate` / `Config::validate` / `Rect::is_valid`
// 约束到有限安全区间，且 FFT 点数不超过 8192、采样率不超过 192000。
// 与其在几十处重复写局部 allow，不如在 crate 边界一次性声明并说明前提。
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
// golden-data 断言需要精确比较浮点常量；配置测试则刻意先构造默认值再逐字段破坏，
// 用来覆盖 §9.1 的「非法值回退」用例，两者都不适合改写成近似比较 / update 语法。
#![cfg_attr(test, allow(clippy::float_cmp, clippy::field_reassign_with_default))]

pub mod analyzer;
pub mod capture;
pub mod config;
pub mod dock_signal;
pub mod geometry;
#[cfg(feature = "capture-pulse")]
pub mod pulse;
pub mod state;

/// 阶段标记，供 `doctor` 输出与回归脚本断言。
pub const CORE_STAGE: &str = "phase-1";

/// 纯逻辑层的统一错误类型（含浮点字段，故只实现 `PartialEq`）。
#[derive(Debug, Clone, PartialEq)]
pub enum CoreError {
    /// FFT 点数必须是 >= 2 的 2 的幂。
    InvalidFftSize(usize),
    /// 窗函数长度必须与 FFT 点数一致。
    FftWindowMismatch {
        /// 请求的 FFT 点数。
        fft_size: usize,
        /// 实际传入的窗函数长度。
        window: usize,
    },
    /// 采样率必须大于 0。
    InvalidSampleRate(u32),
    /// 频段上下限必须为正有限值且 `f_min < f_max`。
    InvalidFrequencyRange {
        /// 配置的下限频率。
        f_min: f32,
        /// 配置的上限频率。
        f_max: f32,
    },
    /// 频段数量必须落在 `1..=128`。
    InvalidBandCount(usize),
    /// 峰值滑块每帧回落量必须落在 0.002~0.05。
    InvalidPeakFall(f32),
    /// 配置文件版本高于当前支持版本，无法向下兼容。
    UnsupportedConfigVersion {
        /// 文件中读到的版本。
        found: u32,
        /// 本二进制支持的最高版本。
        supported: u32,
    },
}

impl std::fmt::Display for CoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidFftSize(n) => write!(f, "非法 FFT 点数 {n}（须为 >= 2 的 2 的幂）"),
            Self::FftWindowMismatch { fft_size, window } => {
                write!(f, "窗函数长度 {window} 与 FFT 点数 {fft_size} 不一致")
            }
            Self::InvalidSampleRate(r) => write!(f, "非法采样率 {r}"),
            Self::InvalidFrequencyRange { f_min, f_max } => {
                write!(f, "非法频段范围 {f_min}..{f_max} Hz")
            }
            Self::InvalidBandCount(n) => write!(f, "非法频段数 {n}（须在 1..=128）"),
            Self::InvalidPeakFall(v) => write!(f, "非法峰值回落速度 {v}（须在 0.002~0.05）"),
            Self::UnsupportedConfigVersion { found, supported } => {
                write!(f, "配置版本 {found} 高于支持的 {supported}")
            }
        }
    }
}

impl std::error::Error for CoreError {}
