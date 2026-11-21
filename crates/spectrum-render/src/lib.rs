//! `spectrum-render`：把频谱幅值画成像素，以及把像素送上屏幕的抽象。
//!
//! ## 分层
//!
//! - [`surface`]：ARGB 像素缓冲（预乘 alpha），零依赖、无 X11
//! - [`SpectrumRenderer`]：幅值 → 柱状图的布局与着色，纯整数运算
//!
//! 上屏（`put_image` / EGL）由 `spectrum-platform` 负责，
//! 本 crate 只产出像素，因此可以**完全离线单测** —— 这一点很重要：
//! 布局错位、柱高算错这类问题不该等到有窗口才能发现。
//!
//! ## 颜色约定
//!
//! 统一 **预乘 alpha 的 ARGB**（`0xAARRGGBB`），与《开发指导文档》§6.3 一致。
//! 预乘的好处是合成时只需加法，且 `alpha == 0` 的像素天然是"完全透明"，
//! 与 X11 depth-32 窗口的语义直接对应（背景留空即穿透可见下层）。
//!
//! ## 为什么先做软件渲染
//!
//! 阶段 2 的目标是"频谱真的画出来"，而 EGL/GLES2 需要 GPU 与 EGL 上下文，
//! 在当前容器里无法可靠验证。软件渲染用 `put_image` 就能出画，
//! 并且**像素可回读比对**（`get_image`），能给出机器可判的证据；
//! EGL 后端后续替换的是"上屏方式"，本 crate 的布局逻辑不变。
//!
//! DSP 与几何换算中的 `usize/u32 <-> f32 <-> u16` 转换是布局固有需求：
//! 幅值已由 `spectrum-core` 归一化到 `[0, 1]`，尺寸受 `u16` 约束。
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

pub mod surface;

pub use surface::Surface;

/// 多色标色板最多容纳的色标数。
pub const MAX_PALETTE: usize = 8;

/// 多色标的铺色方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PaletteMode {
    /// 关闭：走 `color_bottom → color_top` 的双色纵向渐变（v0.4.0 及之前的行为）。
    #[default]
    Off,
    /// 横向渐变：色标沿整条频谱从左到右平滑铺开。
    Across,
    /// 每柱一色：按柱索引取离散色标，柱内保持同色。
    PerBar,
    /// 柱内多段：色标沿柱高从下到上铺开，所有柱一致。
    Vertical,
}

/// 柱状图的外观参数。
///
/// 所有像素量都是**物理像素**（不做 DPI 换算）：调用方拿到的是任务栏的
/// 物理矩形，换算已在 `spectrum-core::geometry` 完成。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderStyle {
    /// 相邻柱之间的空隙（物理像素）。单柱宽度不足时自动让位。
    pub gap_px: u16,
    /// 画布顶部留白。
    pub top_margin_px: u16,
    /// 画布底部留白（贴着任务栏底边时通常为 0）。
    pub bottom_margin_px: u16,
    /// 柱底渐隐高度（物理像素）。
    ///
    /// 最底部这么多行的 alpha 线性递减到 0，让柱体"融进"任务栏，
    /// 消除半透明柱底压在深色任务栏上被合成成近黑硬边的观感瑕疵。
    /// `0` 表示关闭，此时逐像素行为与旧版完全一致（像素证据门不受影响）。
    pub bottom_fade_px: u8,
    /// 画布最底部用于遮盖任务栏接缝的行数（物理像素）。
    ///
    /// 这些行会**跨整个柱槽（含间隙）**填满柱底色，用来挡住任务栏自带的
    /// 深色顶边线：那条线在柱间隙处会透出来，看起来像"密集点状黑线"。
    /// 只在该槽有可见柱高时才画，静音时保持全透明（不留下常驻色条）。
    /// `0` 表示关闭，逐像素行为与旧版一致。
    pub seam_cover_px: u16,
    /// 幅值大于 0 时的最小可见高度，避免"有声音但看不见"。
    pub min_bar_px: u16,
    /// 不透明度（0 = 全透明，255 = 不透明）。
    pub alpha: u8,
    /// 是否绘制峰值帽（`visual.peak_cap`）。
    pub peak_cap: bool,
    /// 柱底颜色 `0xRRGGBB`（未预乘）。
    pub color_bottom: u32,
    /// 柱顶颜色 `0xRRGGBB`（未预乘）。与柱底相同时为纯色。
    pub color_top: u32,
    /// 多色标色板，仅前 `palette_len` 个有效。
    pub palette: [u32; MAX_PALETTE],
    /// 有效色标个数；`< 3` 表示关闭多色。
    pub palette_len: u8,
    /// 多色铺色方式；`Off` 时即使有调色板也走双色渐变。
    pub palette_mode: PaletteMode,
}

