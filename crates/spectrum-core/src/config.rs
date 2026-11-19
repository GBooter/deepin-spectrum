//! 配置模型、默认值、非法值回退与版本迁移（§6.8）。
//!
//! 阶段 1 只提供**内存中的**模型与校验；TOML 反序列化（`toml 0.9`）与文件监听
//! 属于阶段 3，届时同样受会话 `utime` 限制，需在真机 `cargo fetch` 后接入。

use crate::CoreError;
use crate::analyzer::{AnalyzerConfig, MAX_BANDS, MAX_FFT_SIZE};

/// 当前配置 schema 版本。
///
/// v2（0.1.2）：把 v1 的默认 `managed-above` 迁移为 `override-redirect`，
/// 否则升级用户会继续受「显示桌面隐藏频谱」影响（F124）。
pub const CONFIG_VERSION: u32 = 2;

/// 显示区域模式（§6.8 `visual.mode`）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum VisualMode {
    /// 覆盖整条任务栏。
    #[default]
    FullDock,
    /// 仅左侧空白区。
    LeftBlank,
    /// 仅右侧空白区。
    RightBlank,
    /// 任务栏上方独立条带。
    AboveDock,
}

/// 混合模式（§6.2.5）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum BlendMode {
    /// 加法混合，让图标透出（`FullDock` 默认）。
    #[default]
    Additive,
    /// 预乘 alpha 混合。
    Alpha,
}

/// 多屏策略。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OutputMode {
    /// 仅主屏（推荐）。
    #[default]
    Primary,
    /// 全部屏幕。
    All,
    /// 按 [`Outputs::names`] 指定。
    Names,
}

/// X11 堆叠策略（F28）；三者互斥，由枚举在编译期保证。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum X11Stacking {
    /// 覆盖重定向窗口（绕过 WM）。
    ///
    /// 为什么默认是它而不是文档推荐的 ManagedAbove（F124，用户 2026-11-15 真机反馈）：
    /// 受管窗口会被「显示桌面」/ 最小化隐藏；OR 窗口完全绕过 WM，
    /// 频谱不会因此消失。代价：不设 `_NET_WM_*`（F28 互斥），需定期 raise。
    #[default]
    OverrideRedirect,
    /// 受管窗口 + DOCK 类型 + ABOVE。
    ManagedAbove,
    /// 受管窗口 + `_NET_WM_STATE_BELOW`，实验性。
    ManagedBelow,
}

/// `[general]` 段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct General {
    /// 总开关。
    pub enabled: bool,
    /// 开机自启（systemd user unit 为唯一入口，F29）。
    pub autostart: bool,
    /// 目标帧率；`0` = 跟随屏幕刷新率（§13 决策 4 推荐默认）。
    pub fps: u32,
    /// 淡出时长（毫秒）；减弱动画时强制为 0。
    pub fade_ms: u32,
    /// 是否尊重系统「减弱动画」设置（F39）。
    pub respect_reduced_motion: bool,
}

/// `[visual]` 段。
#[derive(Debug, Clone, PartialEq)]
pub struct Visual {
    /// 显示区域。
    pub mode: VisualMode,
    /// 整体不透明度 0.05~1.0。
    pub opacity: f32,
    /// 频段数上限 16~128（实际柱数由 `build_edges` 决定）。
    pub bands: usize,
    /// `AboveDock` 模式下的最大柱高。
    pub bar_max_height: u32,
    /// 柱间距占柱宽比例。
    pub bar_gap_ratio: f32,
    /// 起始色 `#RRGGBB`。
    pub color_start: String,
    /// 结束色 `#RRGGBB`。
    pub color_end: String,
    /// 多色标色板：逗号分隔的 3~8 个 `#RRGGBB`；空 = 双色渐变（v0.4.0 行为）。
    pub palette: String,
    /// 铺色方式：`off` / `across` / `per_bar` / `vertical`。
    pub palette_mode: String,
    /// 混合模式。
    pub blend: BlendMode,
    /// 是否绘制峰值帽。
    pub peak_cap: bool,
    /// 是否圆角。
    pub rounded: bool,
}

