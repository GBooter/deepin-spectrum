//! 配置 → 运行时参数的映射，以及两次配置之间的**差分**。
//!
//! 热重载的关键不是"重新读一遍文件"，而是**只做必须做的那部分**：
//!
//! - 只改配色/透明度 → 下一帧重新渲染即可（窗口、采集、分析器全都不动）
//! - 改柱数 / FFT 点数 / 平滑系数 → 必须重建分析器（一次 `Analyzer::new`，
//!   在 3200px 画布上是微秒级，不需要停采集、也不需要重建窗口）
//! - 改总开关 → 立刻清一帧屏再停止上屏（否则最后一帧会冻在屏幕上，M44 同族）
//!
//! [`diff`] 把"改了什么"翻译成键名列表，日志与证据脚本都靠它断言。

use spectrum_core::analyzer::AnalyzerConfig;
use spectrum_core::config::{Config, VisualMode};
use spectrum_render::{PaletteMode, RenderStyle};

/// 一次配置变更实际需要应用的运行时参数。
#[allow(clippy::struct_excessive_bools)] // 开关彼此独立，不是互斥状态
#[derive(Debug, Clone, PartialEq)]
pub struct ApplyPlan {
    /// 总开关（`general.enabled`）。
    pub enabled: bool,
    /// 全屏播放视频时是否隐藏（`behavior.hide_on_fullscreen`）。
    pub hide_on_fullscreen: bool,
    /// 锁屏/屏保时是否隐藏（`behavior.hide_on_lock`）。
    pub hide_on_lock: bool,
    /// 任务栏自动隐藏时是否隐藏（`behavior.hide_on_autohide`）。
    pub hide_on_autohide: bool,
    /// 分析器参数。
    pub analyzer: AnalyzerConfig,
    /// 渲染样式。
    pub style: RenderStyle,
    /// 静音门限（dBFS）。
    pub silence_dbfs: f32,
    /// 静音超时（毫秒）。
    pub silence_timeout_ms: u64,
}

/// 解析 `#RRGGBB`；非法值回退（`Config::validate` 已保证合法，这里只做防御）。
fn color_of(s: &str, fallback: u32) -> u32 {
    let bytes = s.as_bytes();
    if bytes.len() == 7 && bytes[0] == b'#' {
        u32::from_str_radix(&s[1..], 16).unwrap_or(fallback)
    } else {
        fallback
    }
}

/// 0.05~1.0 的不透明度 → 0~255 的 alpha。
fn alpha_of(opacity: f32) -> u8 {
    let scaled = (opacity.clamp(0.0, 1.0) * 255.0).round();
    // clamp 后再转，值一定落在 0..=255，不会截断出意外值。
    scaled.clamp(0.0, 255.0) as u8
}

/// 柱间隙比例 → 像素。
///
/// 渲染层只有"绝对像素间隙"这一种表达（[`RenderStyle::gap_px`]），
/// 而配置层是比例。这里用 `round(ratio × 10)` 换算，并**故意**让它满足
/// `0.2 → 2px`：这正好等于 [`RenderStyle::new`] 的默认值，
/// 于是"引入配置系统"不会改变任何已有画面的像素（阶段 2 的全部像素证据继续有效）。
fn gap_of(ratio: f32) -> u16 {
    let px = (ratio.clamp(0.0, 0.9) * 10.0).round();
    px.clamp(0.0, 8.0) as u16
}

/// 柱底渐隐高度（物理像素）。
///
/// 生产路径统一启用：柱底 3px 的 alpha 线性递减，消除半透明柱底压在
/// 深色任务栏上被合成成近黑硬边、间隙处呈密集亮点的问题。
/// 这是渲染层的观感修复，不引入新的配置项（不影响配置 schema 与 diff）。
pub const BOTTOM_FADE_PX: u8 = 3;

/// 「任务栏上方」模式下遮盖任务栏接缝的行数（物理像素）。
///
/// 任务栏自己会在顶端画一条深色边线（浅色壁纸下尤其明显），
/// 而 `AboveDock` 的窗口底边正好压在这条线上：柱间隙是透明的，
/// 于是那条线从缝隙里透出来，看起来就是"密集点状黑线"。
/// 这里把窗口最底部两行**跨柱间隙填满柱底色**，把它盖住。
pub const SEAM_COVER_PX: u16 = 2;