impl RenderStyle {
    /// 默认样式：2px 间隙、上下各 1px 留白、柱高至少 1px、85% 不透明，
    /// 青色到品红的渐变（在浅色与深色任务栏上都可见）。
    #[must_use]
    pub const fn new() -> Self {
        Self {
            gap_px: 2,
            top_margin_px: 1,
            bottom_margin_px: 1,
            bottom_fade_px: 0,
            seam_cover_px: 0,
            min_bar_px: 1,
            alpha: 217,
            peak_cap: true,
            color_bottom: 0x00_E5FF,
            color_top: 0xFF_2BD6,
            palette: [0; MAX_PALETTE],
            palette_len: 0,
            palette_mode: PaletteMode::Off,
        }
    }

    /// 纯色样式：无渐变，便于像素级精确比对。
    #[must_use]
    pub const fn flat(rgb: u32, alpha: u8) -> Self {
        Self {
            gap_px: 2,
            top_margin_px: 1,
            bottom_margin_px: 1,
            bottom_fade_px: 0,
            seam_cover_px: 0,
            min_bar_px: 1,
            alpha,
            // 像素级测试只关心柱体本身，峰值帽默认关闭。
            peak_cap: false,
            color_bottom: rgb,
            color_top: rgb,
            palette: [0; MAX_PALETTE],
            palette_len: 0,
            palette_mode: PaletteMode::Off,
        }
    }

    /// 可用高度（扣掉上下留白）。
    #[must_use]
    pub const fn usable_height(&self, canvas_height: u16) -> u16 {
        canvas_height.saturating_sub(self.top_margin_px.saturating_add(self.bottom_margin_px))
    }
}

impl Default for RenderStyle {
    fn default() -> Self {
        Self::new()
    }
}

/// 一次渲染的统计，供 doctor / 日志使用，也便于测试断言。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RenderStats {
    /// 实际画出的柱数（等于输入幅值数，除非画布宽度为 0）。
    pub bars: usize,
    /// 每个柱槽的宽度（含间隙）。
    pub slot_width_px: u16,
    /// 单柱实际宽度（扣掉间隙后）。
    pub bar_width_px: u16,
    /// 最高的柱高（像素）。
    pub max_bar_px: u16,
    /// 实际写入的像素数。
    pub drawn_pixels: u64,
    /// 其中峰值帽写入的像素数。
    pub cap_pixels: u64,
}

/// 峰值帽厚度（物理像素）。
const PEAK_CAP_HEIGHT_PX: u16 = 2;

/// 频谱柱状图渲染器。
///
/// 无内部可变状态：同一 `RenderStyle` 可跨帧复用。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpectrumRenderer {
    style: RenderStyle,
}

impl SpectrumRenderer {
    /// 用给定样式构造。
    #[must_use]
    pub const fn new(style: RenderStyle) -> Self {
        Self { style }
    }

    /// 当前样式。
    #[must_use]
    pub const fn style(&self) -> &RenderStyle {
        &self.style
    }

    /// 把幅值渲染到 `surface` 上（不带峰值帽的便捷入口）。
    ///
    /// `magnitudes` 应为已归一化的 `[0, 1]`；非有限值与越界值都会被
    /// 收敛到合法区间（**不**跳过，避免柱数随垃圾数据变化而抖动）。
    ///
    /// 每次调用都会先清空 `surface`（置为全透明），
    /// 否则上一帧的高柱会在本帧留下残影。
    ///
    /// 返回本帧的统计。
    pub fn render(&self, surface: &mut Surface, magnitudes: &[f32]) -> RenderStats {
        self.render_with_peaks(surface, magnitudes, &[])
    }

