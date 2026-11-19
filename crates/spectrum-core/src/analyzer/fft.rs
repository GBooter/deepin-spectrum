#![allow(clippy::many_single_char_names)] // 蝶形运算沿用教科书的 a/n/j/i/w/u/v 记号
//! 零依赖 radix-2 复数 FFT 与 Hann 窗（§6.2.1 的处理链前两步）。
//!
//! 这里做 N 点复数 FFT（虚部置 0）后取 `0..=N/2` 的功率谱，与
//! `realfft` 的 R2C 输出在 bin 索引与相对幅度上等价，代价是多约一倍蝶形。
//! `4096` 点在 release 下远低于 §8 的 CPU 预算。
//!
//! **C 方案双后端**：[`FftBackend`] 在默认构建用本文件的零依赖实现，
//! 启用 `fft-realfft` feature 时改用 `realfft 3.5.1`。[`power_spectrum`]
//! 始终作为参考实现保留，供 `scripts/fftcheck/` 做数值对比。

use std::f32::consts::PI;

/// 最小可测功率，避免 `log10(0)` 产生 `-inf`。
const POWER_EPSILON: f32 = 1.0e-12;

/// 复数样本，用于 FFT 暂存缓冲与功率谱计算。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Complex {
    /// 实部。
    pub re: f32,
    /// 虚部。
    pub im: f32,
}

impl Complex {
    /// 以实部、虚部构造复数。
    #[must_use]
    pub const fn new(re: f32, im: f32) -> Self {
        Self { re, im }
    }
}

impl std::ops::Add for Complex {
    type Output = Self;

    fn add(self, rhs: Self) -> Self {
        Self::new(self.re + rhs.re, self.im + rhs.im)
    }
}

impl std::ops::Sub for Complex {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self {
        Self::new(self.re - rhs.re, self.im - rhs.im)
    }
}

impl std::ops::Mul for Complex {
    type Output = Self;

    fn mul(self, rhs: Self) -> Self {
        Self::new(
            self.re * rhs.re - self.im * rhs.im,
            self.re * rhs.im + self.im * rhs.re,
        )
    }
}

/// 原地迭代 Cooley-Tukey FFT；`a.len()` 必须是 2 的幂。
pub(super) fn fft_in_place(a: &mut [Complex]) {
    debug_assert!(a.len() >= 2 && a.len().is_power_of_two());
    let n = a.len();

    // 位反转重排
    let mut j = 0usize;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j ^= bit;
        if i < j {
            a.swap(i, j);
        }
    }

    // 逐级蝶形
    let mut len = 2usize;
    while len <= n {
        let angle = -2.0 * PI / len as f32;
        let wlen = Complex::new(angle.cos(), angle.sin());
        let half = len / 2;
        let mut start = 0usize;
        while start < n {
            let mut w = Complex::new(1.0, 0.0);
            for k in 0..half {
                let u = a[start + k];
                let v = a[start + k + half] * w;
                a[start + k] = u + v;
                a[start + k + half] = u - v;
                w = w * wlen;
            }
            start += len;
        }
        len *= 2;
    }
}

/// 周期 Hann 窗；长度为 1 时退化为全 1。
#[must_use]
pub fn hann(size: usize) -> Vec<f32> {
    if size <= 1 {
        return vec![1.0; size];
    }
    (0..size)
        .map(|i| 0.5 - 0.5 * (2.0 * PI * i as f32 / size as f32).cos())
        .collect()
}

/// 加窗 → FFT → 单边功率谱，长度 `n/2 + 1`。
///
/// 用 `(sum(window))²` 归一化：幅度为 `A` 的纯正弦在峰值 bin 上的功率约为
/// `A²/4`，与窗长无关，便于 §9.1 的 golden data 断言稳定。
#[must_use]
pub fn power_spectrum(pcm: &[f32], window: &[f32], scratch: &mut Vec<Complex>) -> Vec<f32> {
    let n = window.len();
    // 空窗 / 非 2 的幂没有定义；返回空谱而不是在 `scratch[..=n/2]` 越界 panic。
    if n < 2 || !n.is_power_of_two() {
        scratch.clear();
        return Vec::new();
    }
    scratch.clear();
    scratch.resize(n, Complex::default());
    for (i, slot) in scratch.iter_mut().enumerate() {
        *slot = Complex::new(pcm.get(i).copied().unwrap_or_default() * window[i], 0.0);
    }
    fft_in_place(scratch);

    let w_sum: f32 = window.iter().sum();
    let denom = (w_sum * w_sum).max(POWER_EPSILON);
    scratch[..=n / 2]
        .iter()
        .map(|c| (c.re * c.re + c.im * c.im) / denom)
        .collect()
}

