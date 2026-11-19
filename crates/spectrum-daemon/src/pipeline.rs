//! 主循环流水线：一段 PCM → 柱高 → 一帧像素。
//!
//! ## 为什么单独成层
//!
//! 「频谱跟着音乐跳」这句话里最容易出错的部分是**顺序与时机**：
//! 一帧 PCM 该喂多少、分析结果什么时候该上屏、静音了要不要停、
//! 停下来之前要不要补一帧零。这些都是纯逻辑，不该绑在声卡和窗口上。
//! 因此本模块零 I/O：输入是 `&[f32]` 与 `now_ms`，输出是柱高与像素缓冲。
//!
//! ## 静音的两次动作（F24 / F34）
//!
//! 静音判定由 [`SilenceGate`] 负责：RMS 低于门限持续 `silence_timeout_ms` 后置为不活跃。
//! 但"不活跃"不等于"不用管"：
//!
//! 1. **刚进入静音**：必须**再上屏一帧全零**，否则窗口上会留着上一帧的柱子，
//!    用户看到"没声音了但柱子还在"（残影）。本模块用 `was_active` 记录边沿。
//! 2. **持续静音**：`should_present = false`，调用方应停止上屏（0 fps，省 CPU/GPU，F34）。
//!
//! 这两条各有单测覆盖。

// RMS 与 bin→Hz 换算中的 `usize/u32 -> f32` 是算法固有需求：
// 帧数受 hop 约束（≤16k）、FFT 点数 ≤8192、采样率 ≤192000，全部远小于 f32
// 精确表示的 2^24，转换不会丢有效位。
#![allow(clippy::cast_precision_loss)]

use spectrum_core::CoreError;
use spectrum_core::analyzer::{Analyzer, AnalyzerConfig};
use spectrum_core::capture::{DEFAULT_SILENCE_DBFS, DEFAULT_SILENCE_TIMEOUT_MS, SilenceGate};
use spectrum_render::{RenderStats, RenderStyle, SpectrumRenderer, Surface};

/// 流水线参数。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PipelineConfig {
    /// 采集速率（Hz）。
    pub sample_rate: u32,
    /// 每次喂给分析器的样本数。默认 1024（48 kHz 下 21.3ms，约 47 帧/秒）。
    pub hop_frames: usize,
    /// 画布宽（物理像素）。
    pub width: u16,
    /// 画布高（物理像素）。
    pub height: u16,
    /// 静音门限（dBFS）。
    pub silence_dbfs: f32,
    /// 静音超时（毫秒）。
    pub silence_timeout_ms: u64,
    /// 渲染样式。
    pub style: RenderStyle,
}

impl PipelineConfig {
    /// 以真实任务栏尺寸构造默认参数。
    #[must_use]
    pub fn for_geometry(width: u16, height: u16) -> Self {
        Self {
            sample_rate: 48_000,
            hop_frames: 1024,
            width,
            height,
            silence_dbfs: DEFAULT_SILENCE_DBFS,
            silence_timeout_ms: DEFAULT_SILENCE_TIMEOUT_MS,
            style: RenderStyle::new(),
        }
    }
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self::for_geometry(3200, 87)
    }
}

/// 一帧的处理结果。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FrameOutcome {
    /// 本帧各频段柱高 `[0,1]`。
    pub bands: Vec<f32>,
    /// 渲染统计；未上屏时为 `None`（省掉一次无意义的像素计算）。
    pub stats: Option<RenderStats>,
    /// 静音门限当前是否活跃。
    pub active: bool,
    /// 本帧是否需要上屏。
    ///
    /// `false` 有三种情形：持续静音、尺寸为 0、本帧 PCM 太短无法分析。
    pub should_present: bool,
    /// 最响频段的中心频率（Hz）；全静音时为 `None`。
    pub peak_hz: Option<f32>,
}

/// 流水线主体的统计，供 `doctor` 输出。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PipelineStats {
    /// 累计处理帧数。
    pub frames: u64,
    /// 累计上屏次数。
    pub presented: u64,
    /// 因静音被跳过的上屏次数。
    pub skipped_silent: u64,
    /// 真正跑了 FFT 的帧数。
    pub analyzed: u64,
    /// 因持续静音而**跳过** FFT 的帧数（省 CPU 的关键指标）。
    pub analyzed_skipped: u64,
}

