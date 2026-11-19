//! `PulseAudio` monitor 采集（`capture-pulse` feature）。
//!
//! 本模块把「采集一个固定 hop 的 PCM」封装成可复用类型，供 daemon 主循环调用。
//! 之前这套逻辑只存在于 `examples/capture_diag.rs`（诊断脚本），
//! 导致 daemon 想用就得复制粘贴 —— 现在提取到库里，示例只保留诊断用途。
//!
//! ## 为什么必须是这个驱动方式（M27 的结论）
//!
//! `capture_diag` 做过单变量对照：
//!
//! | 驱动方式 | 是否传 `pa_buffer_attr` | 实时比 |
//! |---|---|---|
//! | 非阻塞 `iterate(false)` | 否 | 0.000（`peek` 永远 Empty） |
//! | 阻塞 `iterate(true)` | 否 | 0.000 |
//! | threaded mainloop | 否 | 0.000 |
//! | 非阻塞 `iterate(false)` | **是**（40ms / `ADJUST_LATENCY`） | **1.000** |
//!
//! 结论：吞吐的决定性因素是 `pa_buffer_attr`，不是驱动方式。
//! 因此本实现固定传 `fragsize = rate × target_latency_ms`，并带 `ADJUST_LATENCY`。
//!
//! ## 安全边界
//!
//! 只 `connect_record` 到 monitor 源，**不播放**、不新建/卸载 sink、
//! 不改默认输出、不改音量。

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use libpulse_binding::context::{Context, FlagSet as ContextFlagSet, State as ContextState};
use libpulse_binding::def::BufferAttr;
use libpulse_binding::mainloop::standard::Mainloop;
use libpulse_binding::sample::{Format, Spec};
use libpulse_binding::stream::{
    FlagSet as StreamFlagSet, PeekResult, State as StreamState, Stream,
};

use crate::capture::CaptureConfig;

/// 未消费样本的缓冲上限（按采样率折算约 500ms）。
///
/// 渲染/合成器短时卡顿后不能无限积压：超过上限就丢最旧数据，
/// 保证延迟不会持续增大、内存也有硬上限。
fn pending_capacity(sample_rate: u32) -> usize {
    (sample_rate as usize / 2).max(1_024)
}

/// 丢弃超出容量的最旧样本，返回本次丢弃的数量。
fn trim_pending(pending: &mut VecDeque<f32>, capacity: usize) -> u64 {
    let excess = pending.len().saturating_sub(capacity);
    if excess == 0 {
        return 0;
    }
    pending.drain(..excess);
    u64::try_from(excess).unwrap_or(0)
}

/// 取**最新**的 `out.len()` 帧；积压时跳过旧数据。
fn take_latest(pending: &mut VecDeque<f32>, out: &mut [f32]) -> usize {
    if pending.len() > out.len() {
        let skip = pending.len() - out.len();
        pending.drain(..skip);
    }
    let take = out.len().min(pending.len());
    for slot in out.iter_mut().take(take) {
        *slot = pending.pop_front().unwrap_or_default();
    }
    take
}

/// 默认采集源：PulseAudio 的"当前默认输出的 monitor"。
pub const DEFAULT_MONITOR: &str = "@DEFAULT_MONITOR@";

/// 采集失败的原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PulseError {
    /// 无法创建 mainloop。
    NoMainloop,
    /// 无法创建 context。
    NoContext,
    /// context 未能进入 Ready。
    ContextNotReady(String),
    /// 无法创建 record stream。
    NoStream,
    /// stream 未能进入 Ready。
    StreamNotReady(String),
    /// `peek` 返回错误。
    Peek(String),
    /// 流已不可用：context 或 stream 处于 `Failed` / `Terminated`。
    ///
    /// 与 [`Self::Peek`] 分开是有意的：`Peek` 可能是一次瞬时失败，
    /// 而这条意味着**连接真的断了**（设备被移除、服务端重启、休眠唤醒），
    /// 调用方应当重建整条采集链，而不是继续在死流上重试。
    StreamDead(String),
}

impl std::fmt::Display for PulseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoMainloop => write!(f, "无法创建 PulseAudio mainloop"),
            Self::NoContext => write!(f, "无法创建 PulseAudio context"),
            Self::ContextNotReady(e) => write!(f, "context 未就绪：{e}"),
            Self::NoStream => write!(f, "无法创建 record stream"),
            Self::StreamNotReady(e) => write!(f, "record stream 未就绪：{e}"),
            Self::Peek(e) => write!(f, "peek 失败：{e}"),
            Self::StreamDead(e) => write!(f, "采集流已断开：{e}"),
        }
    }
}

impl std::error::Error for PulseError {}