/// `[behavior]` 段。
// 这些是彼此独立的用户开关，不是状态机的互斥状态位，拆成枚举反而会丢失语义。
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq)]
pub struct Behavior {
    /// 静音超时（毫秒）。
    pub silence_timeout_ms: u64,
    /// 静音门限（dBFS）。
    pub silence_dbfs: f32,
    /// 全屏时隐藏。
    pub hide_on_fullscreen: bool,
    /// 任务栏自动隐藏时隐藏。
    pub hide_on_autohide: bool,
    /// 锁屏/屏保时隐藏。
    pub hide_on_lock: bool,
    /// 电池模式降到 30fps。
    pub pause_on_battery_30fps: bool,
}

/// `[dock]` 段。
///
/// ★ `height` 只是 **D-Bus 探测失败时的兜底值**（F38）。用户随时可能调整任务栏
/// 高度，运行时必须以 `FrontendWindowRect` / `geometry` 为准，见 [`crate::geometry`]。
#[derive(Debug, Clone, PartialEq)]
pub struct Dock {
    /// 停靠位置（字符串，避免与 F131 未核验的枚举映射绑死）。
    pub position: String,
    /// 兜底高度（逻辑像素），默认 45。
    pub height: u32,
    /// 是否居中模式。
    pub center_mode: bool,
    /// 空白区宽度；0 = 自动估算。
    pub blank_width_px: u32,
    /// D-Bus 枚举失败时的图标数兜底（F52）。
    pub icon_count_hint: u32,
    /// 图标尺寸（逻辑像素）。
    pub icon_size: f32,
}

/// `[outputs]` 段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outputs {
    /// 多屏策略。
    pub mode: OutputMode,
    /// `mode == Names` 时指定的输出名列表。
    pub names: Vec<String>,
}

/// `[advanced]` 段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Advanced {
    /// 平台后端选择。
    pub backend: String,
    /// 渲染器选择。
    pub renderer: String,
    /// X11 堆叠策略。
    pub x11_stacking: X11Stacking,
}

/// 完整配置。
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// schema 版本。
    pub version: u32,
    /// 通用设置。
    pub general: General,
    /// 视觉设置。
    pub visual: Visual,
    /// 分析参数。
    pub analyze: AnalyzerConfig,
    /// 行为开关。
    pub behavior: Behavior,
    /// 任务栏相关（兜底值）。
    pub dock: Dock,
    /// 多屏设置。
    pub outputs: Outputs,
    /// 高级设置。
    pub advanced: Advanced,
}

/// 一次校验中被回退的字段，供日志与设置界面提示。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigWarning {
    /// 字段被钳制到合法区间。
    Clamped {
        /// 字段路径，如 `visual.opacity`。
        field: &'static str,
        /// 回退后采用的值（格式化文本）。
        applied: String,
    },
    /// 非法枚举/字符串被替换为默认值（含潜在注入字符）。
    Replaced {
        /// 字段路径。
        field: &'static str,
        /// 原因。
        reason: &'static str,
    },
    /// 值本身合法，但本版本未实现；保留原值，运行时按说明降级。
    Unsupported {
        /// 字段路径。
        field: &'static str,
        /// 未实现说明。
        reason: &'static str,
    },
}

impl Default for General {
    fn default() -> Self {
        Self {
            enabled: true,
            autostart: true,
            fps: 0,
            fade_ms: 150,
            respect_reduced_motion: true,
        }
    }
}

impl Default for Visual {
    fn default() -> Self {
        Self {
            mode: VisualMode::FullDock,
            opacity: 0.65,
            bands: 64,
            bar_max_height: 120,
            bar_gap_ratio: 0.2,
            color_start: "#00C8FF".to_owned(),
            color_end: "#7B61FF".to_owned(),
            palette: String::new(),
            palette_mode: "off".to_owned(),
            blend: BlendMode::Additive,
            peak_cap: true,
            rounded: true,
        }
    }
}

impl Default for Behavior {
    fn default() -> Self {
        Self {
            silence_timeout_ms: 800,
            silence_dbfs: -62.0,
            hide_on_fullscreen: true,
            hide_on_autohide: true,
            hide_on_lock: true,
            pause_on_battery_30fps: true,
        }
    }
}

impl Default for Dock {
    fn default() -> Self {
        Self {
            position: "bottom".to_owned(),
            height: 45,
            center_mode: true,
            blank_width_px: 0,
            icon_count_hint: 10,
            icon_size: 32.0,
        }
    }
}