/// PCM → 柱高 → 像素。
pub struct Pipeline {
    cfg: PipelineConfig,
    analyzer: Analyzer,
    analyzer_cfg: AnalyzerConfig,
    renderer: SpectrumRenderer,
    surface: Surface,
    gate: SilenceGate,
    /// 分析器的 FFT 点数（`peak_hz` 需要它换算 bin → Hz）。
    fft_size: usize,
    /// 上一帧是否活跃（用于捕捉"刚进入静音"的边沿）。
    was_active: bool,
    stats: PipelineStats,
}

impl Pipeline {
    /// 建立流水线。
    ///
    /// # Errors
    ///
    /// 分析器配置非法（FFT 点数非 2 的幂等）时返回错误。
    pub fn new(cfg: PipelineConfig) -> Result<Self, CoreError> {
        let analyzer_cfg = AnalyzerConfig::default();
        let analyzer = Analyzer::new(&analyzer_cfg, cfg.sample_rate)?;
        let fft_size = analyzer_cfg.fft_size;
        Ok(Self {
            cfg,
            analyzer,
            analyzer_cfg,
            fft_size,
            renderer: SpectrumRenderer::new(cfg.style),
            surface: Surface::new(cfg.width, cfg.height),
            gate: SilenceGate::new(cfg.silence_dbfs, cfg.silence_timeout_ms),
            was_active: false,
            stats: PipelineStats::default(),
        })
    }

    /// 送一帧 PCM 进来，产出本帧结果。
    ///
    /// `pcm` 长度不必等于 `hop_frames`：分析器本身取"最近 `fft_size` 个样本"，
    /// 因此长帧短帧都能处理（短帧尾部补零，见 [`Analyzer::process`]）。
    pub fn push(&mut self, pcm: &[f32], now_ms: u64) -> FrameOutcome {
        let mut outcome = FrameOutcome::default();
        self.push_into(pcm, now_ms, &mut outcome);
        outcome
    }

    /// 与 [`Pipeline::push`] 相同，但把结果写进可复用的 `outcome`。
    ///
    /// 主循环每帧调用它：`outcome.bands` 的容量跨帧复用，静音帧只做
    /// `resize` 不重新分配；`push` 仅保留给示例/测试等一次性调用。
    pub fn push_into(&mut self, pcm: &[f32], now_ms: u64, outcome: &mut FrameOutcome) {
        self.stats.frames += 1;

        // 1) 静音门限：喂本轮 RMS，让它自己累计"多久没声音"
        let rms = rms_of(pcm);
        self.gate.feed(rms, now_ms);
        let active = self.gate.active(now_ms);

        // 2) 上屏决策：活跃 → 上屏；刚掉到静音 → 补一帧零清屏；持续静音 → 跳过
        let entering_silence = self.was_active && !active;
        self.was_active = active;

        // 3) 分析；复用 `outcome.bands` 的缓冲。
        let band_count = self.analyzer.band_count();
        outcome.bands.clear();
        if entering_silence {
            // ★ 进入静音必须**清掉平滑残值**。
            // release 平滑是"慢慢落"，gate 判静音时 levels 还有个位数百分比的残余，
            // 而 min_bar_px=1 会把任何 >0 的柱都画成至少 1px ——
            // 实测清屏帧仍残留 6060 个不透明像素，画面看着像"声音停了但柱子还在"。
            self.analyzer.reset();
            outcome.bands.resize(band_count, 0.0);
        } else if !active {
            // ★ 持续静音：**跳过 FFT**。
            // 上一版在这里无脑跑 4096 点 FFT（47 次/秒，输入全是 0），
            // 实测静音路径仍占 ~4.8% CPU —— 与文档承诺的「静音 ≈0% CPU」（F34）直接冲突。
            // 边沿处已 `reset()`，levels 已全 0，这里直接交零即可。
            self.stats.analyzed_skipped += 1;
            outcome.bands.resize(band_count, 0.0);
        } else {
            // 有声时必须跑分析（否则重新有声音时平滑状态是旧的，
            // 第一帧会从 0 猛跳，看起来像"炸一下"）
            self.stats.analyzed += 1;
            outcome
                .bands
                .extend_from_slice(self.analyzer.process(pcm, now_ms));
        }
        let peak_hz = peak_band_hz(
            &outcome.bands,
            self.analyzer.edges(),
            self.cfg.sample_rate,
            self.fft_size,
        );

        let should_present = active || entering_silence;

        outcome.stats = if should_present {
            self.surface.resize(self.cfg.width, self.cfg.height);
            let stats = self.renderer.render_with_peaks(
                &mut self.surface,
                &outcome.bands,
                self.analyzer.peaks(),
            );
            self.stats.presented += 1;
            Some(stats)
        } else {
            self.stats.skipped_silent += 1;
            None
        };
        outcome.active = active;
        outcome.should_present = should_present;
        outcome.peak_hz = peak_hz;
    }

