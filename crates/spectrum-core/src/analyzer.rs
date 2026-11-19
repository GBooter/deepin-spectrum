//! 频谱分析：频段边界（F56 去重）、功率谱聚合、dB 归一化、时间平滑与峰值保持（§6.2）。

// 公开为可替换后端接口：阶段 2 前可在真机用 `realfft 3.5.1` 做数值/性能对比（N15），
// 替换只发生在 `fft::power_spectrum` 内部，上层 `Analyzer` 无需改动。
pub mod fft;

use crate::CoreError;
use std::f32::consts::PI;

/// 频段数上限（§6.8：`bands` 是上限，实际柱数由 [`build_edges`] 决定）。
pub const MAX_BANDS: usize = 128;

/// FFT 点数上限。
///
/// 配置文件可以绕过 GUI 直接写 `fft_size`；没有上限时 2^20 点会每 21ms
/// 跑一次百万点 FFT，CPU/内存直接失控。8192 与 GUI 最高档一致。
pub const MAX_FFT_SIZE: usize = 8192;

/// 分析器参数（§6.2.2）。
#[derive(Debug, Clone, PartialEq)]
pub struct AnalyzerConfig {
    /// FFT 点数，默认 4096（★ F56：2048 会让低频段塌缩）。
    pub fft_size: usize,
    /// 目标频段数上限，默认 64。
    pub bands: usize,
    /// 最低频率（Hz），默认 40.0。
    pub f_min: f32,
    /// 最高频率（Hz），默认 16000.0。
    pub f_max: f32,
    /// dB 下限，默认 -80.0。
    pub db_floor: f32,
    /// 上升平滑系数，默认 0.55。
    pub attack: f32,
    /// 下降平滑系数，默认 0.12。
    pub release: f32,
    /// 峰值保持时长（毫秒），默认 800。
    pub peak_hold_ms: u32,
    /// 峰值滑块每帧回落量，默认 `0.02`（等于 P1 之前的硬编码常量）。
    ///
    /// 合法区间 `0.002..=0.05`；越小落得越慢。
    pub peak_fall: f32,
    /// 峰值滑块是否使用重力加速（`true` = 从静止起越落越快）。
    ///
    /// 默认 `false`（匀速回落），保证旧配置的逐帧行为不发生变化。
    pub peak_gravity: bool,
    /// 视觉 gamma 补偿，默认 0.8。
    pub gamma: f32,
    /// 灵敏度倍数，默认 1.0（范围 0.2~3.0）。
    pub sensitivity: f32,
}

impl Default for AnalyzerConfig {
    fn default() -> Self {
        Self {
            fft_size: 4096,
            bands: 64,
            f_min: 40.0,
            f_max: 16_000.0,
            db_floor: -80.0,
            attack: 0.55,
            release: 0.12,
            peak_hold_ms: 800,
            peak_fall: 0.02,
            peak_gravity: false,
            gamma: 0.8,
            sensitivity: 1.0,
        }
    }
}

impl AnalyzerConfig {
    /// 低延迟档（§6.2.2）：2048 + `f_min=80`，牺牲低频分辨率。
    #[must_use]
    pub fn low_latency() -> Self {
        Self {
            fft_size: 2048,
            f_min: 80.0,
            ..Self::default()
        }
    }

    /// 高分辨率档（§6.2.2）：8192 + `f_min=30`，延迟明显，不推荐常驻。
    #[must_use]
    pub fn high_resolution() -> Self {
        Self {
            fft_size: 8192,
            f_min: 30.0,
            ..Self::default()
        }
    }

    /// 参数校验；`Analyzer::new` 会调用它，配置层也用它做非法值回退前的判定。
    pub fn validate(&self) -> Result<(), CoreError> {
        if self.fft_size < 2 || !self.fft_size.is_power_of_two() || self.fft_size > MAX_FFT_SIZE {
            return Err(CoreError::InvalidFftSize(self.fft_size));
        }
        if !(1..=MAX_BANDS).contains(&self.bands) {
            return Err(CoreError::InvalidBandCount(self.bands));
        }
        if !self.peak_fall.is_finite() || !(0.002..=0.05).contains(&self.peak_fall) {
            return Err(CoreError::InvalidPeakFall(self.peak_fall));
        }
        if !self.f_min.is_finite()
            || !self.f_max.is_finite()
            || self.f_min <= 0.0
            || self.f_max <= self.f_min
        {
            return Err(CoreError::InvalidFrequencyRange {
                f_min: self.f_min,
                f_max: self.f_max,
            });
        }
        Ok(())
    }
}