impl Default for Outputs {
    fn default() -> Self {
        Self {
            mode: OutputMode::Primary,
            names: Vec::new(),
        }
    }
}

impl Default for Advanced {
    fn default() -> Self {
        Self {
            backend: "auto".to_owned(),
            renderer: "auto".to_owned(),
            // F124：默认 override-redirect（绕过 WM，「显示桌面」不再隐藏频谱）
            x11_stacking: X11Stacking::OverrideRedirect,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            general: General::default(),
            visual: Visual::default(),
            analyze: AnalyzerConfig::default(),
            behavior: Behavior::default(),
            dock: Dock::default(),
            outputs: Outputs::default(),
            advanced: Advanced::default(),
        }
    }
}

/// 只接受 `#` + 6 位十六进制字符，拒绝注入字符与超长串。
/// 解析 `visual.palette`：3~8 个逗号分隔的 `#RRGGBB`。
///
/// 返回定长数组与有效色标数；格式不合法时返回 `0`，调用方应降级为双色渐变。
/// 返回值不含 `spectrum_render` 的类型，`core` 因此不必依赖渲染 crate。
#[must_use]
pub fn parse_palette(s: &str) -> ([u32; 8], u8) {
    let mut out = [0_u32; 8];
    let mut count = 0_usize;
    for part in s.split(',') {
        let value = part.trim();
        if count >= 8 || !is_hex_color(value) {
            return ([0; 8], 0);
        }
        out[count] = u32::from_str_radix(&value[1..], 16).unwrap_or(0);
        count += 1;
    }
    if (3..=8).contains(&count) {
        (out, count as u8)
    } else {
        ([0; 8], 0)
    }
}

/// `visual.palette_mode` 的合法取值。
#[must_use]
pub fn is_palette_mode(s: &str) -> bool {
    matches!(s, "off" | "across" | "per_bar" | "vertical")
}

fn is_hex_color(s: &str) -> bool {
    s.len() == 7 && s.starts_with('#') && s[1..].bytes().all(|b| b.is_ascii_hexdigit())
}

fn clamp_f32(
    value: &mut f32,
    lo: f32,
    hi: f32,
    field: &'static str,
    warnings: &mut Vec<ConfigWarning>,
) {
    let before = *value;
    *value = if before.is_finite() {
        before.clamp(lo, hi)
    } else {
        lo
    };
    if (*value - before).abs() > f32::EPSILON || !before.is_finite() {
        warnings.push(ConfigWarning::Clamped {
            field,
            applied: format!("{:.4}", *value),
        });
    }
}

impl Config {
    /// 从「空文件」语义构造：等价于全默认配置。
    #[must_use]
    pub fn from_empty() -> Self {
        Self::default()
    }

    /// 版本迁移；高于当前版本直接报错，低于当前版本逐版补齐默认字段。
    pub fn migrate_version(&mut self) -> Result<(), CoreError> {
        if self.version > CONFIG_VERSION {
            return Err(CoreError::UnsupportedConfigVersion {
                found: self.version,
                supported: CONFIG_VERSION,
            });
        }
        if self.version < CONFIG_VERSION {
            let defaults = Self::default();
            // v1 → v2：0.1.0 默认写出的 `managed-above` 会被 WM 的
            // 「显示桌面」/最小化隐藏。0.1.2 起统一迁移到 OR；
            // 用户显式选过的 `managed-below` 属于实验性配置，保持不动。
            if self.version <= 1 && self.advanced.x11_stacking == X11Stacking::ManagedAbove {
                self.advanced.x11_stacking = X11Stacking::OverrideRedirect;
            }
            self.version = CONFIG_VERSION;
            // v0 → v1：补齐 v1 引入的字段，保留用户已设置的值
            if self.general.fps != 0 && self.general.fps != 30 && self.general.fps != 60 {
                self.general.fps = defaults.general.fps;
            }
            if self.analyze.validate().is_err() {
                self.analyze = defaults.analyze;
            }
        }
        Ok(())
    }