    /// 最近一帧的像素缓冲（行主序、预乘 ARGB）。
    #[must_use]
    pub fn surface(&self) -> &Surface {
        &self.surface
    }

    /// 立即清空画面，并把分析器/静音门限恢复到「静音」。
    ///
    /// 断流时必须显式调用：断流后没有 PCM 可推进，最后一帧会**冻在屏幕上**，
    /// 用户看到的是"音乐断了但柱子还举着"，很容易被当成程序卡死。
    ///
    /// 同时重置平滑与门限，避免恢复后第一帧拿旧状态去插值、
    /// 从半空的高度"炸"一下。
    pub fn clear(&mut self, now_ms: u64) -> &Surface {
        self.analyzer.reset();
        self.gate.mute(now_ms);
        self.was_active = false;
        let zeros = vec![0.0_f32; self.analyzer.band_count()];
        self.surface.resize(self.cfg.width, self.cfg.height);
        self.renderer.render(&mut self.surface, &zeros);
        &self.surface
    }

    /// 当前柱高（未平滑前的最近一次结果）。
    #[must_use]
    pub fn bands(&self) -> &[f32] {
        self.analyzer.levels()
    }

    /// 累计统计。
    #[must_use]
    pub const fn stats(&self) -> PipelineStats {
        self.stats
    }

    /// 当前参数。
    #[must_use]
    pub const fn config(&self) -> &PipelineConfig {
        &self.cfg
    }

    /// 画布尺寸变化（任务栏高度被拖动时调用）。
    ///
    /// 只记录新尺寸并重建像素缓冲，**不**重算分析器 ——
    /// FFT 点数与采样率无关，尺寸变化纯属渲染侧的事。
    pub fn resize(&mut self, width: u16, height: u16) {
        self.cfg.width = width;
        self.cfg.height = height;
        self.surface.resize(width, height);
    }

    /// 当前柱高是否全为 0（供静音清理的测试断言）。
    #[cfg(test)]
    fn analyzer_bands_all_zero(&self) -> bool {
        self.analyzer.levels().iter().all(|&v| v == 0.0)
    }

    /// 分析器当前柱数（渲染层的柱数必须与它一致，否则布局会错位）。
    #[must_use]
    pub fn band_count(&self) -> usize {
        self.analyzer.band_count()
    }

    /// 当前生效的 FFT 后端名（doctor 输出与打包校验用）。
    #[must_use]
    pub fn fft_backend_name(&self) -> &'static str {
        self.analyzer.fft_backend_name()
    }

    /// 当前分析器参数。
    #[must_use]
    pub const fn analyzer_config(&self) -> &AnalyzerConfig {
        &self.analyzer_cfg
    }

    /// **只**换渲染样式（配色 / 透明度 / 间隙）。
    ///
    /// 不动分析器与静音门限 —— 这是热重载最常见的一档（用户拖个颜色），
    /// 动分析器会重置平滑状态，画面会"跳一下"。
    pub fn apply_style(&mut self, style: RenderStyle) {
        self.cfg.style = style;
        self.renderer = SpectrumRenderer::new(style);
    }

    /// 按新参数重建分析器（柱数 / FFT 点数 / 平滑系数变了时用）。
    ///
    /// 重建会清掉平滑与峰值保持的状态 —— 这是**故意**的：
    /// 新参数下的旧平滑值没有意义，留着会让第一帧从半空高度"炸"起来。
    ///
    /// # Errors
    ///
    /// 分析器参数非法时返回错误，并**保留原有分析器**（不半途换掉）。
    pub fn reconfigure_analyzer(&mut self, cfg: &AnalyzerConfig) -> Result<(), CoreError> {
        let analyzer = Analyzer::new(cfg, self.cfg.sample_rate)?;
        self.fft_size = cfg.fft_size;
        self.analyzer_cfg = cfg.clone();
        self.analyzer = analyzer;
        Ok(())
    }

    /// 换静音门限与超时。
    ///
    /// 重建门限意味着"是否处于静音"要重新判定：阈值变了，
    /// 之前积累的静音时长不再可信。
    pub fn set_silence(&mut self, dbfs: f32, timeout_ms: u64) {
        self.cfg.silence_dbfs = dbfs;
        self.cfg.silence_timeout_ms = timeout_ms;
        self.gate = SilenceGate::new(dbfs, timeout_ms);
        self.was_active = false;
    }
}