/// 由配置算出运行时参数。
#[must_use]
pub fn plan(cfg: &Config) -> ApplyPlan {
    let mut style = RenderStyle::new();
    style.gap_px = gap_of(cfg.visual.bar_gap_ratio);
    style.alpha = alpha_of(cfg.visual.opacity);
    // 两种模式各有各的"底边"处理方式，不能同时开：
    // - 覆盖模式（InsideDock 等）：柱体压在任务栏上，柱底渐隐融入，
    //   若改成不透明反而会糊住任务栏内容；
    // - 任务栏上方模式：窗口底边贴着任务栏顶边的深色线，
    //   渐隐会把那条线暴露得更清楚，所以改成"跨间隙覆盖"。
    if cfg.visual.mode == VisualMode::AboveDock {
        style.bottom_fade_px = 0;
        style.seam_cover_px = SEAM_COVER_PX;
    } else {
        style.bottom_fade_px = BOTTOM_FADE_PX;
        style.seam_cover_px = 0;
    }
    style.peak_cap = cfg.visual.peak_cap;
    style.color_bottom = color_of(&cfg.visual.color_start, style.color_bottom);
    style.color_top = color_of(&cfg.visual.color_end, style.color_top);
    let (palette, palette_len) = spectrum_core::config::parse_palette(&cfg.visual.palette);
    style.palette = palette;
    style.palette_len = palette_len;
    style.palette_mode = match cfg.visual.palette_mode.as_str() {
        "across" => PaletteMode::Across,
        "per_bar" => PaletteMode::PerBar,
        "vertical" => PaletteMode::Vertical,
        _ => PaletteMode::Off,
    };

    ApplyPlan {
        enabled: cfg.general.enabled,
        hide_on_fullscreen: cfg.behavior.hide_on_fullscreen,
        hide_on_lock: cfg.behavior.hide_on_lock,
        hide_on_autohide: cfg.behavior.hide_on_autohide,
        analyzer: cfg.analyze.clone(),
        style,
        silence_dbfs: cfg.behavior.silence_dbfs,
        silence_timeout_ms: cfg.behavior.silence_timeout_ms,
    }
}

/// 两次配置之间被改动的键（名称为 `section.key`，与本项目其它日志一致）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Delta {
    /// 被改动的键，按固定顺序排列。
    pub keys: Vec<&'static str>,
}

impl Delta {
    /// 没有任何可应用的改动（例如只改了未实现的键）。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// 是否需要重建分析器。
    ///
    /// 注意 `visual.bands` 也算：柱数变化会改变 `build_edges` 的结果，
    /// 分析器必须按新柱数重建（`analyze.bands` 只是 `visual.bands` 的影子字段）。
    #[must_use]
    pub fn needs_analyzer_rebuild(&self) -> bool {
        self.keys
            .iter()
            .any(|k| k.starts_with("analyze.") || *k == "visual.bands")
    }

    /// 是否需要重新上屏（配色类改动）。
    #[must_use]
    pub fn needs_repaint(&self) -> bool {
        self.keys
            .iter()
            .any(|k| k.starts_with("visual.") || k.starts_with("behavior."))
    }

    /// 人类可读的一行摘要。
    #[must_use]
    pub fn describe(&self) -> String {
        self.keys.join(", ")
    }
}