/// 计算**去重后**的 bin 区间 `(lo, hi)`，`hi` 为开区间上界（§6.2.3 / F56）。
///
/// 返回长度即「实际柱数」，可能小于 `cfg.bands`；渲染层必须用本函数返回的
/// 长度算柱宽，而不是 `cfg.bands`，否则右侧会留一段空白。
/// 非法参数返回空 `Vec`（由 [`AnalyzerConfig::validate`] 给出具体错误）。
#[must_use]
pub fn build_edges(cfg: &AnalyzerConfig, sample_rate: u32) -> Vec<(usize, usize)> {
    if sample_rate == 0 || cfg.validate().is_err() {
        return Vec::new();
    }
    let nyq = sample_rate as f32 / 2.0;
    let f_max = cfg.f_max.min(nyq);
    let (lo, hi) = (cfg.f_min.ln(), f_max.ln());
    let k = cfg.fft_size as f32 / sample_rate as f32;
    let max_bin = cfg.fft_size / 2;

    let mut out: Vec<(usize, usize)> = Vec::with_capacity(cfg.bands);
    for i in 0..cfg.bands {
        let f0 = (lo + (hi - lo) * i as f32 / cfg.bands as f32).exp();
        let f1 = (lo + (hi - lo) * (i + 1) as f32 / cfg.bands as f32).exp();
        let b0 = ((f0 * k) as usize).min(max_bin);
        let b1 = (((f1 * k) as usize).max(b0 + 1)).min(max_bin + 1);
        if b1 <= b0 {
            continue;
        }
        // ★ F56：与上一段完全相同 → 合并，否则低频会出现多根同步跳动的柱子
        if out.last() == Some(&(b0, b1)) {
            continue;
        }
        out.push((b0, b1));
    }
    out
}

/// 频谱分析器；所有状态在实例内，不依赖全局静态（F55 的多实例要求）。
#[derive(Debug, Clone)]
pub struct Analyzer {
    cfg: AnalyzerConfig,
    sample_rate: u32,
    edges: Vec<(usize, usize)>,
    window: Vec<f32>,
    backend: fft::FftBackend,
    levels: Vec<f32>,
    peaks: Vec<f32>,
    peak_vel: Vec<f32>,
    peak_time_ms: Vec<u64>,
}

impl Analyzer {
    /// 以配置与**实际设备采样率**构造（★ F6：绝不硬编码 48000）。
    pub fn new(cfg: &AnalyzerConfig, sample_rate: u32) -> Result<Self, CoreError> {
        cfg.validate()?;
        if sample_rate == 0 {
            return Err(CoreError::InvalidSampleRate(sample_rate));
        }
        let edges = build_edges(cfg, sample_rate);
        let count = edges.len();
        let window = fft::hann(cfg.fft_size);
        let backend = fft::FftBackend::try_new(cfg.fft_size, &window)?;
        Ok(Self {
            cfg: cfg.clone(),
            sample_rate,
            edges,
            window,
            backend,
            levels: vec![0.0; count],
            peaks: vec![0.0; count],
            peak_vel: vec![0.0; count],
            peak_time_ms: vec![0; count],
        })
    }