    /// 画柱体 + 峰值帽。
    ///
    /// `peaks` 与 `magnitudes` 一一对应，取值 `0..=1`；缺失的峰值不画帽。
    /// 峰值帽是柱顶上方 2px 的横条，用于显示"最近最响高度"。
    pub fn render_with_peaks(
        &self,
        surface: &mut Surface,
        magnitudes: &[f32],
        peaks: &[f32],
    ) -> RenderStats {
        surface.clear();

        let width = u32::from(surface.width());
        let height = surface.height();
        let usable = self.style.usable_height(height);
        let count = magnitudes.len();
        if count == 0 || width == 0 || usable == 0 {
            return RenderStats {
                bars: count,
                ..RenderStats::default()
            };
        }

        let count_u32 = u32::try_from(count).unwrap_or(u32::MAX);
        // 槽宽按整数均分，余数由"左侧槽多 1px"吸收 —— 这样总和恒等于画布宽度，
        // 不会在右端留下一条不属于任何柱的空白（那会被误判成渲染错位）。
        let slot_width = width / count_u32;
        let gap = u32::from(self.style.gap_px).min(slot_width.saturating_sub(1));
        let bar_width = slot_width.saturating_sub(gap);
        let bottom = height.saturating_sub(self.style.bottom_margin_px);
        let cap_thickness = u32::from(PEAK_CAP_HEIGHT_PX).min(u32::from(bottom));
        // 接缝覆盖行取**画布最后几行**（不受 bottom_margin 影响）：
        // 任务栏的深色顶边正好落在窗口底边，必须在最后一两行上盖住它。
        let seam_rows = u32::from(self.style.seam_cover_px).min(u32::from(height));
        // 有柱的槽跨度：接缝只铺在这段范围内。
        // 只铺"有柱的那几个槽"会在中间静音槽处漏出任务栏的深色边线，
        // 于是仍旧是一段段的黑点；铺满整幅又会留下常驻色条。
        let active_span = if seam_rows > 0 && self.style.alpha > 0 {
            let mut span: Option<(u32, u32)> = None;
            for (index, &magnitude) in magnitudes.iter().enumerate() {
                if bar_height_px(normalize_magnitude(magnitude), usable, self.style.min_bar_px) == 0 {
                    continue;
                }
                let i = u32::try_from(index).unwrap_or(u32::MAX);
                span = Some(match span {
                    Some((first, _)) => (first, i),
                    None => (i, i),
                });
            }
            span
        } else {
            None
        };

        let mut max_bar = 0_u16;
        let mut drawn = 0_u64;
        let mut cap_pixels = 0_u64;

        for (index, &magnitude) in magnitudes.iter().enumerate() {
            let index_u32 = u32::try_from(index).unwrap_or(u32::MAX);
            let x0 = index_u32 * width / count_u32 + gap / 2;
            let x1 = (index_u32 + 1) * width / count_u32 - (gap - gap / 2);
            if x1 <= x0 {
                continue;
            }

            let level = normalize_magnitude(magnitude);
            let bar_height = bar_height_px(level, usable, self.style.min_bar_px);
            max_bar = max_bar.max(bar_height);
            if bar_height > 0 {
                let y1 = u32::from(bottom);
                let y0 = y1.saturating_sub(u32::from(bar_height));
                drawn += self.fill_bar(
                    surface,
                    SatRange { x0, x1 },
                    SatRange { x0: y0, x1: y1 },
                    usable,
                    index_u32,
                    count_u32,
                );
            }

            // 峰值帽：高度取峰值柱高，画 2px 横条。
            if self.style.peak_cap
                && let Some(&peak) = peaks.get(index)
            {
                let cap_height = bar_height_px(normalize_magnitude(peak), usable, 1);
                if cap_height > 0 {
                    let cap_top = u32::from(bottom).saturating_sub(u32::from(cap_height));
                    let cap_end = cap_top.saturating_add(cap_thickness).min(u32::from(bottom));
                    for y in cap_top..cap_end {
                        let from_bottom = u32::from(usable)
                            .saturating_sub(1)
                            .saturating_sub(y.saturating_sub(u32::from(self.style.top_margin_px)));
                        let color =
                            self.bar_color(u32::from(usable), from_bottom, index_u32, count_u32);
                        let written = surface.fill_row(y, x0, x1, color);
                        cap_pixels += written;
                        drawn += written;
                    }
                }
            }
        }

        // 接缝覆盖：在"有柱的横向区间"内把最底部若干行整段填满（连柱间隙一起），
        // 遮住任务栏自带的深色顶边线——否则那条线会从柱间隙透出来，呈密集点状。
        // 用**不透明**色：半透明会让那条线按 alpha 比例透出来，仍旧是点状。
        // 按槽取色，多色模式下接缝与柱底同色。
        if let Some((first, last)) = active_span {
            for y in (u32::from(height) - seam_rows)..u32::from(height) {
                for index in first..=last {
                    let slot_x0 = index * width / count_u32;
                    let slot_x1 = (index + 1) * width / count_u32;
                    let seam = self.bar_color_at(u32::from(usable), 0, index, count_u32, 255);
                    drawn += surface.fill_row(y, slot_x0, slot_x1, seam);
                }
            }
        }

        RenderStats {
            bars: count,
            slot_width_px: u16::try_from(slot_width).unwrap_or(u16::MAX),
            bar_width_px: u16::try_from(bar_width).unwrap_or(u16::MAX),
            max_bar_px: max_bar,
            drawn_pixels: drawn,
            cap_pixels,
        }
    }

    /// 画一根柱；返回写入的像素数。
    ///
    /// 颜色按行插值：`y` 越靠上越接近 `color_top`，
    /// 这样"高柱的顶端更接近配色终点"，视觉上能看出高低差异。
    fn fill_bar(
        &self,
        surface: &mut Surface,
        xs: SatRange,
        ys: SatRange,
        usable: u16,
        index: u32,
        count: u32,
    ) -> u64 {
        let usable = u32::from(usable);
        let top_margin = u32::from(self.style.top_margin_px);
        let fade = u32::from(self.style.bottom_fade_px);
        let mut written = 0_u64;
        for y in ys.x0..ys.x1 {
            // t = 该行距**柱底**多少行（0 = 最底一行），用于取渐变色。
            let from_bottom = usable
                .saturating_sub(1)
                .saturating_sub(y.saturating_sub(top_margin));
            let color = self.bar_color(usable, from_bottom, index, count);
            // 柱底渐隐：越靠近底边 alpha 越低，让柱体柔和地融进任务栏，
            // 消除半透明柱底被深色任务栏合成成近黑硬边 + 间隙亮点的问题。
            // 最底行保留 1/fade 的可见度，避免极矮柱（min_bar_px）整个消失。
            let color = if fade > 0 && from_bottom < fade {
                scale_premultiplied(color, (from_bottom + 1) * 255 / fade)
            } else {
                color
            };
            written += surface.fill_row(y, xs.x0, xs.x1, color);
        }
        written
    }