    /// 原地钳制非法值并回退非法字符串，返回告警列表（§9.1 config 用例）。
    // 配置校验本质是一张扁平的字段→规则表；按段拆函数会让「一处改字段、两处改校验」，
    // 这里保留单一入口，确保 §9.1 的非法值/注入/超大值用例能一处审完。
    #[allow(clippy::too_many_lines)]
    pub fn validate(&mut self) -> Vec<ConfigWarning> {
        let mut w = Vec::new();

        // general
        if !matches!(self.general.fps, 0 | 30 | 60 | 90) {
            self.general.fps = 0;
            w.push(ConfigWarning::Replaced {
                field: "general.fps",
                reason: "只允许 0/30/60/90，回退为 0（跟随刷新率）",
            });
        }
        if self.general.fade_ms > 1_000 {
            self.general.fade_ms = 150;
            w.push(ConfigWarning::Clamped {
                field: "general.fade_ms",
                applied: "150".to_owned(),
            });
        }

        // visual
        clamp_f32(
            &mut self.visual.opacity,
            0.05,
            1.0,
            "visual.opacity",
            &mut w,
        );
        clamp_f32(
            &mut self.visual.bar_gap_ratio,
            0.0,
            0.9,
            "visual.bar_gap_ratio",
            &mut w,
        );
        if !(16..=MAX_BANDS).contains(&self.visual.bands) {
            self.visual.bands = 64;
            w.push(ConfigWarning::Clamped {
                field: "visual.bands",
                applied: "64".to_owned(),
            });
        }
        if self.visual.bar_max_height > 1_000 {
            self.visual.bar_max_height = 120;
            w.push(ConfigWarning::Clamped {
                field: "visual.bar_max_height",
                applied: "120".to_owned(),
            });
        }
        // LeftBlank/RightBlank 还没有空白区几何模型：保留用户值，但明确告警，
        // 绝不静默按 FullDock 运行而不告诉用户。
        if matches!(
            self.visual.mode,
            VisualMode::LeftBlank | VisualMode::RightBlank
        ) {
            w.push(ConfigWarning::Unsupported {
                field: "visual.mode",
                reason: "LeftBlank/RightBlank 本版本未实现，运行时按 FullDock 绘制",
            });
        }
        // 多色标：单个非法色标会让整条色板作废并回退双色渐变，
        // 绝不留下"半套颜色"这种难以理解的中间状态。
        if !self.visual.palette.is_empty() && parse_palette(&self.visual.palette).1 == 0 {
            self.visual.palette.clear();
            w.push(ConfigWarning::Replaced {
                field: "visual.palette",
                reason: "多色标格式非法（须为 3~8 个逗号分隔的 #RRGGBB），已回退双色渐变",
            });
        }
        if !is_palette_mode(&self.visual.palette_mode) {
            "off".clone_into(&mut self.visual.palette_mode);
            w.push(ConfigWarning::Replaced {
                field: "visual.palette_mode",
                reason: "铺色方式只支持 off/across/per_bar/vertical，已回退 off",
            });
        }
        for (name, value, fallback) in [
            (
                "visual.color_start",
                &mut self.visual.color_start,
                "#00C8FF",
            ),
            ("visual.color_end", &mut self.visual.color_end, "#7B61FF"),
        ] {
            if !is_hex_color(value) {
                fallback.clone_into(value);
                w.push(ConfigWarning::Replaced {
                    field: name,
                    reason: "颜色值非法或含注入字符，回退默认色",
                });
            }
        }

        // analyze
        // P1 新键先单独钳制：否则一个越界值会让 analyze 整段回退默认，
        // 把用户同时改过的灵敏度/起音/衰减一起擦掉。
        clamp_f32(
            &mut self.analyze.peak_fall,
            0.002,
            0.05,
            "analyze.peak_fall",
            &mut w,
        );
        // fft_size 超上限先钳制（保留其他 analyze 参数），而不是整段回退默认值。
        if self.analyze.fft_size > MAX_FFT_SIZE {
            self.analyze.fft_size = MAX_FFT_SIZE;
            w.push(ConfigWarning::Clamped {
                field: "analyze.fft_size",
                applied: MAX_FFT_SIZE.to_string(),
            });
        }
        if self.analyze.validate().is_err() {
            self.analyze = AnalyzerConfig::default();
            w.push(ConfigWarning::Replaced {
                field: "analyze",
                reason: "分析参数非法，整段回退默认值",
            });
        }
        clamp_f32(&mut self.analyze.attack, 0.0, 1.0, "analyze.attack", &mut w);
        clamp_f32(
            &mut self.analyze.release,
            0.0,
            1.0,
            "analyze.release",
            &mut w,
        );
        clamp_f32(
            &mut self.analyze.sensitivity,
            0.2,
            3.0,
            "analyze.sensitivity",
            &mut w,
        );
        clamp_f32(&mut self.analyze.gamma, 0.1, 3.0, "analyze.gamma", &mut w);
        if self.analyze.db_floor >= 0.0 || !self.analyze.db_floor.is_finite() {
            self.analyze.db_floor = -80.0;
            w.push(ConfigWarning::Clamped {
                field: "analyze.db_floor",
                applied: "-80".to_owned(),
            });
        } else if self.analyze.db_floor < -120.0 {
            // 设置界面的滑杆范围是 [-120, -1]，这里保持一致：
            // 否则 GUI 会把配置里合法的 -150 静默夹成 -120 再写回。
            self.analyze.db_floor = -120.0;
            w.push(ConfigWarning::Clamped {
                field: "analyze.db_floor",
                applied: "-120".to_owned(),
            });
        }
        if self.visual.bands != self.analyze.bands {
            self.analyze.bands = self.visual.bands;
        }

        // behavior
        clamp_f32(
            &mut self.behavior.silence_dbfs,
            -100.0,
            -1.0,
            "behavior.silence_dbfs",
            &mut w,
        );
        if self.behavior.silence_timeout_ms > 60_000 {
            self.behavior.silence_timeout_ms = 800;
            w.push(ConfigWarning::Clamped {
                field: "behavior.silence_timeout_ms",
                applied: "800".to_owned(),
            });
        }

        // dock：用户可随时调整任务栏高度，这里只兜住明显越界的值
        if !(20..=400).contains(&self.dock.height) {
            self.dock.height = 45;
            w.push(ConfigWarning::Clamped {
                field: "dock.height",
                applied: "45".to_owned(),
            });
        }
        clamp_f32(
            &mut self.dock.icon_size,
            8.0,
            128.0,
            "dock.icon_size",
            &mut w,
        );
        if self.dock.icon_count_hint > 512 {
            self.dock.icon_count_hint = 10;
            w.push(ConfigWarning::Clamped {
                field: "dock.icon_count_hint",
                applied: "10".to_owned(),
            });
        }

        // advanced：白名单，拒绝注入
        for (name, value, allowed) in [
            (
                "advanced.backend",
                &mut self.advanced.backend,
                ["auto", "x11", "wayland"],
            ),
            (
                "advanced.renderer",
                &mut self.advanced.renderer,
                ["auto", "gl", "shm"],
            ),
        ] {
            if !allowed.iter().any(|a| value == a) {
                "auto".clone_into(value);
                w.push(ConfigWarning::Replaced {
                    field: name,
                    reason: "不在允许列表内，回退 auto",
                });
            }
        }
        if !["top", "bottom", "left", "right"].contains(&self.dock.position.as_str()) {
            "bottom".clone_into(&mut self.dock.position);
            w.push(ConfigWarning::Replaced {
                field: "dock.position",
                reason: "未知停靠位置，回退 bottom",
            });
        }
        if self.advanced.x11_stacking == X11Stacking::ManagedBelow {
            // 值本身合法（枚举保证互斥），但本版本没有实现 BELOW 堆叠：
            // 运行时明确降级为 override-redirect，而不是假装配置生效。
            w.push(ConfigWarning::Unsupported {
                field: "advanced.x11_stacking",
                reason: "managed-below 未实现，运行时按 override-redirect 处理",
            });
        }
        self.outputs.names.retain(|n| {
            n.len() <= 128
                && n.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        });

        w
    }