/// 比较两份配置，得出需要应用的键。
///
/// 只看**真正会映射到运行时**的字段：改 `visual.rounded` 不会出现在这里，
/// 因为渲染器根本没有圆角（它属于"未实现"，见 [`crate::schema::Scope`]）。
#[must_use]
pub fn diff(before: &Config, after: &Config) -> Delta {
    let mut keys: Vec<&'static str> = Vec::new();
    if before.general.enabled != after.general.enabled {
        keys.push("general.enabled");
    }
    if (before.visual.opacity - after.visual.opacity).abs() > f32::EPSILON {
        keys.push("visual.opacity");
    }
    if before.visual.bands != after.visual.bands {
        keys.push("visual.bands");
    }
    if (before.visual.bar_gap_ratio - after.visual.bar_gap_ratio).abs() > f32::EPSILON {
        keys.push("visual.bar_gap_ratio");
    }
    if before.visual.color_start != after.visual.color_start {
        keys.push("visual.color_start");
    }
    if before.visual.color_end != after.visual.color_end {
        keys.push("visual.color_end");
    }
    // v0.5.0 多色标：漏了这两个键，换预置时颜色不会变（静默失效）。
    if before.visual.palette != after.visual.palette {
        keys.push("visual.palette");
    }
    if before.visual.palette_mode != after.visual.palette_mode {
        keys.push("visual.palette_mode");
    }
    if before.visual.mode != after.visual.mode {
        keys.push("visual.mode");
    }
    if before.visual.peak_cap != after.visual.peak_cap {
        keys.push("visual.peak_cap");
    }
    let a = &before.analyze;
    let b = &after.analyze;
    if a.fft_size != b.fft_size {
        keys.push("analyze.fft_size");
    }
    if (a.f_min - b.f_min).abs() > f32::EPSILON {
        keys.push("analyze.f_min");
    }
    if (a.f_max - b.f_max).abs() > f32::EPSILON {
        keys.push("analyze.f_max");
    }
    if (a.db_floor - b.db_floor).abs() > f32::EPSILON {
        keys.push("analyze.db_floor");
    }
    if (a.attack - b.attack).abs() > f32::EPSILON {
        keys.push("analyze.attack");
    }
    if (a.release - b.release).abs() > f32::EPSILON {
        keys.push("analyze.release");
    }
    if a.peak_hold_ms != b.peak_hold_ms {
        keys.push("analyze.peak_hold_ms");
    }
    // P1：峰值滑块的两个新键必须进入 Delta，否则改了不会触发热重载（静默失效）。
    if (a.peak_fall - b.peak_fall).abs() > f32::EPSILON {
        keys.push("analyze.peak_fall");
    }
    if a.peak_gravity != b.peak_gravity {
        keys.push("analyze.peak_gravity");
    }
    if (a.gamma - b.gamma).abs() > f32::EPSILON {
        keys.push("analyze.gamma");
    }
    if (a.sensitivity - b.sensitivity).abs() > f32::EPSILON {
        keys.push("analyze.sensitivity");
    }
    if (before.behavior.silence_dbfs - after.behavior.silence_dbfs).abs() > f32::EPSILON {
        keys.push("behavior.silence_dbfs");
    }
    if before.behavior.silence_timeout_ms != after.behavior.silence_timeout_ms {
        keys.push("behavior.silence_timeout_ms");
    }
    if before.behavior.hide_on_fullscreen != after.behavior.hide_on_fullscreen {
        keys.push("behavior.hide_on_fullscreen");
    }
    if before.behavior.hide_on_lock != after.behavior.hide_on_lock {
        keys.push("behavior.hide_on_lock");
    }
    if before.behavior.hide_on_autohide != after.behavior.hide_on_autohide {
        keys.push("behavior.hide_on_autohide");
    }
    Delta { keys }
}

#[cfg(test)]
mod tests {
    use super::{BOTTOM_FADE_PX, SEAM_COVER_PX, alpha_of, color_of, diff, gap_of, plan};
    use spectrum_core::config::{Config, VisualMode};
    use spectrum_render::{PaletteMode, RenderStyle};

    #[test]
    fn palette_keys_produce_delta_and_reach_render_style() {
        // 反向门：漏了这两个键，界面上换多色预置就会「看起来选了、颜色没变」。
        let before = Config::default();
        let mut after = before.clone();
        after.visual.palette = "#FF0000,#00FF00,#0000FF".to_owned();
        after.visual.palette_mode = "across".to_owned();
        let d = diff(&before, &after);
        assert!(d.keys.contains(&"visual.palette"));
        assert!(d.keys.contains(&"visual.palette_mode"));
        assert!(d.needs_repaint());
        let p = plan(&after);
        assert_eq!(p.style.palette_len, 3);
        assert_eq!(p.style.palette_mode, PaletteMode::Across);
        assert_eq!(p.style.palette[0], 0xFF_0000);
    }