/// 采集链的健康状态。
///
/// 单独抽出来是因为「没数据」和「断了」必须分开：音乐暂停时 `read_hop` 也会
/// 返回 0 帧，如果把 0 帧当成断流，用户每次暂停音乐都会触发一次重连风暴。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PulseHealth {
    /// context 与 stream 都 `Ready`，可以继续读。
    Ready,
    /// 仍在建立连接（`open` 过程中）。
    Starting,
    /// 已断开，附可读原因。
    Dead(String),
}

/// 一个已连上 monitor 的采集流。
///
/// 生命周期：`open` → 循环 `read_hop` → `Drop`（自动断开）。
pub struct PulseCapture {
    mainloop: Mainloop,
    context: Context,
    stream: Stream,
    /// 声道数固定为 1（下混由服务端做，本实现只接受单声道 Spec）。
    sample_rate: u32,
    /// 上一轮 `peek` 没被取走的样本（`read_hop` 需要凑满固定 hop）。
    ///
    /// 用 `VecDeque` 而不是 `Vec`：积压时从头部 O(1) 丢弃最旧数据。
    pending: VecDeque<f32>,
    /// 因积压超过上限被丢弃的样本数，供 doctor 观测。
    overflow_frames: u64,
    /// 累计交付的帧数，供 doctor 统计。
    frames_delivered: u64,
    /// `peek` 返回 Empty 的轮数，供 doctor 判断"没声音"还是"采不到"。
    empty_rounds: u64,
    /// 累计丢弃的 hole 数（服务端欠载）。
    holes: u64,
    /// `mainloop.iterate` 失败次数。
    mainloop_errors: u64,
}

impl PulseCapture {
    /// 连接默认 monitor 源。
    ///
    /// `sample_rate` 必须与目标设备一致（本机 graph 锁 48 kHz，见 F74）；
    /// 不匹配时服务端会做重采样，频谱的频率轴会整体偏移。
    ///
    /// # Errors
    ///
    /// mainloop / context / stream 任一环节失败时返回错误。
    pub fn open(
        device: Option<&str>,
        sample_rate: u32,
        cfg: CaptureConfig,
    ) -> Result<Self, PulseError> {
        Self::open_on(None, device, sample_rate, cfg)
    }

    /// 连接**指定**服务端的 monitor 源。
    ///
    /// `server` 为 `None` 时与 [`Self::open`] 完全一致（走 `PULSE_SERVER` / 默认路径）；
    /// 传 `Some("unix:/path/native")` 可连接到非默认实例。
    ///
    /// 这个入口是为断流实证加的：测试需要连一个**私有**的音频服务，
    /// 然后把它杀掉来复现"服务端消失"，而不能去动用户正在用的那个。
    /// 顺带也支持了 `PULSE_SERVER` 指向非默认路径的用户场景。
    ///
    /// # Errors
    ///
    /// 同 [`Self::open`]。
    pub fn open_on(
        server: Option<&str>,
        device: Option<&str>,
        sample_rate: u32,
        cfg: CaptureConfig,
    ) -> Result<Self, PulseError> {
        let mut mainloop = Mainloop::new().ok_or(PulseError::NoMainloop)?;
        let mut context =
            Context::new(&mainloop, "deepin-spectrum-capture").ok_or(PulseError::NoContext)?;
        context
            .connect(server, ContextFlagSet::NOFLAGS, None)
            .map_err(|e| PulseError::ContextNotReady(pa_err(e)))?;
        drive_until(
            &mut mainloop,
            || context.get_state() == ContextState::Ready,
            3_000,
        )
        .map_err(|()| PulseError::ContextNotReady(format!("state={:?}", context.get_state())))?;

        let spec = Spec {
            format: Format::F32le,
            rate: sample_rate,
            channels: 1,
        };
        let mut stream = Stream::new(&mut context, "deepin-spectrum-record", &spec, None)
            .ok_or(PulseError::NoStream)?;

        // ★ M27：必须传 BufferAttr + ADJUST_LATENCY，否则 peek 永远 Empty
        let frag = sample_rate
            .saturating_mul(cfg.target_latency_ms)
            .saturating_mul(4)
            / 1000;
        let attr = BufferAttr {
            maxlength: u32::MAX,
            tlength: u32::MAX,
            prebuf: u32::MAX,
            minreq: u32::MAX,
            fragsize: frag.max(4096),
        };
        stream
            .connect_record(
                Some(device.unwrap_or(DEFAULT_MONITOR)),
                Some(&attr),
                StreamFlagSet::ADJUST_LATENCY,
            )
            .map_err(|e| PulseError::StreamNotReady(pa_err(e)))?;
        drive_until(
            &mut mainloop,
            || stream.get_state() == StreamState::Ready,
            3_000,
        )
        .map_err(|()| PulseError::StreamNotReady(format!("state={:?}", stream.get_state())))?;

        Ok(Self {
            mainloop,
            context,
            stream,
            sample_rate,
            pending: VecDeque::with_capacity(16_384),
            overflow_frames: 0,
            frames_delivered: 0,
            empty_rounds: 0,
            holes: 0,
            mainloop_errors: 0,
        })
    }