    /// 某个像素的颜色（预乘 alpha 的 ARGB）。
    ///
    /// `palette_len < 3` 或 `palette_mode == Off` 时走与 v0.4.0 完全相同的
    /// 「按柱高在 `color_bottom` → `color_top` 之间插值」，逐像素等价。
    fn bar_color(&self, usable: u32, from_bottom: u32, index: u32, count: u32) -> u32 {
        self.bar_color_at(usable, from_bottom, index, count, self.style.alpha)
    }

    /// 同 [`Self::bar_color`]，但用指定 alpha（接缝覆盖需要不透明版本）。
    fn bar_color_at(
        &self,
        usable: u32,
        from_bottom: u32,
        index: u32,
        count: u32,
        alpha: u8,
    ) -> u32 {
        let height_t = if usable <= 1 {
            0
        } else {
            (from_bottom.saturating_mul(255) / (usable - 1)).min(255)
        };
        if self.style.palette_len >= 3 && self.style.palette_mode != PaletteMode::Off {
            let (r, g, b) = match self.style.palette_mode {
                PaletteMode::Across => {
                    let t = if count <= 1 {
                        0
                    } else {
                        (index.saturating_mul(255) / (count - 1)).min(255)
                    };
                    let (r, g, b) = sample_palette(&self.style.palette, self.style.palette_len, t);
                    // 柱内轻微明度（底部 72% → 顶部 100%），保留横向渐变连贯性。
                    shade(r, g, b, height_t)
                }
                PaletteMode::PerBar => {
                    let len = u32::from(self.style.palette_len);
                    let seg = (len.saturating_mul(index) / count.max(1)).min(len - 1);
                    let v = self.style.palette[seg as usize];
                    shade(v >> 16 & 0xFF, v >> 8 & 0xFF, v & 0xFF, height_t)
                }
                PaletteMode::Vertical => {
                    sample_palette(&self.style.palette, self.style.palette_len, height_t)
                }
                PaletteMode::Off => (0, 0, 0),
            };
            return premultiply(r, g, b, alpha);
        }
        let bottom = self.style.color_bottom;
        let top = self.style.color_top;
        let r = lerp_channel(bottom >> 16, top >> 16, height_t);
        let g = lerp_channel(bottom >> 8, top >> 8, height_t);
        let b = lerp_channel(bottom, top, height_t);
        premultiply(r, g, b, alpha)
    }
}

/// 在多色标上按 `t`（0..=255）插值取色。
#[allow(clippy::many_single_char_names)] // 色标插值里 p/len/t/a/b/seg/f 是领域惯例
fn sample_palette(p: &[u32; MAX_PALETTE], len: u8, t: u32) -> (u32, u32, u32) {
    let segments = u32::from(len.saturating_sub(1)).max(1);
    let pos = t.min(255).saturating_mul(segments);
    let seg = (pos / 255).min(segments - 1);
    // 用「总位移 - 段起点」算权重：t=255 时 seg 被钳到末段、f 必须是 255，
    // 才能精确落在最后一个色标上（否则会退回倒数第二色）。
    let f = pos.saturating_sub(seg.saturating_mul(255)).min(255);
    let a = p[seg as usize];
    let b = p[(seg + 1) as usize];
    (
        lerp_channel(a >> 16 & 0xFF, b >> 16 & 0xFF, f),
        lerp_channel(a >> 8 & 0xFF, b >> 8 & 0xFF, f),
        lerp_channel(a & 0xFF, b & 0xFF, f),
    )
}

/// 给颜色加一层随柱高变化的明度，避免多色柱看起来像平板色块。
fn shade(r: u32, g: u32, b: u32, height_t: u32) -> (u32, u32, u32) {
    let k = 184 + (height_t.saturating_mul(71) / 255); // 184..=255
    ((r * k) / 255, (g * k) / 255, (b * k) / 255)
}

/// 半开区间 `[x0, x1)`，用于像素范围。
#[derive(Debug, Clone, Copy)]
struct SatRange {
    x0: u32,
    x1: u32,
}

