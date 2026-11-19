//! 音频采集侧（§6.1）。
//!
//! 本文件保留**纯逻辑模型**，默认构建零依赖；真实 `PulseAudio` 采集在
//! [`crate::pulse`]（`capture-pulse` feature，`libpulse-binding 2.30.1`）。
//! 两条路径共用同一套 F24 / F54 语义：
//!
//! - **F24**：只有 `rms > silence_lin` 才刷新 `last_sound_ms`，边界值等于门限时**不刷新**。
//! - **F54**：生产端 `push_slice` 满了丢**新**数据；「丢弃最旧」由消费端 `skip` 完成。
//!
//! **预留实现**：生产采集走 [`crate::pulse`]（有界最新数据），[`FrameRing`] 目前
//! 只用于测试与 `core_selfcheck` 的语义验证，不在 daemon 主路径上。

use std::collections::VecDeque;

/// 环形缓冲容量：16384 帧（§6.1.2）。
pub const RING_CAPACITY_FRAMES: usize = 16_384;

/// F32LE 单声道下的字节数（★ F53：16384 × 4 = 65536，不是 32768）。
pub const RING_CAPACITY_BYTES: usize = RING_CAPACITY_FRAMES * size_of::<f32>();

/// 默认静音门限（dBFS，§6.1.5）。
pub const DEFAULT_SILENCE_DBFS: f32 = -62.0;

/// 默认静音超时（毫秒，§6.1.5）。
pub const DEFAULT_SILENCE_TIMEOUT_MS: u64 = 800;

/// 采集参数（不含设备句柄，纯数据）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureConfig {
    /// 通道数；必须下混为 1。
    pub channels: u8,
    /// 缓冲时长（毫秒）。
    pub buffer_ms: u32,
    /// 目标延迟（毫秒），用于 `pa_buffer_attr` 调优。
    pub target_latency_ms: u32,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            channels: 1,
            buffer_ms: 60,
            target_latency_ms: 40,
        }
    }
}

/// dBFS → 线性 RMS 门限（§6.1.5）。
///
/// `-62.0` dB 对应的线性值约 `7.943e-4`；**不能**拿 `-62.0` 直接和 RMS 比较。
#[must_use]
pub fn silence_linear(dbfs: f32) -> f32 {
    10.0f32.powf(dbfs / 20.0)
}

/// 静音门限状态（F24）。
///
/// 由音频回调把每次读到的 RMS 喂给 [`SilenceGate::feed`]，主循环用
/// [`SilenceGate::active`] 决定是否继续渲染（静音超时后 0 fps）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SilenceGate {
    silence_lin_bits: u32,
    timeout_ms: u64,
    last_sound_ms: u64,
    /// 显式静音标志：断流/清屏时置位，喂入超过门限的声音后清除。
    ///
    /// 不能只靠把 `last_sound_ms` 往前推：启动初期 `now_ms < timeout_ms`
    /// 时饱和到 0，`active()` 仍会判为"刚出过声"。
    muted: bool,
}

impl SilenceGate {
    /// 以 dBFS 与超时（毫秒）构造；`last_sound_ms` 初始为 0（视为从未出声）。
    #[must_use]
    pub fn new(dbfs: f32, timeout_ms: u64) -> Self {
        Self {
            silence_lin_bits: silence_linear(dbfs).to_bits(),
            timeout_ms,
            last_sound_ms: 0,
            muted: false,
        }
    }

    /// 喂入一次 RMS 采样。★ F24：严格 `>` 才刷新时间戳，等于门限不刷新。
    pub fn feed(&mut self, rms: f32, now_ms: u64) {
        if rms > f32::from_bits(self.silence_lin_bits) {
            self.last_sound_ms = now_ms;
            self.muted = false;
        }
    }

    /// 当前是否仍处于「有声音」窗口内。
    #[must_use]
    pub fn active(&self, now_ms: u64) -> bool {
        !self.muted && now_ms.saturating_sub(self.last_sound_ms) < self.timeout_ms
    }