    /// 迁移 + 校验的组合入口。
    pub fn normalize(&mut self) -> Result<Vec<ConfigWarning>, CoreError> {
        self.migrate_version()?;
        Ok(self.validate())
    }
}

#[cfg(test)]
mod tests {
    use super::{CONFIG_VERSION, Config, ConfigWarning, VisualMode, X11Stacking, is_hex_color};

    #[test]
    fn defaults_match_document() {
        let c = Config::default();
        assert_eq!(c.version, CONFIG_VERSION);
        assert_eq!(c.general.fps, 0, "§13 决策 4：跟随刷新率");
        assert_eq!(c.general.fade_ms, 150);
        assert!((c.visual.opacity - 0.65).abs() < 1e-9);
        assert_eq!(c.visual.bands, 64);
        assert_eq!(c.analyze.fft_size, 4096);
        assert!((c.analyze.f_min - 40.0).abs() < 1e-9);
        assert!((c.behavior.silence_dbfs + 62.0).abs() < 1e-9);
        assert_eq!(c.dock.height, 45, "F38：运行时 45，不是 DConfig 的 48");
    }

    #[test]
    fn default_config_validates_cleanly() {
        let mut c = Config::default();
        assert!(c.normalize().expect("迁移失败").is_empty());
    }

    #[test]
    fn empty_file_is_all_defaults() {
        assert_eq!(Config::from_empty(), Config::default());
    }