/// 把任意幅值收敛到 `[0, 1]`；`NaN` 视为 0。
fn normalize_magnitude(magnitude: f32) -> f32 {
    if magnitude.is_finite() {
        magnitude.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

/// 幅值 → 柱高（像素）。
///
/// `level == 0` 时返回 0（完全不画）；否则至少 `min_px` 且不超过 `usable`。
fn bar_height_px(level: f32, usable: u16, min_px: u16) -> u16 {
    if level <= 0.0 {
        return 0;
    }
    let raw = (level * f32::from(usable)).round();
    let capped = if raw >= f32::from(usable) {
        usable
    } else if raw < 1.0 {
        1
    } else {
        u16::try_from(raw as u32).unwrap_or(usable)
    };
    capped.max(min_px.min(usable))
}

/// 两通道按 `t/255` 线性插值。
fn lerp_channel(from: u32, to: u32, t: u32) -> u32 {
    let from = from & 0xFF;
    let to = to & 0xFF;
    if to >= from {
        from + (to - from) * t / 255
    } else {
        from - (from - to) * t / 255
    }
}

/// 合成预乘 alpha 的 ARGB。
fn premultiply(r: u32, g: u32, b: u32, alpha: u8) -> u32 {
    let a = u32::from(alpha);
    let r = (r & 0xFF) * a / 255;
    let g = (g & 0xFF) * a / 255;
    let b = (b & 0xFF) * a / 255;
    (a << 24) | (r << 16) | (g << 8) | b
}

/// 按比例缩放一个预乘 alpha 颜色的**所有四个通道**（含 alpha）。
///
/// 预乘语义下，`k · premul(r,g,b,a) == premul(r,g,b,k·a)`，
/// 所以四通道同乘 `factor/255` 既降低了不透明度，又保持颜色可正确合成，
/// 不会出现"变暗但没变透明"的脏边。`factor` 需调用方保证在 `0..=255`。
fn scale_premultiplied(argb: u32, factor: u32) -> u32 {
    let a = (argb >> 24) & 0xFF;
    let r = (argb >> 16) & 0xFF;
    let g = (argb >> 8) & 0xFF;
    let b = argb & 0xFF;
    let scale = |v: u32| v * factor / 255;
    (scale(a) << 24) | (scale(r) << 16) | (scale(g) << 8) | scale(b)
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_PALETTE, PaletteMode, RenderStyle, SpectrumRenderer, bar_height_px,
        normalize_magnitude, sample_palette,
    };
    use crate::Surface;

    /// 测试用画布：320×80，便于手算。
    fn canvas() -> Surface {
        Surface::new(320, 80)
    }

    fn multicolor_style(mode: PaletteMode) -> RenderStyle {
        let mut style = RenderStyle::new();
        let mut palette = [0; MAX_PALETTE];
        palette[..4].copy_from_slice(&[0xFF_0000, 0x00_FF00, 0x00_00FF, 0xFF_FF00]);
        style.palette = palette;
        style.palette_len = 4;
        style.palette_mode = mode;
        style
    }

    #[test]
    fn default_palette_is_off() {
        let style = RenderStyle::new();
        assert_eq!(style.palette_len, 0);
        assert_eq!(style.palette_mode, PaletteMode::Off);
    }

    #[test]
    fn palette_off_is_pixel_identical_to_legacy_gradient() {
        // 回归门：即使填了调色板，mode=Off 也必须逐像素等于默认双色渐变。
        let levels = [0.2, 0.9, 0.5, 0.75, 1.0];
        let mut legacy = canvas();
        let mut off = canvas();
        SpectrumRenderer::new(RenderStyle::new()).render(&mut legacy, &levels);
        SpectrumRenderer::new(multicolor_style(PaletteMode::Off)).render(&mut off, &levels);
        assert_eq!(legacy.pixels(), off.pixels(), "多色关闭时不得改变任何像素");
    }

    #[test]
    fn sample_palette_hits_both_endpoints() {
        let mut p = [0; MAX_PALETTE];
        p[..3].copy_from_slice(&[0xFF_0000, 0x00_FF00, 0x00_00FF]);
        assert_eq!(sample_palette(&p, 3, 0), (0xFF, 0, 0));
        assert_eq!(sample_palette(&p, 3, 255), (0, 0, 0xFF));
    }

    #[test]
    fn across_palette_varies_horizontally() {
        let mut surface = Surface::new(40, 10);
        SpectrumRenderer::new(multicolor_style(PaletteMode::Across))
            .render(&mut surface, &[1.0; 4]);
        // y=9 是 bottom_margin 留白（透明），最底一行着色像素是 y=8。
        let left = surface.get(2, 8).expect("左柱像素");
        let right = surface.get(32, 8).expect("右柱像素");
        assert_ne!(left, right, "横向渐变两端颜色应不同");
    }

    #[test]
    fn per_bar_palette_uses_multiple_distinct_colors() {
        let mut surface = Surface::new(40, 10);
        SpectrumRenderer::new(multicolor_style(PaletteMode::PerBar))
            .render(&mut surface, &[1.0; 4]);
        let mut seen = std::collections::HashSet::new();
        for x in [2, 12, 22, 32] {
            seen.insert(surface.get(x, 8).expect("柱像素"));
        }
        assert!(
            seen.len() >= 3,
            "每柱一色应出现多种颜色，实际 {}",
            seen.len()
        );
    }

    #[test]
    fn vertical_palette_varies_with_bar_height() {
        let mut surface = Surface::new(10, 20);
        SpectrumRenderer::new(multicolor_style(PaletteMode::Vertical)).render(&mut surface, &[1.0]);
        let bottom = surface.get(5, 18).expect("柱底像素");
        let top = surface.get(5, 1).expect("柱顶像素");
        assert_ne!(bottom, top, "柱内多段：上下颜色应不同");
    }

    #[test]
    fn empty_magnitudes_draws_nothing() {
        let mut surface = canvas();
        let stats =
            SpectrumRenderer::new(RenderStyle::flat(0xFF_FFFF, 255)).render(&mut surface, &[]);
        assert_eq!(stats.bars, 0);
        assert_eq!(stats.drawn_pixels, 0);
        assert!(surface.pixels().iter().all(|&p| p == 0));
    }

    #[test]
    fn full_magnitude_fills_usable_height() {
        let style = RenderStyle::flat(0xFF_FFFF, 255);
        let mut surface = Surface::new(10, 20);
        let stats = SpectrumRenderer::new(style).render(&mut surface, &[1.0]);
        // 可用高度 = 20 - 1 - 1 = 18
        assert_eq!(stats.max_bar_px, 18);
        // 柱覆盖 y = 1..19（bottom_margin = 1）
        assert_eq!(surface.get(5, 1), Some(0xFFFF_FFFF));
        assert_eq!(surface.get(5, 18), Some(0xFFFF_FFFF));
        assert_eq!(surface.get(5, 0), Some(0), "顶部留白应透明");
        assert_eq!(surface.get(5, 19), Some(0), "底部留白应透明");
    }

    #[test]
    fn half_magnitude_is_half_height() {
        let mut surface = Surface::new(10, 20);
        let stats =
            SpectrumRenderer::new(RenderStyle::flat(0xFF_FFFF, 255)).render(&mut surface, &[0.5]);
        // 0.5 * 18 = 9
        assert_eq!(stats.max_bar_px, 9);
    }

    #[test]
    fn out_of_range_and_non_finite_are_clamped() {
        assert!(normalize_magnitude(f32::NAN).abs() < f32::EPSILON);
        assert!(normalize_magnitude(f32::INFINITY).abs() < f32::EPSILON);
        assert!((normalize_magnitude(9.0) - 1.0).abs() < f32::EPSILON);
        assert!(normalize_magnitude(-3.0).abs() < f32::EPSILON);
    }

    #[test]
    fn zero_magnitude_draws_no_pixels() {
        let mut surface = canvas();
        let stats = SpectrumRenderer::new(RenderStyle::flat(0xFF_FFFF, 255))
            .render(&mut surface, &[0.0, 0.0, 0.0]);
        assert_eq!(stats.bars, 3);
        assert_eq!(stats.drawn_pixels, 0);
        assert!(surface.pixels().iter().all(|&p| p == 0));
    }

    #[test]
    fn gap_produces_transparent_columns_between_bars() {
        // 4 柱、宽 40 → 槽宽 10、间隙 2 → 柱宽 8，柱内 x 起始 +1
        let style = RenderStyle {
            gap_px: 2,
            top_margin_px: 0,
            bottom_margin_px: 0,
            min_bar_px: 1,
            alpha: 255,
            peak_cap: false,
            color_bottom: 0xFF_FFFF,
            color_top: 0xFF_FFFF,
            ..RenderStyle::new()
        };
        let mut surface = Surface::new(40, 10);
        SpectrumRenderer::new(style).render(&mut surface, &[1.0; 4]);
        // 第一槽占 x = 0..10，柱本身在 1..9
        assert_eq!(surface.get(0, 9), Some(0), "槽左边缘的间隙应透明");
        assert_eq!(surface.get(1, 9), Some(0xFFFF_FFFF), "柱左边缘应着色");
        assert_eq!(surface.get(8, 9), Some(0xFFFF_FFFF), "柱右边缘应着色");
        assert_eq!(surface.get(9, 9), Some(0), "槽右边缘的间隙应透明");
    }

    #[test]
    fn slots_cover_full_width_without_leftover_column() {
        // 320/7 除不尽，必须仍然铺满到最后一列，否则右端会出现"无主"空白
        let style = RenderStyle {
            gap_px: 0,
            top_margin_px: 0,
            bottom_margin_px: 0,
            min_bar_px: 1,
            alpha: 255,
            peak_cap: false,
            color_bottom: 0xFF_FFFF,
            color_top: 0xFF_FFFF,
            ..RenderStyle::new()
        };
        let mut surface = Surface::new(320, 4);
        SpectrumRenderer::new(style).render(&mut surface, &[1.0; 7]);
        for x in 0..320 {
            assert_eq!(surface.get(x, 3), Some(0xFFFF_FFFF), "x={x} 应被覆盖");
        }
    }

    #[test]
    fn render_clears_previous_frame() {
        let mut surface = Surface::new(10, 20);
        let renderer = SpectrumRenderer::new(RenderStyle::flat(0xFF_FFFF, 255));
        renderer.render(&mut surface, &[1.0]);
        assert!(surface.pixels().iter().any(|&p| p != 0));
        let stats = renderer.render(&mut surface, &[0.0]);
        assert_eq!(stats.drawn_pixels, 0);
        assert!(surface.pixels().iter().all(|&p| p == 0), "上一帧必须被清掉");
    }

    #[test]
    fn peak_cap_draws_bar_above_current_level() {
        let mut style = RenderStyle::flat(0xFF_FFFF, 255);
        style.peak_cap = true;
        let mut surface = Surface::new(10, 20);
        let stats = SpectrumRenderer::new(style).render_with_peaks(&mut surface, &[0.2], &[0.8]);

        // 10x20、上下各 1px 留白 → usable=18、底边 y=19。
        // 柱高 4 → y=15..19；峰值 0.8 → 帽顶 y=5，厚 2px。
        assert_eq!(surface.get(5, 5), Some(0xFFFF_FFFF), "峰值帽第一行");
        assert_eq!(surface.get(5, 6), Some(0xFFFF_FFFF), "峰值帽第二行");
        assert_eq!(surface.get(5, 7), Some(0), "帽与柱体之间应透明");
        assert_eq!(surface.get(5, 15), Some(0xFFFF_FFFF), "柱顶仍在原高度");
        assert_eq!(stats.cap_pixels, 16, "2 行 × 8px 柱宽");

        style.peak_cap = false;
        let mut surface = Surface::new(10, 20);
        let stats = SpectrumRenderer::new(style).render_with_peaks(&mut surface, &[0.2], &[0.8]);
        assert_eq!(stats.cap_pixels, 0);
        assert_eq!(surface.get(5, 5), Some(0), "关闭后不画峰值帽");
    }

    #[test]
    fn alpha_is_premultiplied() {
        // 128/255 alpha 的纯白 → 颜色通道 = 255*128/255 = 128 = 0x80
        let style = RenderStyle::flat(0xFF_FFFF, 128);
        let mut surface = Surface::new(4, 4);
        SpectrumRenderer::new(style).render(&mut surface, &[1.0]);
        // 4x4 画布、上下各 1px 留白 → 柱覆盖 y = 1..3，取 y=2 才在柱内
        let pixel = surface.get(2, 2).unwrap();
        assert_eq!(pixel >> 24, 128, "alpha 通道");
        assert_eq!(pixel & 0x00FF_FFFF, 0x0080_8080, "颜色需预乘 alpha");
    }

    #[test]
    fn bar_height_is_monotonic_in_magnitude() {
        let mut previous = 0;
        for step in 0..=10 {
            let level = step as f32 / 10.0;
            let h = bar_height_px(level, 18, 1);
            assert!(h >= previous, "幅值增大时柱高不应下降（{level}）");
            previous = h;
        }
    }

    #[test]
    fn default_gradient_goes_from_cyan_bottom_to_magenta_top() {
        // 这个测试是为一个真实踩过的坑加的：`color_bottom` 曾写成
        // `0x00E5_FFFF`（8 位十六进制），掩到 24 位后成了 `0xE5FFFF`
        // = 浅青白，于是整条频谱发白。颜色常量打错编译器不会报，
        // 只有把"两端应该是什么色相"写成断言才抓得住。
        let style = RenderStyle::new();
        let mut surface = Surface::new(8, 40);
        SpectrumRenderer::new(style).render(&mut surface, &[1.0]);

        let bottom = surface.get(4, 38).expect("柱底应有像素");
        let top = surface.get(4, 2).expect("柱顶应有像素");

        let (br, bg, bb) = ((bottom >> 16) & 0xFF, (bottom >> 8) & 0xFF, bottom & 0xFF);
        let (tr, tg, tb) = ((top >> 16) & 0xFF, (top >> 8) & 0xFF, top & 0xFF);

        // 底部：蓝 >= 绿 > 红（青色系，红应当接近 0）
        assert!(bb >= bg && bg > br, "柱底应偏青：r={br} g={bg} b={bb}");
        assert!(br <= 8, "柱底红色分量应接近 0，实测 {br}");
        // 顶部：红 > 蓝 > 绿（品红系）
        assert!(tr > tb && tb > tg, "柱顶应偏品红：r={tr} g={tg} b={tb}");
    }

    #[test]
    fn style_usable_height_saturates() {
        let style = RenderStyle::new();
        assert_eq!(style.usable_height(0), 0);
        assert_eq!(style.usable_height(1), 0, "上下留白吃掉全部高度");
    }

    /// 柱底渐隐默认关闭，保证所有历史像素证据逐像素不变。
    #[test]
    fn bottom_fade_is_off_by_default() {
        assert_eq!(RenderStyle::new().bottom_fade_px, 0);
        assert_eq!(RenderStyle::flat(0xFF_FFFF, 255).bottom_fade_px, 0);
    }

    /// 开启渐隐后：柱底最底行 alpha 最低，往上逐行递增，到 fade 高度之上恢复满值。
    #[test]
    fn bottom_fade_ramps_alpha_from_base() {
        let style = RenderStyle {
            bottom_fade_px: 3,
            ..RenderStyle::flat(0xFF_FFFF, 255)
        };
        let mut surface = Surface::new(10, 20);
        // usable = 18，bottom = 19，柱覆盖 y=1..19；底行 y=18，往上 from_bottom 递增。
        SpectrumRenderer::new(style).render(&mut surface, &[1.0]);
        let alpha_at = |y| surface.get(5, y).map(|p| (p >> 24) & 0xFF).unwrap();
        let base = alpha_at(18); // from_bottom = 0 → factor = 1*255/3 = 85
        let one_up = alpha_at(17); // from_bottom = 1 → factor = 2*255/3 = 170
        let two_up = alpha_at(16); // from_bottom = 2 → 满 255
        assert!(base < one_up, "底行应比上一行更透明：{base} vs {one_up}");
        assert!(one_up < two_up, "次底行应比再上一行更透明：{one_up} vs {two_up}");
        assert_eq!(two_up, 255, "渐隐高度之上应为满 alpha");
        assert!(base >= 80 && base <= 90, "底行约 1/3 alpha，实测 {base}");
    }

    /// 渐隐后柱底像素仍是合法的预乘颜色：alpha 降低时各通道同步降低。
    #[test]
    fn scale_premultiplied_keeps_channels_le_alpha() {
        // 纯白全不透明 0xFFFFFFFF，缩放到 factor=128 → 各通道约 128。
        let scaled = super::scale_premultiplied(0xFFFF_FFFF, 128);
        assert_eq!(scaled >> 24, 128, "alpha 通道缩放");
        assert_eq!(scaled & 0x00FF_FFFF, 0x0080_8080, "RGB 需随 alpha 同比缩放");
    }

    /// 接缝覆盖默认关闭：历史像素证据必须逐像素不变。
    #[test]
    fn seam_cover_is_off_by_default() {
        assert_eq!(RenderStyle::new().seam_cover_px, 0);
        assert_eq!(RenderStyle::flat(0xFF_FFFF, 255).seam_cover_px, 0);
    }

    /// 开启接缝覆盖后，画布最底部若干行必须**连柱间隙一起**填满。
    ///
    /// 这是"任务栏顶端深色点状线"的关键断言：只填柱体是不够的，
    /// 间隙只要有一个像素透明，那条线就会从缝隙里透出来。
    #[test]
    fn seam_cover_fills_bar_gaps_in_bottom_rows() {
        let style = RenderStyle {
            seam_cover_px: 2,
            ..RenderStyle::flat(0xFF_FFFF, 255)
        };
        let mut surface = Surface::new(10, 20);
        // 宽度 10 / 2 柱 → 每槽 5px，间隙 2px，柱宽 3px。
        SpectrumRenderer::new(style).render(&mut surface, &[1.0, 1.0]);
        for y in [18, 19] {
            for x in 0..10 {
                let alpha = surface.get(x, y).map(|p| (p >> 24) & 0xFF).unwrap();
                assert_eq!(alpha, 255, "底行 x={x} y={y} 应被不透明覆盖（含间隙）");
            }
        }
        // 覆盖行之上仍应保留原有的间隙（不能把整根柱糊成一片）。
        // 槽 0 的柱体占 x=1..=3，x=4 是它与槽 1 之间的间隙。
        let gap_alpha = surface.get(4, 16).map(|p| (p >> 24) & 0xFF).unwrap();
        assert_eq!(gap_alpha, 0, "覆盖行以上的间隙必须保持透明");
    }

    /// 静音（该槽柱高为 0）时不画接缝覆盖，避免留下常驻色条。
    #[test]
    fn seam_cover_skips_silent_slots() {
        let style = RenderStyle {
            seam_cover_px: 2,
            ..RenderStyle::flat(0xFF_FFFF, 255)
        };
        let mut surface = Surface::new(10, 20);
        SpectrumRenderer::new(style).render(&mut surface, &[1.0, 0.0]);
        let covered = surface.get(2, 19).map(|p| (p >> 24) & 0xFF).unwrap();
        let silent = surface.get(7, 19).map(|p| (p >> 24) & 0xFF).unwrap();
        assert_eq!(covered, 255, "有声槽的底行应被覆盖");
        assert_eq!(silent, 0, "静音槽不应留下色条");
    }

    /// 中间有静音槽时要跨过去连续覆盖：否则任务栏那条线会在静音槽处
    /// 重新露出，看起来仍旧是一段段的黑点（这正是用户反复反馈的形态）。
    #[test]
    fn seam_cover_bridges_silent_slots_inside_span() {
        let style = RenderStyle {
            seam_cover_px: 2,
            ..RenderStyle::flat(0xFF_FFFF, 255)
        };
        let mut surface = Surface::new(12, 20);
        // 三槽（每槽 4px），中间槽静音，但它在"有柱区间"内 → 底行必须被盖住。
        SpectrumRenderer::new(style).render(&mut surface, &[1.0, 0.0, 1.0]);
        for x in [0, 2, 5, 7, 10] {
            let alpha = surface.get(x, 19).map(|p| (p >> 24) & 0xFF).unwrap();
            assert_eq!(alpha, 255, "底行 x={x} 应被连续覆盖（含中间的静音槽）");
        }
        // 区间之外（右端静音槽）不覆盖。
        let mut outside = Surface::new(12, 20);
        SpectrumRenderer::new(style).render(&mut outside, &[1.0, 0.0, 0.0]);
        let tail = outside.get(9, 19).map(|p| (p >> 24) & 0xFF).unwrap();
        assert_eq!(tail, 0, "最后一根柱之外的尾部不应有色条");
    }
}