    /// 最近一次超过门限的时间戳，供 `doctor` 输出。
    #[must_use]
    pub const fn last_sound_ms(&self) -> u64 {
        self.last_sound_ms
    }

    /// 立即置为「已静音」。
    ///
    /// 用于采集断流：此时没有 PCM 可喂，但画面必须立刻清空 ——
    /// 否则最后那一帧柱子会**冻在屏幕上**，看起来像"程序死了但还留着残影"。
    /// 同时设置 `muted` 标志并推进时间戳；`active()` 优先判断该标志，
    /// 因此启动初期 `now_ms < timeout_ms` 也能立即生效。
    ///
    /// 与 [`Self::new`] 的区别：`new` 的 `last_sound_ms = 0` 在启动初期
    /// （`now_ms < timeout_ms`）会被判为"刚出过声"（这是给"启动即播放"留的宽限），
    /// 而断流场景需要**立即**静音，不能用那个初值。
    pub fn mute(&mut self, now_ms: u64) {
        self.last_sound_ms = now_ms.saturating_sub(self.timeout_ms);
        self.muted = true;
    }

    /// 当前生效的线性门限，供断言与日志。
    #[must_use]
    pub fn threshold(&self) -> f32 {
        f32::from_bits(self.silence_lin_bits)
    }
}

impl Default for SilenceGate {
    fn default() -> Self {
        Self::new(DEFAULT_SILENCE_DBFS, DEFAULT_SILENCE_TIMEOUT_MS)
    }
}

/// ringbuf 0.6 SPSC 语义的**单线程可测模型**（F54）。
///
/// 真实实现用 `HeapProd<f32>` / `HeapCons<f32>` 跨线程；本结构用
/// `VecDeque` 表达完全相同的索引语义，用于回归断言，不含任何 unsafe。
#[derive(Debug, Clone)]
pub struct FrameRing {
    buf: VecDeque<f32>,
    capacity: usize,
    overflow: u64,
}

impl FrameRing {
    /// 以帧数构造（默认 [`RING_CAPACITY_FRAMES`]）。
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            buf: VecDeque::with_capacity(capacity),
            capacity,
            overflow: 0,
        }
    }

    /// 生产端：只写空闲区，返回实际写入数；未满全写时 `overflow` +1。
    pub fn push_slice(&mut self, samples: &[f32]) -> usize {
        let free = self.capacity.saturating_sub(self.buf.len());
        let written = free.min(samples.len());
        self.buf.extend(samples[..written].iter().copied());
        if written < samples.len() {
            self.overflow += 1;
        }
        written
    }

    /// 消费端：先 `skip` 掉过期帧，再取最近 `n` 帧写入 `out`。
    ///
    /// 返回实际取得的帧数；不足时不补零（由调用方决定沿用上一帧或补零）。
    pub fn consume_latest(&mut self, n: usize, out: &mut [f32]) -> usize {
        let occupied = self.buf.len();
        if occupied > n {
            for _ in 0..(occupied - n) {
                self.buf.pop_front();
            }
        }
        let take = n.min(self.buf.len()).min(out.len());
        for slot in out.iter_mut().take(take) {
            *slot = self.buf.pop_front().unwrap_or_default();
        }
        take
    }

    /// 当前占用帧数。
    #[must_use]
    pub fn occupied_len(&self) -> usize {
        self.buf.len()
    }

    /// 生产端溢出次数，供 `doctor` 输出 `overflow_count`。
    #[must_use]
    pub const fn overflow_count(&self) -> u64 {
        self.overflow
    }

    /// 清空缓冲（采样率变更时调用，§6.1.4）。
    pub fn clear(&mut self) {
        self.buf.clear();
    }
}

impl Default for FrameRing {
    fn default() -> Self {
        Self::new(RING_CAPACITY_FRAMES)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CaptureConfig, FrameRing, RING_CAPACITY_BYTES, RING_CAPACITY_FRAMES, SilenceGate,
        silence_linear,
    };

    #[test]
    fn f53_ring_capacity_bytes() {
        assert_eq!(RING_CAPACITY_FRAMES, 16_384);
        assert_eq!(RING_CAPACITY_BYTES, 65_536);
    }