    #[test]
    fn default_palette_keeps_legacy_render_style() {
        let p = plan(&Config::default());
        assert_eq!(p.style.palette_len, 0);
        assert_eq!(p.style.palette_mode, PaletteMode::Off);
    }

    #[test]
    fn peak_slider_keys_produce_delta() {
        // 反向门：漏了这两个键，GUI 里调回落速度/重力就会「改了没反应」。
        let before = Config::default();
        let mut after = before.clone();
        after.analyze.peak_fall = 0.01;
        after.analyze.peak_gravity = true;
        let d = diff(&before, &after);
        assert!(d.keys.contains(&"analyze.peak_fall"));
        assert!(d.keys.contains(&"analyze.peak_gravity"));
        assert!(d.needs_analyzer_rebuild());
        let p = plan(&after);
        assert!((p.analyzer.peak_fall - 0.01).abs() < 1e-9);
        assert!(p.analyzer.peak_gravity);
    }

    #[test]
    fn default_config_keeps_gap_and_alpha_semantics() {
        let p = plan(&Config::default());
        // 0.2 ↔ 2px：与 RenderStyle::new() 相同，已有像素证据不受影响
        assert_eq!(p.style.gap_px, RenderStyle::new().gap_px);
        assert_eq!(p.style.gap_px, 2);
        // 0.65 × 255 = 165.75 → 166
        assert_eq!(p.style.alpha, 166);
        // 生产路径启用柱底渐隐；new() 默认仍为 0，历史像素证据不受影响。
        assert_eq!(p.style.bottom_fade_px, BOTTOM_FADE_PX);
        assert_eq!(RenderStyle::new().bottom_fade_px, 0);
        // 默认模式（覆盖整条任务栏）不需要接缝覆盖。
        assert_eq!(p.style.seam_cover_px, 0);
    }

    /// 任务栏上方模式必须改成"接缝覆盖"而不是"柱底渐隐"。
    ///
    /// 反向门：两种模式若同时开，渐隐会把任务栏顶端的深色边线暴露得更清楚，
    /// 那条线又会从柱间隙透出来——正是用户反复反馈的"密集点状黑线"。
    #[test]
    fn above_dock_mode_covers_seam_instead_of_fading() {
        let mut cfg = Config::default();
        cfg.visual.mode = VisualMode::AboveDock;
        let p = plan(&cfg);
        assert_eq!(p.style.seam_cover_px, SEAM_COVER_PX);
        assert_eq!(p.style.bottom_fade_px, 0, "上方模式不能用渐隐，否则会透出接缝线");

        let mut inside = Config::default();
        inside.visual.mode = VisualMode::FullDock;
        let q = plan(&inside);
        assert_eq!(q.style.seam_cover_px, 0);
        assert_eq!(q.style.bottom_fade_px, BOTTOM_FADE_PX);
    }

    #[test]
    fn opacity_maps_to_alpha() {
        assert_eq!(alpha_of(0.0), 0);
        assert_eq!(alpha_of(0.05), 13);
        assert_eq!(alpha_of(0.5), 128);
        assert_eq!(alpha_of(1.0), 255);
        // 越界不得回绕成很小的值（cast 截断陷阱）
        assert_eq!(alpha_of(9.9), 255);
        assert_eq!(alpha_of(-3.0), 0);
    }

    #[test]
    fn gap_ratio_maps_to_pixels_capped() {
        assert_eq!(gap_of(0.0), 0);
        assert_eq!(gap_of(0.2), 2);
        assert_eq!(gap_of(0.35), 4);
        assert_eq!(gap_of(0.9), 8);
        assert_eq!(gap_of(50.0), 8, "越界必须钳制");
    }

