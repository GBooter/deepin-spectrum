//! B 档动画预览：Rust 侧生成 RGBA 帧 → `Image::from_rgba8` → Slint `Image` 属性。
//!
//! ## 为什么是模拟帧
//!
//! 设置界面与 daemon 零耦合（界面只写配置文件），这里不接真实音频电平，
//! 而是按与 [`spectrum_core::analyzer`] 相同的物理生成一段「像音乐」的幅值：
//! 低频更厚、有鼓点脉冲、attack/release 一阶平滑、峰值滑块顶飞后悬停缓落。
//! 这样用户在没播放音乐时也能看到每套预置的性格。
//!
//! ## CPU 预算（设计约束 R8）
//!
//! 产品侧 Timer 为 **200ms（5fps）**，每次只在画布上写 900×96 的 RGBA，
//! 且仅当设置窗口可见时刷新。原型里的 46.9fps 只用于观察物理，产品不跑满帧。

use slint::{Image, Rgba8Pixel, SharedPixelBuffer};
use spectrum_core::config::Config;

/// 一帧预览需要的全部外观/动态参数（从 `Config` 提取，保持纯数据结构便于单测）。
#[derive(Debug, Clone, PartialEq)]
pub struct PreviewStyle {
    /// 柱底颜色 `0xRRGGBB`。
    pub color_start: u32,
    /// 柱顶颜色 `0xRRGGBB`。
    pub color_end: u32,
    /// 多色标色板（与 `RenderStyle.palette` 同构）。
    pub palette: [u32; 8],
    /// 有效色标数；`< 3` 表示关闭。
    pub palette_len: u8,
    /// 铺色方式 `off` / `across` / `per_bar` / `vertical`。
    pub palette_mode: String,
    /// 不透明度 0~1。
    pub opacity: f32,
    /// 柱数。
    pub bands: usize,
    /// 柱间隙比例。
    pub gap_ratio: f32,
    /// 是否绘制峰值滑块。
    pub peak_cap: bool,
    /// 滑块悬停时长（毫秒）。
    pub hold_ms: u32,
    /// 滑块每帧回落量。
    pub fall: f32,
    /// 滑块是否重力加速。
    pub gravity: bool,
    /// 起音系数。
    pub attack: f32,
    /// 衰减系数。
    pub release: f32,
    /// 灵敏度。
    pub sensitivity: f32,
}

impl Default for PreviewStyle {
    fn default() -> Self {
        Self::from_config(&Config::default())
    }
}

impl PreviewStyle {
    /// 从当前生效配置提取预览参数。
    #[must_use]
    pub fn from_config(c: &Config) -> Self {
        Self {
            color_start: parse_hex(&c.visual.color_start, 0x00_C8_FF),
            color_end: parse_hex(&c.visual.color_end, 0x7B_61_FF),
            palette: spectrum_core::config::parse_palette(&c.visual.palette).0,
            palette_len: spectrum_core::config::parse_palette(&c.visual.palette).1,
            palette_mode: c.visual.palette_mode.clone(),
            opacity: c.visual.opacity,
            bands: c.visual.bands.clamp(1, 128),
            gap_ratio: c.visual.bar_gap_ratio,
            peak_cap: c.visual.peak_cap,
            hold_ms: c.analyze.peak_hold_ms,
            fall: c.analyze.peak_fall,
            gravity: c.analyze.peak_gravity,
            attack: c.analyze.attack,
            release: c.analyze.release,
            sensitivity: c.analyze.sensitivity,
        }
    }
}

fn parse_hex(s: &str, fallback: u32) -> u32 {
    let b = s.as_bytes();
    if b.len() == 7 && b[0] == b'#' {
        u32::from_str_radix(&s[1..], 16).unwrap_or(fallback)
    } else {
        fallback
    }
}

const fn rgb(v: u32) -> (u8, u8, u8) {
    (
        ((v >> 16) & 0xFF) as u8,
        ((v >> 8) & 0xFF) as u8,
        (v & 0xFF) as u8,
    )
}

const fn lerp(a: u8, b: u8, t: u32) -> u8 {
    let (a, b) = (a as u32, b as u32);
    if b >= a {
        (a + (b - a) * t / 255) as u8
    } else {
        (a - (a - b) * t / 255) as u8
    }
}

/// 动画预览的状态机（无窗口依赖）。
#[derive(Debug)]
pub struct Preview {
    w: u32,
    h: u32,
    levels: Vec<f32>,
    caps: Vec<f32>,
    vel: Vec<f32>,
    at: Vec<f32>,
    t: f32,
}