    /// 采集速率（Hz）。
    #[must_use]
    pub const fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// 累计交付帧数。
    #[must_use]
    pub const fn frames_delivered(&self) -> u64 {
        self.frames_delivered
    }

    /// `peek` 返回 Empty 的轮数。
    #[must_use]
    pub const fn empty_rounds(&self) -> u64 {
        self.empty_rounds
    }

    /// 累计 hole 数（服务端欠载丢帧）。
    #[must_use]
    pub const fn holes(&self) -> u64 {
        self.holes
    }

    /// `mainloop.iterate` 返回错误的次数。
    ///
    /// 单看这个数字没有意义（偶发失败可恢复），但它与 [`Self::health`]
    /// 一起就能区分「服务端重启中」和「一直没数据」。
    #[must_use]
    pub const fn mainloop_errors(&self) -> u64 {
        self.mainloop_errors
    }

    /// 因积压超过缓冲上限而丢弃的样本数。
    #[must_use]
    pub const fn overflow_frames(&self) -> u64 {
        self.overflow_frames
    }

    /// 当前健康状态。
    ///
    /// ★ 这是断流恢复的判据：`read_hop` 返回 0 帧只代表「这一轮没数据」
    /// （音乐暂停就是这样），不能据此判断连接已死。
    #[must_use]
    pub fn health(&self) -> PulseHealth {
        let ctx = self.context.get_state();
        let strm = self.stream.get_state();
        let ctx_dead = matches!(ctx, ContextState::Failed | ContextState::Terminated);
        let strm_dead = matches!(strm, StreamState::Failed | StreamState::Terminated);
        if ctx_dead || strm_dead {
            // Debug 而非 Display：状态枚举没有人类可读文案，Debug 名已经够清楚
            return PulseHealth::Dead(format!("context={ctx:?} stream={strm:?}"));
        }
        if ctx == ContextState::Ready && strm == StreamState::Ready {
            return PulseHealth::Ready;
        }
        PulseHealth::Starting
    }

    /// 是否仍然可读（`Ready`）。
    #[must_use]
    pub fn is_healthy(&self) -> bool {
        self.health() == PulseHealth::Ready
    }