/// FFT 后端封装（C 方案双后端）。
///
/// - 默认（无 feature）：零依赖内置 radix-2 复数 FFT，见 [`power_spectrum`]。
/// - `fft-realfft`：`realfft 3.5.1` 的 R2C 变换，实测快 10.98× 且数值等价。
///
/// 上层 `Analyzer` 只依赖本类型，不感知后端差异；所有状态在实例内（F55）。
#[derive(Clone)]
pub struct FftBackend {
    inner: BackendInner,
    fft_size: usize,
}

// trait object `dyn RealToComplex` 不实现 Debug，故手写而不是 derive。
// `inner` 在 realfft 路径含 `Arc<dyn RealToComplex>` 且只是计算缓冲，
// 对调试无意义；只暴露后端名与点数即可。
#[allow(clippy::missing_fields_in_debug)]
impl std::fmt::Debug for FftBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FftBackend")
            .field("name", &self.name())
            .field("fft_size", &self.fft_size)
            .finish()
    }
}

impl FftBackend {
    /// 以 FFT 点数与窗函数构造；窗只用于预计算功率归一化分母。
    ///
    /// # Errors
    ///
    /// 点数非法（<2 / 非 2 的幂）或窗函数长度与点数不一致时返回错误，
    /// 而不是让后续 `compute` 越界。
    pub fn try_new(fft_size: usize, window: &[f32]) -> Result<Self, crate::CoreError> {
        if fft_size < 2 || !fft_size.is_power_of_two() {
            return Err(crate::CoreError::InvalidFftSize(fft_size));
        }
        if window.len() != fft_size {
            return Err(crate::CoreError::FftWindowMismatch {
                fft_size,
                window: window.len(),
            });
        }
        Ok(Self {
            inner: BackendInner::new(fft_size, window),
            fft_size,
        })
    }

    /// 计算单边功率谱，返回内部缓存切片（长度 `fft_size/2 + 1`）。
    pub fn compute(&mut self, pcm: &[f32], window: &[f32]) -> &[f32] {
        self.inner.compute(pcm, window)
    }

    /// 当前后端名，供 `doctor` JSON 的 `fft_backend` 字段使用。
    #[must_use]
    pub const fn name(&self) -> &'static str {
        if cfg!(feature = "fft-realfft") {
            "realfft-3.5.1-r2c"
        } else {
            "builtin-radix2-complex"
        }
    }
}

// ── 默认后端：零外部依赖 ────────────────────────────────────────
#[cfg(not(feature = "fft-realfft"))]
#[derive(Clone)]
struct BackendInner {
    scratch: Vec<Complex>,
    cache: Vec<f32>,
}

#[cfg(not(feature = "fft-realfft"))]
impl BackendInner {
    fn new(fft_size: usize, _window: &[f32]) -> Self {
        Self {
            scratch: Vec::with_capacity(fft_size),
            cache: Vec::with_capacity(fft_size / 2 + 1),
        }
    }

    fn compute(&mut self, pcm: &[f32], window: &[f32]) -> &[f32] {
        self.cache = power_spectrum(pcm, window, &mut self.scratch);
        &self.cache
    }
}

// ── 可选后端：realfft 3.5.1 R2C ─────────────────────────────────
#[cfg(feature = "fft-realfft")]
#[derive(Clone)]
struct BackendInner {
    plan: std::sync::Arc<dyn realfft::RealToComplex<f32>>,
    input: Vec<f32>,
    output: Vec<rustfft::num_complex::Complex<f32>>,
    denom: f32,
    cache: Vec<f32>,
}

#[cfg(feature = "fft-realfft")]
impl BackendInner {
    fn new(fft_size: usize, window: &[f32]) -> Self {
        let mut planner = realfft::RealFftPlanner::<f32>::new();
        let plan = planner.plan_fft_forward(fft_size);
        let w_sum: f32 = window.iter().sum();
        Self {
            input: plan.make_input_vec(),
            output: plan.make_output_vec(),
            plan,
            denom: (w_sum * w_sum).max(POWER_EPSILON),
            cache: vec![0.0; fft_size / 2 + 1],
        }
    }

    fn compute(&mut self, pcm: &[f32], window: &[f32]) -> &[f32] {
        for (i, slot) in self.input.iter_mut().enumerate() {
            *slot = pcm.get(i).copied().unwrap_or_default() * window[i];
        }
        // `FftError` 未实现 Debug，不能用 expect/unwrap；缓冲尺寸由 make_*_vec 保证，
        // 万一失败则保留上一帧形状但置零，绝不 panic。
        if self
            .plan
            .process(&mut self.input, &mut self.output)
            .is_err()
        {
            self.cache.fill(0.0);
            return &self.cache;
        }
        for (dst, c) in self.cache.iter_mut().zip(self.output.iter()) {
            *dst = (c.re * c.re + c.im * c.im) / self.denom;
        }
        &self.cache
    }
}

#[cfg(test)]
mod tests {
    use super::{Complex, FftBackend, fft_in_place, hann, power_spectrum};
    use std::f32::consts::PI;