    #[test]
    fn capture_defaults_are_mono() {
        let c = CaptureConfig::default();
        assert_eq!(c.channels, 1);
        assert_eq!(c.buffer_ms, 60);
        assert_eq!(c.target_latency_ms, 40);
    }

    #[test]
    fn silence_linear_conversion() {
        let lin = silence_linear(-62.0);
        assert!((lin - 7.943_282e-4).abs() < 1.0e-8, "实际 {lin}");
        assert!((silence_linear(0.0) - 1.0).abs() < 1.0e-9);
    }

    /// ★ F24 表驱动：低于 / 等于 / 高于门限三种情况。
    #[test]
    fn f24_silence_gate_table_driven() {
        let lin = silence_linear(-62.0);
        let mut gate = SilenceGate::new(-62.0, 800);
        assert!((gate.threshold() - lin).abs() < 1.0e-9);

        let cases: [(f32, u64, u64, &str); 3] = [
            (lin * 0.5, 100, 0, "低于门限：不刷新"),
            (lin, 200, 0, "等于门限：不刷新（严格 >）"),
            (lin * 2.0, 300, 300, "高于门限：刷新"),
        ];
        for (rms, now, expected, label) in cases {
            gate.feed(rms, now);
            assert_eq!(gate.last_sound_ms(), expected, "{label}");
        }

        assert!(gate.active(1_000), "距最后声音 700ms 仍活跃");
        assert!(!gate.active(1_100), "距最后声音 800ms 应判静音");
        assert!(!gate.active(50_000), "长时间静音后帧计数应停止增长");
    }

    #[test]
    fn mute_is_immediate_even_before_timeout_window() {
        let mut gate = SilenceGate::new(-62.0, 800);
        assert!(gate.active(100), "启动初期应保留「启动即播放」宽限");

        gate.mute(100);
        assert!(
            !gate.active(100),
            "mute 必须立即生效，不能依赖 saturating_sub"
        );
        assert!(!gate.active(101));

        gate.feed(silence_linear(-62.0) * 2.0, 102);
        assert!(gate.active(102), "超过门限的声音应解除显式静音");
    }

    /// ★ F54：生产端丢新、消费端 skip 丢旧。
    #[test]
    fn f54_ring_producer_and_consumer_semantics() {
        let mut ring = FrameRing::new(8);
        assert_eq!(ring.push_slice(&[1.0, 2.0, 3.0, 4.0, 5.0]), 5);
        assert_eq!(ring.overflow_count(), 0);

        // 只剩 3 个空位 → 写入 3，丢掉后 2 个，overflow +1
        assert_eq!(ring.push_slice(&[6.0, 7.0, 8.0, 9.0, 10.0]), 3);
        assert_eq!(ring.overflow_count(), 1);
        assert_eq!(ring.occupied_len(), 8);

        // 消费端只要最近 4 帧 → 应为 5,6,7,8（旧数据 1..4 被 skip）
        let mut out = [0.0f32; 4];
        assert_eq!(ring.consume_latest(4, &mut out), 4);
        assert_eq!(out, [5.0, 6.0, 7.0, 8.0]);
        assert_eq!(ring.occupied_len(), 0);
    }

    #[test]
    fn consume_latest_short_input_does_not_pad() {
        let mut ring = FrameRing::new(16);
        ring.push_slice(&[0.25, 0.5]);
        let mut out = [9.0f32; 4];
        assert_eq!(ring.consume_latest(4, &mut out), 2);
        assert_eq!(out[0], 0.25);
        assert_eq!(out[1], 0.5);
        assert_eq!(out[2], 9.0, "不足部分由调用方决定，模型不擅自补零");
    }

    #[test]
    fn sample_rate_change_clears_ring() {
        let mut ring = FrameRing::default();
        ring.push_slice(&[0.1; 100]);
        assert_eq!(ring.occupied_len(), 100);
        ring.clear();
        assert_eq!(ring.occupied_len(), 0);
    }
}