    /// 读满一个 hop。
    ///
    /// 返回实际写进 `out` 的帧数：
    /// - `== out.len()`：正常
    /// - `< out.len()`：等到 `wait` 超时仍未凑满（静音或设备无数据）
    ///
    /// 超时时**不**补零：补零会让下一帧 FFT 的窗函数边缘被伪造成静音，
    /// 在频谱上看成"整条突然掉一下"。调用方应据返回值决定这一帧是跳过还是当作静音。
    ///
    /// # Errors
    ///
    /// `peek` 出错时返回错误（通常是流已断开）。
    pub fn read_hop(&mut self, out: &mut [f32], wait: Duration) -> Result<usize, PulseError> {
        if out.is_empty() {
            return Ok(0);
        }
        let deadline = Instant::now() + wait;
        loop {
            // ★ 每轮都先查健康状态：服务端重启/设备移除后，`peek` 可能一直
            // 返回 Empty（不是 Err），只靠返回值永远发现不了已经断了 ——
            // 那种情况下 daemon 会「永远静音」而不是报错。
            if let PulseHealth::Dead(why) = self.health() {
                return Err(PulseError::StreamDead(why));
            }
            self.pump()?;
            if self.pending.len() >= out.len() {
                // ★ 取最新 hop：卡顿后积压的旧数据直接跳过，否则延迟会越积越大。
                let take = take_latest(&mut self.pending, out);
                self.frames_delivered += u64::try_from(take).unwrap_or(0);
                return Ok(take);
            }
            if Instant::now() >= deadline {
                let take = take_latest(&mut self.pending, out);
                self.frames_delivered += u64::try_from(take).unwrap_or(0);
                return Ok(take);
            }
            // 2ms 与 capture_diag 的 D 阶段一致：足够跟上 40ms 缓冲，又不空转
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// 把流上所有可用 chunk 搬进 `pending`。
    fn pump(&mut self) -> Result<(), PulseError> {
        // 非阻塞驱动：先推进 mainloop 再到流上取数据
        if self.mainloop.iterate(false).is_error() {
            // 不在这里直接报错：瞬时失败可恢复，而 mainloop 真坏掉时
            // context 会变成 Failed，由 `read_hop` 的健康检查统一判定。
            self.mainloop_errors += 1;
            self.empty_rounds += 1;
            return Ok(());
        }
        // 单次最多取 64 块，避免长时间占用主循环
        for _ in 0..64 {
            match self.stream.peek() {
                Ok(PeekResult::Data(bytes)) => {
                    // F32le + 单声道 → 每帧 4 字节
                    self.pending.extend(
                        bytes
                            .chunks_exact(4)
                            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])),
                    );
                    let _ = self.stream.discard();
                }
                Ok(PeekResult::Hole(_)) => {
                    self.holes += 1;
                    let _ = self.stream.discard();
                }
                Ok(PeekResult::Empty) => {
                    self.empty_rounds += 1;
                    break;
                }
                Err(e) => {
                    // ★ 流已死时 `peek` 返回的是 `Bad state`（PULSE_ERR_BADSTATE），
                    // 对调用方毫无信息量，还会被误判成"偶发失败"。实测：服务端被 kill 后
                    // 先撞到的就是这个错误，而 `health()` 已经是 `Dead`。
                    // 因此这里先查健康状态，把"死流上的任何错误"统一升级为 `StreamDead`，
                    // 上层只需处理一种情况。
                    if let PulseHealth::Dead(why) = self.health() {
                        return Err(PulseError::StreamDead(format!(
                            "{why}（peek: {}）",
                            pa_err(e)
                        )));
                    }
                    return Err(PulseError::Peek(pa_err(e)));
                }
            }
        }
        // 有硬上限：积压超过约 500ms 就丢最旧数据，防止延迟/内存无限增长。
        let capacity = pending_capacity(self.sample_rate);
        self.overflow_frames += trim_pending(&mut self.pending, capacity);
        Ok(())
    }

    /// 主动断开；`Drop` 也会调用（幂等）。
    pub fn close(&mut self) {
        let _ = self.stream.disconnect();
        self.context.disconnect();
    }
}

impl Drop for PulseCapture {
    fn drop(&mut self) {
        self.close();
    }
}

impl std::fmt::Debug for PulseCapture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PulseCapture")
            .field("sample_rate", &self.sample_rate)
            .field("frames_delivered", &self.frames_delivered)
            .field("empty_rounds", &self.empty_rounds)
            .field("holes", &self.holes)
            .field("mainloop_errors", &self.mainloop_errors)
            .field("overflow_frames", &self.overflow_frames)
            .finish_non_exhaustive()
    }
}

/// `PAErr` 自带 `to_string() -> Option<String>`（遮蔽了 `ToString`），
/// 直接调用会拿到 `Option`，因此统一走这个包装。
fn pa_err(err: libpulse_binding::error::PAErr) -> String {
    err.to_string()
        .unwrap_or_else(|| format!("PulseAudio 错误码 {:?}", err.0))
}

/// 推进 mainloop 直到条件成立或超时。
fn drive_until(
    mainloop: &mut Mainloop,
    cond: impl Fn() -> bool,
    timeout_ms: u64,
) -> Result<(), ()> {
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(timeout_ms) {
        if mainloop.iterate(false).is_error() {
            return Err(());
        }
        if cond() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    Err(())
}

#[cfg(test)]
mod tests {
    use super::{pending_capacity, take_latest, trim_pending};
    use std::collections::VecDeque;

    #[test]
    fn backlog_keeps_latest_hop() {
        let mut pending: VecDeque<f32> = (0..10).map(|i| i as f32).collect();
        let mut out = [0.0_f32; 4];
        assert_eq!(take_latest(&mut pending, &mut out), 4);
        assert_eq!(out, [6.0, 7.0, 8.0, 9.0], "应跳过旧数据，取最新 4 帧");
        assert!(pending.is_empty());
    }

    #[test]
    fn overflow_drops_oldest_and_is_counted() {
        let mut pending: VecDeque<f32> = (0..10).map(|i| i as f32).collect();
        assert_eq!(trim_pending(&mut pending, 4), 6);
        assert_eq!(
            pending.iter().copied().collect::<Vec<_>>(),
            vec![6.0, 7.0, 8.0, 9.0]
        );
        assert_eq!(trim_pending(&mut pending, 4), 0, "未超限时不应丢弃");
    }

    #[test]
    fn capacity_is_half_a_second() {
        assert_eq!(pending_capacity(48_000), 24_000);
        assert!(pending_capacity(0) >= 1_024, "极低采样率也要有最小缓冲");
    }
}