    /// 处理一帧 PCM，返回平滑后的归一化柱高 `[0,1]`。
    ///
    /// 输入超过 `fft_size` 时取**最近** `fft_size` 帧（消费端 skip 语义），
    /// 不足时尾部补零；`now_ms` 用于峰值保持计时。
    pub fn process(&mut self, pcm: &[f32], now_ms: u64) -> &[f32] {
        let n = self.cfg.fft_size;
        let tail = if pcm.len() > n {
            &pcm[pcm.len() - n..]
        } else {
            pcm
        };
        let power = self.backend.compute(tail, &self.window);

        for (i, &(b0, b1)) in self.edges.iter().enumerate() {
            let hi = b1.min(power.len());
            let lo = b0.min(hi);
            let slice = &power[lo..hi];
            let avg = if slice.is_empty() {
                0.0
            } else {
                slice.iter().sum::<f32>() / slice.len() as f32
            };

            // 功率 → dB → 归一化 [0,1] → 灵敏度 → gamma 视觉补偿
            // ★ 静音时 avg==0 必须直接落到 db_floor；若用 `log10(avg+EPSILON)`，
            //   f32::EPSILON 会被算成约 -69dB，静音帧反而出现 0.13 的伪底噪柱。
            let db = if avg <= 1.0e-20 {
                self.cfg.db_floor
            } else {
                10.0 * avg.log10()
            };
            let norm = ((db - self.cfg.db_floor) / -self.cfg.db_floor).clamp(0.0, 1.0);
            let target = (norm * self.cfg.sensitivity)
                .clamp(0.0, 1.0)
                .powf(self.cfg.gamma);

            // §6.2.4 时间平滑：快起慢落
            let k = if target > self.levels[i] {
                self.cfg.attack
            } else {
                self.cfg.release
            }
            .clamp(0.0, 1.0);
            self.levels[i] += (target - self.levels[i]) * k;

            // §6.2.4 峰值保持 + P1「峰值滑块 · 悬浮缓落」。
            // 柱顶超过滑块时把它顶上去（顶飞），悬停 peak_hold_ms 后开始回落；
            // 回落分匀速 / 重力两档，且始终不低于当前柱高（不穿底）。
            if self.levels[i] >= self.peaks[i] {
                self.peaks[i] = self.levels[i];
                self.peak_vel[i] = 0.0;
                self.peak_time_ms[i] = now_ms;
            } else if now_ms.saturating_sub(self.peak_time_ms[i]) > u64::from(self.cfg.peak_hold_ms)
            {
                if self.cfg.peak_gravity {
                    // 47 帧 ≈ 1 秒 @46.9fps；a = 2v/N 让 1 秒总位移与匀速档相当，
                    // 但前段明显更慢 —— 这就是「缓慢落下来」的手感来源。
                    const GRAVITY_FRAMES: f32 = 47.0;
                    self.peak_vel[i] += self.cfg.peak_fall * 2.0 / GRAVITY_FRAMES;
                    self.peaks[i] -= self.peak_vel[i];
                } else {
                    self.peaks[i] -= self.cfg.peak_fall;
                }
                self.peaks[i] = self.peaks[i].max(self.levels[i]);
            }
        }
        &self.levels
    }

    /// 实际柱数（去重后），渲染层用它算柱宽。
    #[must_use]
    pub fn band_count(&self) -> usize {
        self.edges.len()
    }

    /// 去重后的 bin 区间。
    #[must_use]
    pub fn edges(&self) -> &[(usize, usize)] {
        &self.edges
    }

    /// 峰值帽高度。
    #[must_use]
    pub fn peaks(&self) -> &[f32] {
        &self.peaks
    }

    /// 当前柱高。
    #[must_use]
    pub fn levels(&self) -> &[f32] {
        &self.levels
    }

    /// 构造时的设备采样率。
    #[must_use]
    pub const fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// 当前生效的 FFT 后端名，供 `doctor` JSON 的 `fft_backend` 字段使用。
    #[must_use]
    pub fn fft_backend_name(&self) -> &'static str {
        self.backend.name()
    }

    /// 清空平滑与峰值状态（采样率变更 / 长时间静音后恢复时调用）。
    pub fn reset(&mut self) {
        self.levels.fill(0.0);
        self.peaks.fill(0.0);
        self.peak_vel.fill(0.0);
        self.peak_time_ms.fill(0);
    }
}