impl std::fmt::Debug for Pipeline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pipeline")
            .field("cfg", &self.cfg)
            .field("bands", &self.analyzer.band_count())
            .field("stats", &self.stats)
            .finish_non_exhaustive()
    }
}

/// 均方根（线性，非 dB）。
fn rms_of(pcm: &[f32]) -> f32 {
    if pcm.is_empty() {
        return 0.0;
    }
    let sum: f32 = pcm.iter().map(|s| s * s).sum();
    (sum / pcm.len() as f32).sqrt()
}

/// 最响频段的中心频率。
///
/// `edges` 是 bin 区间，bin 宽度 = `sample_rate / fft_size`，
/// 因此必须显式知道 `fft_size` —— 想从 `edges` 反推既绕又容易算错
/// （`edges` 只覆盖到 `f_max`，最大上界不是 `fft_size/2`）。
///
/// 全 0（或全非有限值）时返回 `None`：**不要**返回 0 Hz ——
/// 0 Hz 会被上游误当成"有一个很强的低频"。
fn peak_band_hz(
    bands: &[f32],
    edges: &[(usize, usize)],
    sample_rate: u32,
    fft_size: usize,
) -> Option<f32> {
    let (index, level) = bands
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))?;
    if !level.is_finite() || *level <= 0.0 {
        return None;
    }
    let (lo, hi) = *edges.get(index)?;
    let center_bin = (lo + hi) as f32 / 2.0;
    let bin_hz = sample_rate as f32 / fft_size.max(1) as f32;
    Some(center_bin * bin_hz)
}

#[cfg(test)]
mod tests {
    use super::{FrameOutcome, Pipeline, PipelineConfig, peak_band_hz, rms_of};
    use spectrum_core::analyzer::sine_wave;

    const RATE: u32 = 48_000;

    fn pipeline(width: u16, height: u16) -> Pipeline {
        let mut cfg = PipelineConfig::for_geometry(width, height);
        cfg.sample_rate = RATE;
        cfg.hop_frames = 1024;
        Pipeline::new(cfg).expect("pipeline")
    }

    #[test]
    fn push_into_reuses_bands_buffer() {
        let mut pipeline = Pipeline::new(PipelineConfig::default()).expect("构造流水线");
        let mut outcome = FrameOutcome::default();
        pipeline.push_into(&[0.0; 1024], 0, &mut outcome);
        let capacity = outcome.bands.capacity();
        assert!(capacity >= outcome.bands.len());
        pipeline.push_into(&[0.0; 1024], 100, &mut outcome);
        assert_eq!(
            outcome.bands.capacity(),
            capacity,
            "静音帧只 resize，不应重新分配 bands"
        );
    }

    #[test]
    fn rms_matches_known_sine() {
        // 正弦波 RMS = 幅度 / √2
        let tone = sine_wave(1_000.0, RATE, 4_800);
        let expected = 1.0 / std::f32::consts::SQRT_2;
        assert!(
            (rms_of(&tone) - expected).abs() < 0.01,
            "rms={}",
            rms_of(&tone)
        );
    }

    #[test]
    fn rms_of_empty_is_zero() {
        assert!(rms_of(&[]).abs() < f32::EPSILON);
    }