    #[test]
    fn colors_are_parsed_strictly() {
        assert_eq!(color_of("#00C8FF", 0), 0x00_C8FF);
        assert_eq!(color_of("#ffffff", 0), 0x00FF_FFFF);
        assert_eq!(
            color_of("red", 0x0012_3456),
            0x0012_3456,
            "非 #RRGGBB 必须回退"
        );
        assert_eq!(color_of("#00C8F", 0x0012_3456), 0x0012_3456);
        assert_eq!(color_of("#00C8FFx", 0x0012_3456), 0x0012_3456);
    }

    #[test]
    fn default_plan_colors_come_from_config() {
        let p = plan(&Config::default());
        assert_eq!(p.style.color_bottom, 0x00_C8FF);
        assert_eq!(p.style.color_top, 0x7B_61FF);
        assert_eq!(p.silence_dbfs, -62.0);
        assert_eq!(p.silence_timeout_ms, 800);
        assert!(p.enabled);
        assert_eq!(p.analyzer, Config::default().analyze);
    }

    #[test]
    fn diff_ignores_identical_configs() {
        let a = Config::default();
        assert!(diff(&a, &a.clone()).is_empty());
    }

    #[test]
    fn diff_reports_exact_keys() {
        let a = Config::default();
        let mut b = a.clone();
        b.visual.opacity = 0.9;
        b.analyze.sensitivity = 2.0;
        b.general.enabled = false;
        let d = diff(&a, &b);
        assert!(d.keys.contains(&"visual.opacity"));
        assert!(d.keys.contains(&"analyze.sensitivity"));
        assert!(d.keys.contains(&"general.enabled"));
        assert!(d.needs_analyzer_rebuild(), "灵敏度变化要重建分析器");
        assert!(d.needs_repaint());
        assert_eq!(d.keys.len(), 3, "不该多报：{:?}", d.keys);
    }

    #[test]
    fn unimplemented_fields_do_not_show_up_in_delta() {
        let a = Config::default();
        let mut b = a.clone();
        b.visual.rounded = false;
        b.visual.blend = spectrum_core::config::BlendMode::Alpha;
        b.general.fade_ms = 500;
        // `behavior.hide_on_autohide` 已实现（Wayland 热生效），不再属于
        // "未实现字段"——它变化时**应该**出现在 delta 里。
        // `visual.peak_cap` 同理（渲染器已绘制峰值帽）。
        assert!(
            diff(&a, &b).is_empty(),
            "未实现的字段不该让 daemon 以为要重画"
        );
        let mut c = a.clone();
        c.behavior.hide_on_autohide = false;
        c.visual.peak_cap = false;
        let delta = diff(&a, &c);
        assert!(
            delta.keys.contains(&"behavior.hide_on_autohide"),
            "已实现的字段变化必须出现在 delta 里"
        );
        assert!(
            delta.keys.contains(&"visual.peak_cap"),
            "峰值帽已实现，变化必须触发重绘"
        );
    }

    #[test]
    fn analyzer_rebuild_flag() {
        let a = Config::default();
        let mut b = a.clone();
        b.visual.bands = 96;
        // bands 在 Config::validate 里会同步到 analyze.bands；
        // 这里手工同步，模拟"已校验过的配置"
        b.analyze.bands = 96;
        let d = diff(&a, &b);
        assert!(d.keys.contains(&"visual.bands"));
        assert!(
            d.needs_analyzer_rebuild(),
            "柱数变化必须触发分析器重建（analyze.bands 由 visual.bands 驱动）"
        );
    }

    #[test]
    fn hide_on_fullscreen_is_reported_in_delta() {
        let a = Config::default();
        let mut b = a.clone();
        b.behavior.hide_on_fullscreen = false;
        let d = diff(&a, &b);
        assert!(
            d.keys.contains(&"behavior.hide_on_fullscreen"),
            "{:?}",
            d.keys
        );
        assert!(!d.needs_analyzer_rebuild());
        assert!(d.needs_repaint());
    }

    #[test]
    fn describe_joins_keys() {
        let d = super::Delta {
            keys: vec!["visual.opacity", "analyze.gamma"],
        };
        assert_eq!(d.describe(), "visual.opacity, analyze.gamma");
    }
}