/// 生成单频正弦 PCM，供测试与 `doctor` 自检使用。
#[must_use]
pub fn sine_wave(freq: f32, sample_rate: u32, frames: usize) -> Vec<f32> {
    (0..frames)
        .map(|i| (2.0 * PI * freq * i as f32 / sample_rate as f32).sin())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{Analyzer, AnalyzerConfig, build_edges, sine_wave};

    const RATE: u32 = 48_000;

    /// §6.2.2 的实算柱数表；这张表同时校验 `build_edges` 与文档的 F117 修正。
    #[test]
    fn f56_edge_count_matches_document_table() {
        let cases: [(usize, f32, u32, usize, &str); 11] = [
            (2048, 30.0, 48_000, 52, "2048/30@48k"),
            (2048, 40.0, 48_000, 54, "2048/40@48k"),
            (2048, 40.0, 44_100, 55, "2048/40@44.1k"),
            (2048, 80.0, 48_000, 57, "2048/80@48k（低延迟档）"),
            (4096, 30.0, 48_000, 58, "4096/30@48k"),
            (4096, 40.0, 48_000, 59, "4096/40@48k（默认档）"),
            (4096, 40.0, 44_100, 60, "4096/40@44.1k"),
            (4096, 80.0, 48_000, 63, "4096/80@48k"),
            (8192, 30.0, 48_000, 62, "8192/30@48k"),
            (8192, 40.0, 48_000, 63, "8192/40@48k"),
            (8192, 80.0, 48_000, 64, "8192/80@48k（唯一无塌缩）"),
        ];
        for (fft_size, f_min, rate, expected, label) in cases {
            let cfg = AnalyzerConfig {
                fft_size,
                f_min,
                ..AnalyzerConfig::default()
            };
            let edges = build_edges(&cfg, rate);
            assert_eq!(edges.len(), expected, "{label}");
        }
    }

    /// ★ F56：任意档位都不存在相邻重复区间，且 2048 档柱数必须 < 64。
    #[test]
    fn f56_no_adjacent_duplicate_edges() {
        for cfg in [
            AnalyzerConfig::low_latency(),
            AnalyzerConfig::default(),
            AnalyzerConfig::high_resolution(),
        ] {
            let edges = build_edges(&cfg, RATE);
            assert!(!edges.is_empty());
            assert!(
                edges.windows(2).all(|w| w[0] != w[1]),
                "fft_size={} 存在相邻重复区间",
                cfg.fft_size
            );
            assert!(edges.iter().all(|&(lo, hi)| lo < hi));
            if cfg.fft_size == 2048 {
                assert!(edges.len() < 64, "2048 档柱数应 < 64");
            }
        }
    }

    #[test]
    fn default_band_count_is_59_not_64() {
        let cfg = AnalyzerConfig::default();
        let edges = build_edges(&cfg, RATE);
        assert_eq!(edges.len(), 59);
        assert_eq!(cfg.bands, 64);
    }

    /// ★ F22：40 / 60 / 100 Hz 必须落在不同 band（4096 档）。
    #[test]
    fn f22_low_frequency_tones_land_in_distinct_bands() {
        let cfg = AnalyzerConfig::default();
        let edges = build_edges(&cfg, RATE);
        let bin_to_band = |bin: usize| -> Option<usize> {
            edges.iter().position(|&(lo, hi)| (lo..hi).contains(&bin))
        };
        let bins = [40.0, 60.0, 100.0].map(|f| (f * 4096.0 / RATE as f32).round() as usize);
        let bands: Vec<Option<usize>> = bins.iter().map(|&b| bin_to_band(b)).collect();
        assert!(
            bands.iter().all(Option::is_some),
            "有频点落在无 band 覆盖区"
        );
        let unique: std::collections::BTreeSet<usize> = bands.iter().filter_map(|&b| b).collect();
        assert_eq!(unique.len(), 3, "低频三点应落在不同 band：{bands:?}");
    }

    #[test]
    fn sample_rate_change_rebuilds_edges() {
        let cfg = AnalyzerConfig::default();
        // 44.1k / 48k 来自文档 §6.2.2 实算表；96k 为本次按同一算法实算（54 根）。
        for (rate, expected) in [(44_100, 60), (48_000, 59), (96_000, 54)] {
            let an = Analyzer::new(&cfg, rate).expect("构造失败");
            assert_eq!(an.sample_rate(), rate);
            assert_eq!(an.band_count(), expected, "rate={rate}");
        }
    }

    #[test]
    fn tone_energy_lands_in_expected_band() {
        let cfg = AnalyzerConfig::default();
        let mut an = Analyzer::new(&cfg, RATE).expect("构造失败");
        let pcm = sine_wave(1_000.0, RATE, cfg.fft_size);
        let levels = an.process(&pcm, 0).to_vec();
        let peak = levels
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .map(|(i, _)| i)
            .unwrap_or_default();
        let target_bin = (1_000.0 * cfg.fft_size as f32 / RATE as f32).round() as usize;
        let (lo, hi) = an.edges()[peak];
        assert!(
            (lo..=hi).contains(&target_bin),
            "1kHz 峰值 band {peak} 区间 ({lo},{hi}) 不含 bin {target_bin}"
        );
        assert!(levels[peak] > 0.1, "峰值柱高应显著大于 0");
    }

    #[test]
    fn silence_produces_zero_levels() {
        let cfg = AnalyzerConfig::default();
        let mut an = Analyzer::new(&cfg, RATE).expect("构造失败");
        let levels = an.process(&vec![0.0; cfg.fft_size], 0).to_vec();
        assert!(levels.iter().all(|&v| v < 1.0e-3));
    }

    #[test]
    fn short_input_and_empty_input_do_not_panic() {
        let cfg = AnalyzerConfig::default();
        let mut an = Analyzer::new(&cfg, RATE).expect("构造失败");
        assert_eq!(an.process(&[], 0).len(), an.band_count());
        assert_eq!(an.process(&[0.5; 128], 1).len(), an.band_count());
        let long = sine_wave(440.0, RATE, cfg.fft_size * 3);
        assert_eq!(an.process(&long, 2).len(), an.band_count());
    }

    #[test]
    fn peak_hold_decays_after_timeout() {
        let cfg = AnalyzerConfig::default();
        let mut an = Analyzer::new(&cfg, RATE).expect("构造失败");
        let tone = sine_wave(1_000.0, RATE, cfg.fft_size);
        an.process(&tone, 0);
        let before = an.peaks().to_vec();
        an.process(&vec![0.0; cfg.fft_size], 100);
        assert!(
            an.peaks()
                .iter()
                .all(|&p| p <= before.iter().copied().fold(0.0, f32::max) + 1e-6)
        );
        an.process(&vec![0.0; cfg.fft_size], 2_000);
        let after = an.peaks().to_vec();
        assert!(
            after.iter().sum::<f32>() <= before.iter().sum::<f32>() + 1.0e-3,
            "超时后峰值应衰减"
        );
    }

    #[test]
    fn default_peak_fall_keeps_legacy_behaviour() {
        // P1 之前 analyzer.rs 里写死的是 `- 0.02` 匀速衰减。
        // 默认值必须逐帧等价，否则老用户升级后频谱观感会悄悄变。
        let cfg = AnalyzerConfig::default();
        assert_eq!(cfg.peak_fall, 0.02, "默认值必须等于旧硬编码常量");
        assert!(!cfg.peak_gravity, "默认匀速");
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn invalid_peak_fall_is_rejected() {
        for bad in [0.0, -1.0, 0.001, 0.06, f32::NAN, f32::INFINITY] {
            let cfg = AnalyzerConfig {
                peak_fall: bad,
                ..AnalyzerConfig::default()
            };
            assert!(cfg.validate().is_err(), "{bad} 应被拒绝");
            assert!(Analyzer::new(&cfg, RATE).is_err(), "{bad} 不应能构造分析器");
        }
        for good in [0.002, 0.02, 0.05] {
            let cfg = AnalyzerConfig {
                peak_fall: good,
                ..AnalyzerConfig::default()
            };
            assert!(cfg.validate().is_ok(), "{good} 应被接受");
        }
    }

    #[test]
    fn gravity_starts_slower_then_accelerates() {
        let base = AnalyzerConfig::default();
        let gravity = AnalyzerConfig {
            peak_gravity: true,
            ..base.clone()
        };
        let mut linear = Analyzer::new(&base, RATE).expect("构造失败");
        let mut grav = Analyzer::new(&gravity, RATE).expect("构造失败");
        let tone = sine_wave(1_000.0, RATE, base.fft_size);
        linear.process(&tone, 0);
        grav.process(&tone, 0);
        let silence = vec![0.0; base.fft_size];

        // 悬停期之后前 5 帧：重力档应该明显落得更少（「缓慢落下来」）。
        for frame in 1_u64..=5 {
            linear.process(&silence, 800 + frame * 21);
            grav.process(&silence, 800 + frame * 21);
        }
        let linear_sum: f32 = linear.peaks().iter().sum();
        let grav_sum: f32 = grav.peaks().iter().sum();
        assert!(
            grav_sum > linear_sum,
            "重力档前段应落得更慢：{grav_sum} vs {linear_sum}"
        );

        // 再跑 60 帧：重力档速度累积起来，越落越快。
        for frame in 6_u64..=65 {
            linear.process(&silence, 800 + frame * 21);
            grav.process(&silence, 800 + frame * 21);
        }
        assert!(
            grav.peak_vel.iter().any(|&v| v > 0.0),
            "重力档应累积下落速度"
        );
        assert!(
            linear.peak_vel.iter().all(|&v| v == 0.0),
            "匀速档不应累积速度"
        );
    }

    #[test]
    fn peak_slider_never_falls_below_bar() {
        let cfg = AnalyzerConfig {
            peak_gravity: true,
            peak_fall: 0.05,
            ..AnalyzerConfig::default()
        };
        let mut an = Analyzer::new(&cfg, RATE).expect("构造失败");
        let tone = sine_wave(1_000.0, RATE, cfg.fft_size);
        for frame in 0_u64..10 {
            an.process(&tone, frame * 21);
        }
        let silence = vec![0.0; cfg.fft_size];
        for frame in 10_u64..80 {
            an.process(&silence, frame * 21);
        }
        for (i, (&peak, &level)) in an.peaks().iter().zip(an.levels()).enumerate() {
            assert!(
                peak + 1.0e-6 >= level,
                "第 {i} 根柱的滑块穿底：peak={peak} level={level}"
            );
        }
    }

    #[test]
    fn reset_clears_peak_velocity() {
        let cfg = AnalyzerConfig {
            peak_gravity: true,
            ..AnalyzerConfig::default()
        };
        let mut an = Analyzer::new(&cfg, RATE).expect("构造失败");
        let tone = sine_wave(1_000.0, RATE, cfg.fft_size);
        an.process(&tone, 0);
        let silence = vec![0.0; cfg.fft_size];
        for frame in 1_u64..20 {
            an.process(&silence, 800 + frame * 21);
        }
        assert!(an.peak_vel.iter().any(|&v| v > 0.0));
        an.reset();
        assert!(an.peak_vel.iter().all(|&v| v == 0.0), "reset 必须清掉速度");
    }

    #[test]
    fn invalid_config_is_rejected() {
        let bad = AnalyzerConfig {
            fft_size: 3000,
            ..AnalyzerConfig::default()
        };
        assert!(Analyzer::new(&bad, RATE).is_err());
        assert!(Analyzer::new(&AnalyzerConfig::default(), 0).is_err());
        assert!(build_edges(&bad, RATE).is_empty());
    }

    #[test]
    fn reset_clears_smoothing_state() {
        let cfg = AnalyzerConfig::default();
        let mut an = Analyzer::new(&cfg, RATE).expect("构造失败");
        an.process(&sine_wave(440.0, RATE, cfg.fft_size), 0);
        assert!(an.levels().iter().any(|&v| v > 0.0));
        an.reset();
        assert!(an.levels().iter().all(|&v| v == 0.0));
        assert!(an.peaks().iter().all(|&v| v == 0.0));
    }
}