impl Preview {
    /// 以画布尺寸构造。
    #[must_use]
    pub fn new(w: u32, h: u32) -> Self {
        Self {
            w,
            h,
            levels: vec![0.0; 160],
            caps: vec![0.0; 160],
            vel: vec![0.0; 160],
            at: vec![0.0; 160],
            t: 0.0,
        }
    }

    /// 推进一帧（与 `analyzer.rs` 的峰值物理同构）。
    pub fn step(&mut self, dt: f32, s: &PreviewStyle) {
        self.t += dt;
        let n = s.bands.min(160);
        let hold = s.hold_ms as f32 / 1000.0;
        for i in 0..n {
            let x = i as f32 / n as f32;
            let low = (-x * 3.0).exp() * 0.62;
            let beat = 0.5 + 0.5 * (self.t * 2.3 + i as f32 * 0.21).sin();
            let mid = 0.5 + 0.5 * (self.t * 4.7 + i as f32 * 0.83).sin();
            let hi = 0.5 + 0.5 * (self.t * 9.1 + i as f32 * 1.9).sin();
            let kick = (self.t * 3.1).sin().max(0.0).powi(8) * (-x * 4.5).exp() * 0.7;
            let target =
                (low * 0.55 + beat * 0.18 + mid * 0.12 + hi * 0.05 + kick * 0.5) * s.sensitivity;
            let target = target.clamp(0.0, 1.0);
            let k = if target > self.levels[i] {
                s.attack
            } else {
                s.release
            };
            self.levels[i] += (target - self.levels[i]) * (k * dt * 12.0).clamp(0.0, 1.0);

            if self.levels[i] >= self.caps[i] {
                self.caps[i] = (self.levels[i] + 0.025).min(1.0);
                self.at[i] = self.t;
                self.vel[i] = 0.0;
            } else if self.t - self.at[i] > hold {
                // 以「帧」为单位（与 analyzer.rs 每帧减 peak_fall 一致）：
                // a = 2v/N 让 N=47 帧的总位移与匀速相当，但前段明显更慢。
                if s.gravity {
                    const GRAVITY_FRAMES: f32 = 47.0;
                    self.vel[i] += s.fall * 2.0 / GRAVITY_FRAMES;
                    self.caps[i] -= self.vel[i];
                } else {
                    self.caps[i] -= s.fall;
                }
                self.caps[i] = self.caps[i].max(self.levels[i]);
            }
        }
    }

    /// 生成一帧 Slint 图像。
    ///
    /// `speed` 为时间轴倍率（慢放 0.35）；`substeps` 模拟两次 Timer 之间
    /// 实际走过的分析帧数，保证 5fps 的 UI 刷新也有连续的物理。
    #[must_use]
    pub fn frame(&mut self, s: &PreviewStyle, speed: f32, substeps: usize) -> Image {
        let dt = 1.0 / 46.9 * speed;
        for _ in 0..substeps.max(1) {
            self.step(dt, s);
        }
        let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(self.w, self.h);
        self.paint(s, buffer.make_mut_bytes());
        Image::from_rgba8(buffer)
    }

    /// 把当前状态画到 RGBA 字节缓冲（`w*h*4`，顺序 r,g,b,a）。
    ///
    /// 抽成独立函数是为了能在没有窗口的单测里做像素级断言。
    #[allow(clippy::many_single_char_names)] // DSP 像素循环里 w/h/x/y/t 是领域惯例
    pub fn paint(&self, s: &PreviewStyle, bytes: &mut [u8]) {
        bytes.fill(0);
        let (w, h) = (self.w, self.h);
        if w == 0 || h == 0 {
            return;
        }
        let n = s.bands.clamp(1, 160);
        let slot = w as f32 / n as f32;
        let gap = (s.gap_ratio * 10.0).round().max(1.0) * (w as f32 / 1100.0).max(0.6);
        let bar_w = (slot - gap).max(1.0) as u32;
        let alpha = (s.opacity.clamp(0.0, 1.0) * 255.0).round() as u8;
        let (r0, g0, b0) = rgb(s.color_start);
        let (r1, g1, b1) = rgb(s.color_end);
        let usable = (h - 2) as f32;
        let cap_h = (h / 32).max(3);
        // 与 spectrum-render 的 bar_color 同构：默认关闭时走原来的双色渐变。
        let multi = s.palette_len >= 3 && s.palette_mode != "off";
        let plen = s.palette_len as usize;

        for i in 0..n {
            let x0 = (i as f32 * slot + gap / 2.0) as u32;
            let bar_h = (self.levels[i] * usable).round() as u32;
            let across_t = (i.saturating_mul(255) / (n - 1).max(1)).min(255) as u32;
            let seg = (plen.saturating_mul(i) / n.max(1)).min(plen.saturating_sub(1));
            for row in 0..bar_h {
                // 渐变按**画布绝对 y**：底部 = color_start，顶部 = color_end。
                let py = h - 1 - row;
                let t = (row * 255 / (h - 1).max(1)).min(255);
                let legacy = (lerp(r0, r1, t), lerp(g0, g1, t), lerp(b0, b1, t));
                let color = if multi {
                    match s.palette_mode.as_str() {
                        "across" => shade3(sample_palette(&s.palette, s.palette_len, across_t), t),
                        "per_bar" => shade3(rgb(s.palette[seg]), t),
                        "vertical" => sample_palette(&s.palette, s.palette_len, t),
                        _ => legacy,
                    }
                } else {
                    legacy
                };
                for x in x0..(x0 + bar_w).min(w) {
                    put(bytes, w, x, py, color, alpha);
                }
            }
            if s.peak_cap && self.caps[i] > 0.02 {
                let cap_y = h - 1 - (self.caps[i] * usable).round() as u32;
                for d in 0..cap_h {
                    let py = cap_y.saturating_add(d).min(h - 1);
                    for x in x0..(x0 + bar_w).min(w) {
                        put(bytes, w, x, py, (248, 248, 248), alpha);
                    }
                }
            }
        }
    }