    #[test]
    fn tone_peaks_at_its_own_band() {
        let mut p = pipeline(320, 60);
        let tone = sine_wave(1_000.0, RATE, 9_600);
        // 喂足够多帧让 attack/release 平滑收敛
        let mut last = None;
        for step in 0..20_u64 {
            let start = usize::try_from(step).unwrap_or(0) * 1024 % 4_800;
            last = Some(p.push(&tone[start..start + 1024], step * 21));
        }
        let out = last.expect("at least one frame");
        let argmax = out
            .bands
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(i, _)| i)
            .expect("bands");
        let hz = out.peak_hz.expect("有声音时应有主频");
        // 1 kHz 落在正确频段：允许 20% 的频段宽度误差
        assert!(
            (hz - 1_000.0).abs() < 250.0,
            "主频 {hz}Hz 偏离 1kHz 太多（argmax band #{argmax}）"
        );
    }

    #[test]
    fn silent_frames_stop_presenting_after_timeout() {
        let mut p = pipeline(320, 60);
        // 先制造声音，让状态变活跃
        let tone = sine_wave(1_000.0, RATE, 4_800);
        let first = p.push(&tone[..1024], 0);
        assert!(first.active, "有声音时应活跃");
        assert!(first.should_present);

        // 然后持续静音，超过 timeout（默认 800ms）后应停止上屏
        let silence = vec![0.0_f32; 1024];
        let mut last = None;
        for step in 1..30_u64 {
            last = Some(p.push(&silence, step * 50));
        }
        let out = last.expect("frame");
        assert!(!out.active, "静音超时后不应活跃");
        assert!(!out.should_present, "持续静音应停止上屏（F34）");
        assert!(out.stats.is_none(), "跳过上屏时不应做像素计算");
    }

    #[test]
    fn entering_silence_presents_one_zero_frame() {
        let mut p = pipeline(320, 60);
        let tone = sine_wave(1_000.0, RATE, 4_800);
        p.push(&tone[..1024], 0);
        assert!(p.stats().presented > 0);

        // 静音到刚好越过 timeout 的那一帧，必须**再上屏一次**（清屏）
        let silence = vec![0.0_f32; 1024];
        let mut cleared = false;
        for step in 1..40_u64 {
            let out = p.push(&silence, step * 50);
            if !out.active {
                assert!(out.should_present, "刚进入静音时必须补一帧清屏，否则留残影");
                cleared = true;
                break;
            }
        }
        assert!(cleared, "测试未走到静音边沿");

        // 清屏帧必须**真的**全零：不只是 should_present=true，
        // 而是 bands 全 0 且渲染后像素全透明（M44：release 尾音会让它不干净）
        assert!(
            p.analyzer_bands_all_zero(),
            "进入静音后柱高必须归零，否则 min_bar_px 会留下一排 1px 的柱子"
        );

        // 之后继续静音 → 不再上屏
        let after = p.push(&silence, 5_000);
        assert!(!after.should_present);
    }

    #[test]
    fn clearing_frame_has_no_opaque_pixels() {
        let mut p = pipeline(320, 60);
        let tone = sine_wave(1_000.0, RATE, 4_800);
        p.push(&tone[..1024], 0);

        let silence = vec![0.0_f32; 1024];
        let mut edge = None;
        for step in 1..40_u64 {
            let out = p.push(&silence, step * 50);
            if !out.active && out.should_present {
                edge = Some(out);
                break;
            }
        }
        let edge = edge.expect("应捕捉到静音边沿");
        assert!(edge.bands.iter().all(|&b| b == 0.0), "清屏帧柱高必须全 0");
        let lit = p
            .surface()
            .pixels()
            .iter()
            .filter(|&&px| px >> 24 != 0)
            .count();
        assert_eq!(lit, 0, "清屏帧不应留下任何不透明像素（残留 {lit}）");
    }

    #[test]
    fn clear_wipes_surface_and_levels() {
        // 断流时调用 `clear()`：画面必须立刻全透明，柱高必须归零。
        // 若不重置分析器，恢复后第一帧会拿断流前的平滑值去插值，
        // 表现为"啪一下从半空跳出来"。
        let mut p = pipeline(320, 60);
        let tone = sine_wave(1_000.0, RATE, 4_800);
        for step in 0..6_u64 {
            let start = usize::try_from(step).unwrap_or(0) * 1024 % 4_800;
            let end = (start + 1024).min(4_800);
            p.push(&tone[start..end], step * 25);
        }
        let lit_before = p
            .surface()
            .pixels()
            .iter()
            .filter(|&&px| px >> 24 != 0)
            .count();
        assert!(lit_before > 0, "前置条件：清屏前应有可见像素");

        let surface = p.clear(1_000);
        let lit_after = surface.pixels().iter().filter(|&&px| px >> 24 != 0).count();
        assert_eq!(
            lit_after, 0,
            "清屏后不应有任何不透明像素（残留 {lit_after}）"
        );
        assert!(p.analyzer_bands_all_zero(), "清屏后柱高必须全 0");

        // 清屏后门限也被置为静音：直接再喂静音不应重新"进入静音边沿"
        let out = p.push(&vec![0.0_f32; 1024], 1_100);
        assert!(!out.active, "清屏后应处于静音状态");
        assert!(!out.should_present, "持续静音不应上屏");
    }

    #[test]
    fn sustained_silence_skips_fft() {
        // 这条测试守护一个真实的性能缺陷：静音期曾经每帧都跑 4096 点 FFT，
        // 实测让「静音」也占 ~4.8% CPU（debug）。跳过分析后应当不再增长 analyzed。
        let mut p = pipeline(320, 60);
        let tone = sine_wave(1_000.0, RATE, 4_800);
        p.push(&tone[..1024], 0);

        let silence = vec![0.0_f32; 1024];
        // 先跑到静音边沿
        let mut edge_at = None;
        for step in 1..40_u64 {
            let out = p.push(&silence, step * 50);
            if !out.active {
                edge_at = Some(step);
                break;
            }
        }
        assert!(edge_at.is_some(), "测试未走到静音边沿");

        let analyzed_at_edge = p.stats().analyzed;
        let skipped_at_edge = p.stats().analyzed_skipped;
        // 再喂 50 帧静音
        for step in 0..50_u64 {
            p.push(&silence, 5_000 + step * 50);
        }
        assert_eq!(p.stats().analyzed, analyzed_at_edge, "持续静音不应再跑 FFT");
        assert!(
            p.stats().analyzed_skipped > skipped_at_edge,
            "静音帧应被计入 analyzed_skipped"
        );
        // 而且柱高必须仍是全 0（不能因为跳过分析就把旧值留着）
        assert!(p.analyzer_bands_all_zero());
    }

    #[test]
    fn band_heights_reach_renderer() {
        let mut p = pipeline(320, 60);
        let tone = sine_wave(1_000.0, RATE, 9_600);
        let mut last = None;
        for step in 0..15_u64 {
            let start = usize::try_from(step).unwrap_or(0) * 1024 % 4_800;
            last = Some(p.push(&tone[start..start + 1024], step * 21));
        }
        let out = last.expect("frame");
        let stats = out.stats.expect("活跃时应上屏");
        assert_eq!(stats.bars, p.band_count());
        // 至少有一个柱被画出来
        assert!(stats.max_bar_px > 0, "有声音时最高柱应 > 0");
        assert!(stats.drawn_pixels > 0);
    }

    #[test]
    fn resize_keeps_band_count() {
        let mut p = pipeline(320, 60);
        let before = p.band_count();
        p.resize(320, 120);
        assert_eq!(p.band_count(), before, "改画布尺寸不应改频段划分");
        assert_eq!(p.surface().height(), 120);
    }

    #[test]
    fn peak_hz_is_none_when_silent() {
        assert!(peak_band_hz(&[0.0; 8], &[(1, 2); 8], RATE, 4096).is_none());
        assert!(peak_band_hz(&[], &[], RATE, 4096).is_none());
    }

    #[test]
    fn apply_style_changes_colors_without_touching_bands() {
        let mut p = pipeline(320, 60);
        let before = p.band_count();
        let mut style = spectrum_render::RenderStyle::new();
        style.color_bottom = 0x00_00FF;
        style.alpha = 255;
        p.apply_style(style);
        assert_eq!(p.config().style.color_bottom, 0x00_00FF);
        assert_eq!(p.config().style.alpha, 255);
        assert_eq!(p.band_count(), before, "换样式不该改频段");
    }

    #[test]
    fn reconfigure_analyzer_changes_band_count() {
        let mut p = pipeline(320, 60);
        let before = p.band_count();
        let cfg = spectrum_core::analyzer::AnalyzerConfig {
            bands: 32,
            ..spectrum_core::analyzer::AnalyzerConfig::default()
        };
        p.reconfigure_analyzer(&cfg).expect("重建分析器");
        // 实际柱数 = build_edges 去重后的长度（F56：低频段会塌缩），
        // 因此不能直接断言等于 32
        let expected = spectrum_core::analyzer::build_edges(&cfg, RATE).len();
        assert_eq!(p.band_count(), expected);
        assert_ne!(p.band_count(), before);
        assert_eq!(p.analyzer_config().bands, 32);
    }

    #[test]
    fn reconfigure_analyzer_rejects_invalid_and_keeps_old() {
        let mut p = pipeline(320, 60);
        let before = p.band_count();
        let cfg = spectrum_core::analyzer::AnalyzerConfig {
            fft_size: 3000, // 不是 2 的幂
            ..spectrum_core::analyzer::AnalyzerConfig::default()
        };
        assert!(p.reconfigure_analyzer(&cfg).is_err());
        assert_eq!(p.band_count(), before, "非法参数不得把分析器换掉");
    }

    #[test]
    fn set_silence_updates_gate_parameters() {
        let mut p = pipeline(320, 60);
        p.set_silence(-50.0, 1_234);
        assert!((p.config().silence_dbfs + 50.0).abs() < f32::EPSILON);
        assert_eq!(p.config().silence_timeout_ms, 1_234);
    }
}