    #[test]
    fn illegal_values_are_clamped() {
        let mut c = Config::default();
        c.visual.opacity = 99.0;
        c.visual.bands = 5;
        c.analyze.fft_size = 3000;
        c.analyze.sensitivity = 100.0;
        c.behavior.silence_dbfs = 10.0;
        c.general.fps = 144;
        c.dock.height = 9_999;
        let w = c.normalize().expect("迁移失败");
        assert_eq!(c.general.fps, 0);
        assert_eq!(c.visual.bands, 64);
        assert_eq!(c.analyze.fft_size, 4096);
        assert_eq!(c.dock.height, 45);
        assert!((c.visual.opacity - 1.0).abs() < 1e-9);
        assert!(
            (c.analyze.sensitivity - 1.0).abs() < 1e-9,
            "fft_size 非法导致 analyze 整段回退默认，sensitivity 也回到 1.0"
        );
        assert!(w.iter().any(
            |x| matches!(x, ConfigWarning::Clamped { field, .. } if *field == "visual.opacity")
        ));
        assert!(
            w.iter()
                .any(|x| matches!(x, ConfigWarning::Replaced { field, .. } if *field == "analyze"))
        );
    }

    #[test]
    fn oversized_fft_is_clamped_without_resetting_analyze() {
        let mut c = Config::default();
        c.analyze.fft_size = 65_536;
        c.analyze.sensitivity = 2.5;
        let warnings = c.normalize().expect("迁移失败");
        assert_eq!(
            c.analyze.fft_size,
            crate::analyzer::MAX_FFT_SIZE,
            "超大 fft_size 应钳制到上限"
        );
        assert!(
            (c.analyze.sensitivity - 2.5).abs() < 1e-9,
            "钳制 fft_size 不应把其他 analyze 参数一起重置"
        );
        assert!(warnings.iter().any(
            |x| matches!(x, ConfigWarning::Clamped { field, .. } if *field == "analyze.fft_size")
        ));
    }

    #[test]
    fn db_floor_below_gui_slider_range_is_clamped() {
        // 设置界面的滑杆是 [-120, -1]；配置层必须给出同样的边界，
        // 否则 GUI 的 fill → collect 会静默改写用户的合法值。
        let mut c = Config::default();
        c.analyze.db_floor = -150.0;
        let warnings = c.normalize().expect("迁移失败");
        assert!((c.analyze.db_floor + 120.0).abs() < 1e-9);
        assert!(warnings.iter().any(
            |x| matches!(x, ConfigWarning::Clamped { field, .. } if *field == "analyze.db_floor")
        ));

        // 边界内的值原样保留，不应产生告警。
        c.analyze.db_floor = -120.0;
        assert!(c.normalize().expect("迁移失败").is_empty());
        c.analyze.db_floor = -1.0;
        assert!(c.normalize().expect("迁移失败").is_empty());
    }

    #[test]
    fn unimplemented_visual_mode_is_preserved_with_warning() {
        let mut c = Config::default();
        c.visual.mode = VisualMode::LeftBlank;
        let warnings = c.normalize().expect("迁移失败");
        assert_eq!(
            c.visual.mode,
            VisualMode::LeftBlank,
            "未实现的模式不能被静默改写"
        );
        assert!(
            warnings.iter().any(|w| matches!(
                w,
                ConfigWarning::Unsupported { field, .. } if *field == "visual.mode"
            )),
            "应明确告警而不是静默失效：{warnings:?}"
        );
    }