    /// 当前柱高与滑块高度，供单测与诊断使用。
    #[must_use]
    pub fn state(&self) -> (&[f32], &[f32]) {
        (&self.levels, &self.caps)
    }
}

/// 在多色标上按 `t`（0..=255）插值取色，与渲染层 `sample_palette` 同构。
#[allow(clippy::many_single_char_names)] // 色标插值里 p/len/t/a/b/seg/f 是领域惯例
fn sample_palette(p: &[u32; 8], len: u8, t: u32) -> (u8, u8, u8) {
    let segments = u32::from(len.saturating_sub(1)).max(1);
    let pos = t.min(255).saturating_mul(segments);
    let seg = (pos / 255).min(segments - 1);
    let f = pos.saturating_sub(seg.saturating_mul(255)).min(255);
    let a = rgb(p[seg as usize]);
    let b = rgb(p[(seg + 1) as usize]);
    (lerp(a.0, b.0, f), lerp(a.1, b.1, f), lerp(a.2, b.2, f))
}

/// 柱内明度（底部 72% → 顶部 100%），避免多色柱像平板色块。
fn shade3(c: (u8, u8, u8), height_t: u32) -> (u8, u8, u8) {
    let k = 184 + (height_t.saturating_mul(71) / 255);
    (
        ((u32::from(c.0) * k) / 255) as u8,
        ((u32::from(c.1) * k) / 255) as u8,
        ((u32::from(c.2) * k) / 255) as u8,
    )
}

#[allow(clippy::many_single_char_names)] // 像素坐标用单字符更易读
fn put(bytes: &mut [u8], w: u32, x: u32, y: u32, c: (u8, u8, u8), a: u8) {
    let i = ((y * w + x) * 4) as usize;
    if i + 3 < bytes.len() {
        bytes[i] = c.0;
        bytes[i + 1] = c.1;
        bytes[i + 2] = c.2;
        bytes[i + 3] = a;
    }
}

#[cfg(test)]
mod tests {
    use super::{Preview, PreviewStyle};

    fn warmed(style: &PreviewStyle, frames: usize) -> Preview {
        let mut p = Preview::new(64, 32);
        for _ in 0..frames {
            p.step(1.0 / 46.9, style);
        }
        p
    }

    fn multicolor(style: &mut PreviewStyle, mode: &str) {
        style.palette = [0xFF_0000, 0x00_FF00, 0x00_00FF, 0, 0, 0, 0, 0];
        style.palette_len = 3;
        style.palette_mode = mode.to_owned();
    }

    #[test]
    fn default_preview_keeps_legacy_two_color_gradient() {
        let s = PreviewStyle::default();
        assert_eq!(s.palette_len, 0);
        assert_eq!(s.palette_mode, "off");
    }