    /// 朴素 DFT，仅用于测试里校验 FFT 的正确性（O(N²)，只在 64 点上跑）。
    fn naive_dft(signal: &[f32], k: usize) -> Complex {
        let n = signal.len();
        let mut acc = Complex::new(0.0, 0.0);
        for (i, &x) in signal.iter().enumerate() {
            let angle = -2.0 * PI * (i * k) as f32 / n as f32;
            let w = Complex::new(angle.cos(), angle.sin());
            acc = acc + Complex::new(x, 0.0) * w;
        }
        acc
    }

    #[test]
    fn fft_matches_naive_dft() {
        let n = 64usize;
        let signal: Vec<f32> = (0..n)
            .map(|i| (2.0 * PI * 5.0 * i as f32 / n as f32).sin())
            .collect();
        let mut buf: Vec<Complex> = signal.iter().map(|&x| Complex::new(x, 0.0)).collect();
        fft_in_place(&mut buf);
        for (k, &got) in buf.iter().enumerate() {
            let expected = naive_dft(&signal, k);
            assert!(
                (got.re - expected.re).abs() < 1.0e-3,
                "bin {k}: re {} vs {}",
                got.re,
                expected.re
            );
            assert!(
                (got.im - expected.im).abs() < 1.0e-3,
                "bin {k}: im {} vs {}",
                got.im,
                expected.im
            );
        }
    }

    #[test]
    fn hann_is_symmetric_and_bounded() {
        let w = hann(4096);
        assert_eq!(w.len(), 4096);
        assert!(w.iter().all(|&x| (0.0..=1.0).contains(&x)));
        assert!(w[0] < 0.01);
        assert!(w[2048] > 0.99);
    }

    #[test]
    fn tone_peaks_at_expected_bin() {
        const N: usize = 4096;
        const RATE: f32 = 48_000.0;
        let freq = 1_000.0f32;
        let pcm: Vec<f32> = (0..N)
            .map(|i| (2.0 * PI * freq * i as f32 / RATE).sin())
            .collect();
        let window = hann(N);
        let mut scratch = Vec::new();
        let power = power_spectrum(&pcm, &window, &mut scratch);
        assert_eq!(power.len(), N / 2 + 1);
        let peak = power
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .map(|(i, _)| i)
            .unwrap_or_default();
        let expected = (freq * N as f32 / RATE).round() as usize;
        assert!(
            peak.abs_diff(expected) <= 1,
            "峰值 bin {peak} 与期望 {expected} 不符"
        );
    }

    #[test]
    fn silence_produces_zero_power() {
        let window = hann(256);
        let mut scratch = Vec::new();
        let power = power_spectrum(&[0.0; 256], &window, &mut scratch);
        assert!(power.iter().all(|&p| p < 1.0e-9));
    }

    /// C 方案：无论启用哪个 feature，`FftBackend` 都必须与参考实现等价。
    /// 这条断言在默认 / `--all-features` 两条路径下都会跑，是双后端的共同回归门。
    #[test]
    fn backend_matches_reference_spectrum() {
        const N: usize = 4096;
        const RATE: f32 = 48_000.0;
        let pcm: Vec<f32> = (0..N)
            .map(|i| (2.0 * PI * 1_000.0 * i as f32 / RATE).sin())
            .collect();
        let window = hann(N);
        let mut scratch = Vec::new();
        let reference = power_spectrum(&pcm, &window, &mut scratch);

        let mut backend = FftBackend::try_new(N, &window).expect("合法参数应成功");
        let got = backend.compute(&pcm, &window);
        assert_eq!(got.len(), reference.len());
        let peak = reference
            .iter()
            .copied()
            .fold(0.0_f32, f32::max)
            .max(1.0e-12);
        let max_abs = got
            .iter()
            .zip(reference.iter())
            .map(|(a, b)| (a - b).abs())
            .fold(0.0_f32, f32::max);
        assert!(
            max_abs / peak < 1.0e-3,
            "后端 {} 与参考实现不一致：归一化误差 {}",
            backend.name(),
            max_abs / peak
        );
        assert!(got.iter().all(|v| v.is_finite()));
    }

    #[test]
    fn empty_or_invalid_window_does_not_panic() {
        let mut scratch = Vec::new();
        assert!(power_spectrum(&[], &[], &mut scratch).is_empty());
        assert!(power_spectrum(&[0.0; 8], &[1.0; 3], &mut scratch).is_empty());

        assert!(FftBackend::try_new(0, &[]).is_err());
        assert!(FftBackend::try_new(64, &[1.0; 32]).is_err());
    }

    #[test]
    fn backend_name_reflects_active_feature() {
        let window = hann(64);
        let backend = FftBackend::try_new(64, &window).expect("合法参数应成功");
        if cfg!(feature = "fft-realfft") {
            assert_eq!(backend.name(), "realfft-3.5.1-r2c");
        } else {
            assert_eq!(backend.name(), "builtin-radix2-complex");
        }
    }
}