    #[test]
    fn injection_strings_are_rejected() {
        assert!(is_hex_color("#00C8FF"));
        assert!(!is_hex_color("#00C8FF\"; rm -rf /"));
        assert!(!is_hex_color("red"));
        let mut c = Config::default();
        c.visual.color_start = "#fff\"; os.system('x')".to_owned();
        c.advanced.backend = "x11; curl evil".to_owned();
        c.dock.position = "bottom' OR 1=1".to_owned();
        c.outputs.names = vec!["ok-name".to_owned(), "bad/../../etc".to_owned()];
        c.normalize().expect("迁移失败");
        assert_eq!(c.visual.color_start, "#00C8FF");
        assert_eq!(c.advanced.backend, "auto");
        assert_eq!(c.dock.position, "bottom");
        assert_eq!(c.outputs.names, vec!["ok-name".to_owned()]);
    }

    #[test]
    fn illegal_peak_fall_is_clamped_without_resetting_analyze() {
        // 只钳制这一个键，不能把用户同时改过的灵敏度一起擦回默认。
        let mut c = Config::default();
        c.analyze.peak_fall = 99.0;
        c.analyze.sensitivity = 2.5;
        let warnings = c.normalize().expect("迁移失败");
        assert!((c.analyze.peak_fall - 0.05).abs() < 1e-9);
        assert!(
            (c.analyze.sensitivity - 2.5).abs() < 1e-9,
            "钳制 peak_fall 不应擦掉其他 analyze 参数"
        );
        assert!(warnings.iter().any(
            |x| matches!(x, ConfigWarning::Clamped { field, .. } if *field == "analyze.peak_fall")
        ));
    }

    #[test]
    fn version_migration_and_future_version() {
        let mut old = Config::default();
        old.version = 0;
        old.general.fps = 200;
        old.normalize().expect("v0 → v1 迁移失败");
        assert_eq!(old.version, CONFIG_VERSION);
        assert_eq!(old.general.fps, 0);

        let mut future = Config::default();
        future.version = 999;
        assert!(future.normalize().is_err());
    }

    #[test]
    fn v1_managed_above_migrates_to_override_redirect() {
        // 0.1.0 写出的默认值；不迁移的话「显示桌面」会继续隐藏频谱（F124）
        let mut old = Config::default();
        old.version = 1;
        old.advanced.x11_stacking = X11Stacking::ManagedAbove;
        old.normalize().expect("v1 → v2 迁移失败");
        assert_eq!(old.version, CONFIG_VERSION);
        assert_eq!(
            old.advanced.x11_stacking,
            X11Stacking::OverrideRedirect,
            "v1 的 managed-above 必须迁移为 override-redirect"
        );
    }

    #[test]
    fn v1_managed_below_is_preserved() {
        // 显式选择的实验配置不应被迁移覆盖
        let mut old = Config::default();
        old.version = 1;
        old.advanced.x11_stacking = X11Stacking::ManagedBelow;
        old.normalize().expect("v1 → v2 迁移失败");
        assert_eq!(old.advanced.x11_stacking, X11Stacking::ManagedBelow);
    }

    #[test]
    fn user_adjustable_dock_heights_all_accepted() {
        // 用户随时会改任务栏高度；合法范围内不得回退
        for h in [24, 40, 45, 48, 60, 72, 120, 200] {
            let mut c = Config::default();
            c.dock.height = h;
            let w = c.normalize().expect("迁移失败");
            assert_eq!(c.dock.height, h, "高度 {h} 不应被回退");
            assert!(!w.iter().any(
                |x| matches!(x, ConfigWarning::Clamped { field, .. } if *field == "dock.height")
            ));
        }
    }

    #[test]
    fn visual_bands_drives_analyzer_bands() {
        let mut c = Config::default();
        c.visual.bands = 96;
        c.normalize().expect("迁移失败");
        assert_eq!(c.analyze.bands, 96);
    }

    #[test]
    fn x11_stacking_variants_are_mutually_exclusive() {
        assert_ne!(X11Stacking::ManagedAbove, X11Stacking::ManagedBelow);
        assert_ne!(X11Stacking::ManagedBelow, X11Stacking::OverrideRedirect);
        assert_ne!(X11Stacking::ManagedAbove, X11Stacking::OverrideRedirect);
    }
}