    #[test]
    fn multicolor_modes_all_change_the_preview() {
        let mut p = Preview::new(64, 32);
        let mut s = PreviewStyle {
            peak_cap: false,
            ..PreviewStyle::default()
        };
        for _ in 0..12 {
            p.step(1.0 / 46.9, &s);
        }
        let mut legacy = vec![0_u8; 64 * 32 * 4];
        p.paint(&s, &mut legacy);
        let mut frames = Vec::new();
        for mode in ["across", "per_bar", "vertical"] {
            multicolor(&mut s, mode);
            let mut bytes = vec![0_u8; 64 * 32 * 4];
            p.paint(&s, &mut bytes);
            assert_ne!(bytes, legacy, "{mode} 预览应与双色渐变不同");
            frames.push(bytes);
        }
        assert_ne!(frames[0], frames[1], "横向渐变与每柱一色应不同");
        assert_ne!(frames[1], frames[2], "每柱一色与柱内多段应不同");
    }

    #[test]
    fn frame_draws_visible_pixels() {
        let s = PreviewStyle::default();
        let mut p = Preview::new(64, 32);
        let _ = p.frame(&s, 1.0, 9);
        let mut bytes = vec![0_u8; 64 * 32 * 4];
        p.paint(&s, &mut bytes);
        let opaque = bytes.chunks_exact(4).filter(|px| px[3] > 0).count();
        assert!(opaque > 100, "预览不能是空白，实际 {opaque} 个像素");
    }

    #[test]
    fn gradient_uses_canvas_bottom_for_color_start() {
        let s = PreviewStyle {
            color_start: 0x00_00_FF,
            color_end: 0xFF_00_00,
            peak_cap: false,
            ..PreviewStyle::default()
        };
        let mut p = Preview::new(64, 32);
        for v in p.levels.iter_mut().take(8) {
            *v = 1.0;
        }
        let mut bytes = vec![0_u8; 64 * 32 * 4];
        p.paint(&s, &mut bytes);
        // 底部一行应偏蓝（color_start），顶部一行应偏红（color_end）。
        // 扫描实际着色的像素，找最高/最低行，不受顶部留白行数影响。
        let mut painted: Vec<(u32, [u8; 4])> = Vec::new();
        for (idx, px) in bytes.chunks_exact(4).enumerate() {
            if px[3] > 0 {
                let py = (idx / 64) as u32;
                painted.push((py, [px[0], px[1], px[2], px[3]]));
            }
        }
        assert!(!painted.is_empty(), "预览不应为空白");
        let min_y = painted.iter().map(|(y, _)| *y).min().unwrap_or(0);
        let max_y = painted.iter().map(|(y, _)| *y).max().unwrap_or(0);
        let bottom = painted
            .iter()
            .find(|(y, _)| *y == max_y)
            .map_or([0; 4], |(_, c)| *c);
        let top = painted
            .iter()
            .find(|(y, _)| *y == min_y)
            .map_or([0; 4], |(_, c)| *c);
        assert!(bottom[2] > bottom[0], "柱底应偏蓝：{bottom:?}");
        assert!(top[0] > top[2], "柱顶应偏红：{top:?}");
    }

    #[test]
    fn peak_slider_separates_then_stays_above_bar() {
        let s = PreviewStyle::default();
        let p = warmed(&s, 40);
        let (levels, caps) = p.state();
        assert!(
            caps.iter().zip(levels).any(|(c, l)| c > l),
            "滑块应当与柱顶分离"
        );
        assert!(
            caps.iter().zip(levels).all(|(c, l)| c + 1e-6 >= *l),
            "滑块不得穿底"
        );
    }

    #[test]
    fn gravity_falls_slower_at_first() {
        // 关掉悬停期：20 帧只有 0.43s，默认 800ms 会让两档都还没开始落。
        let base = PreviewStyle {
            hold_ms: 0,
            gravity: false,
            ..PreviewStyle::default()
        };
        let linear = warmed(&base, 20);
        let with_gravity = PreviewStyle {
            gravity: true,
            ..base.clone()
        };
        let grav = warmed(&with_gravity, 20);
        let sum = |v: &[f32]| v.iter().sum::<f32>();
        assert!(
            sum(grav.state().1) > sum(linear.state().1),
            "重力档前段应落得更慢"
        );
    }

    #[test]
    fn disabling_peak_cap_removes_white_slider_pixels() {
        let s = PreviewStyle {
            peak_cap: false,
            ..PreviewStyle::default()
        };
        let p = warmed(&s, 30);
        let mut bytes = vec![0_u8; 64 * 32 * 4];
        p.paint(&s, &mut bytes);
        let white = bytes
            .chunks_exact(4)
            .filter(|px| px[3] > 0 && px[0] > 240 && px[1] > 240 && px[2] > 240)
            .count();
        assert_eq!(white, 0, "关闭峰值滑块后不应有白色滑块像素");
    }
}
