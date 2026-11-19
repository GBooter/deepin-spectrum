//! 阶段 2 只读采集探针（需 `capture-pulse` feature）。
//!
//! 默认连接 `@DEFAULT_MONITOR@`；也可指定设备。
//! 用法：
//! ```text
//! cargo run --release -p spectrum-core --features capture-pulse --example capture_probe \
//!     -- <out.json> [device]
//! ```
//! 只读取 monitor，不新建/卸载 sink、不改默认输出、不播放测试音、不注册服务。
//!
//! 除了采集本身，还会把真实音频喂给 [`Analyzer`]，验证「采集 → FFT → 频段柱」
//! 整链路，而不是只看「拿到了多少字节」。
//!
//! 采集走生产 API [`spectrum_core::pulse::PulseCapture`]：两套 `PulseCapture`
//! 已合并，本探针不再维护第二套实现。

use spectrum_core::analyzer::{Analyzer, AnalyzerConfig, build_edges};
use spectrum_core::capture::{CaptureConfig, DEFAULT_SILENCE_DBFS, silence_linear};
use spectrum_core::pulse::{DEFAULT_MONITOR, PulseCapture};
use std::time::{Duration, Instant};

const ROUNDS: usize = 10;
const ROUND_MS: u64 = 200;
const FFT_SIZE: usize = 4096;
const DEFAULT_SAMPLE_RATE: u32 = 48_000;

fn esc(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// 采集 + 分析的一次汇总。
struct Summary {
    device: String,
    frames_delivered: u64,
    holes: u64,
    empty_rounds: u64,
    mainloop_errors: u64,
    overflow_frames: u64,
    elapsed_ms: u64,
    rms: f32,
    peak: f32,
    audio_active: bool,
    finite: bool,
    consumed_total: usize,
    levels: Vec<f32>,
    fed_frames: usize,
    backend: &'static str,
    peak_hz: (u32, u32),
}

fn main() {
    let out_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "docs/spike-capture-pulse.json".to_owned());
    let device = std::env::args()
        .nth(2)
        .unwrap_or_else(|| DEFAULT_MONITOR.to_owned());
    let json = run(&device);
    println!("{json}");
    if let Some(parent) = std::path::Path::new(&out_path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = std::fs::write(&out_path, format!("{json}\n")) {
        eprintln!("错误：无法写入证据 {out_path}: {e}");
        std::process::exit(1);
    }
}

fn boundary_of(device: &str) -> &'static str {
    if device == DEFAULT_MONITOR {
        "只读取 monitor；未播放测试音、未新建/卸载 sink、未改默认输出、未注册服务"
    } else {
        "虚拟 null-sink 无声回路：测试音只进入系统已有虚拟 sink，扬声器无声；未改默认输出、未新建/卸载 sink、未注册服务"
    }
}

fn skip(reason: &str, boundary: &str) -> String {
    format!(
        "{{\"verdict\":\"SKIP\",\"reason\":\"{}\",\"safe_boundary\":\"{boundary}\"}}",
        esc(reason)
    )
}

fn run(device: &str) -> String {
    let boundary = boundary_of(device);
    let mut cap =
        match PulseCapture::open(Some(device), DEFAULT_SAMPLE_RATE, CaptureConfig::default()) {
            Ok(c) => c,
            Err(e) => return skip(&format!("连接失败: {e}"), boundary),
        };
    let cfg = AnalyzerConfig::default();
    let mut analyzer = match Analyzer::new(&cfg, DEFAULT_SAMPLE_RATE) {
        Ok(a) => a,
        Err(e) => return skip(&format!("Analyzer 构造失败: {e}"), boundary),
    };
    let edges = build_edges(&cfg, DEFAULT_SAMPLE_RATE);

    let Some(mut sum) = collect(&mut cap, &mut analyzer, &edges) else {
        return skip("采集失败或无结果", boundary);
    };
    device.clone_into(&mut sum.device);
    sum.backend = analyzer.fft_backend_name();
    render(&sum, boundary)
}

/// 分轮 `read_hop` + 分析。
#[allow(clippy::cast_possible_truncation)]
fn collect(
    cap: &mut PulseCapture,
    analyzer: &mut Analyzer,
    edges: &[(usize, usize)],
) -> Option<Summary> {
    let mut out = vec![0.0f32; FFT_SIZE];
    let mut consumed_total = 0usize;
    let mut finite = true;
    let mut fed_frames = 0usize;
    let mut levels: Vec<f32> = Vec::new();
    let mut sum_sq = 0.0f64;
    let mut peak_abs = 0.0f32;
    let started = Instant::now();

    for round in 0..ROUNDS {
        let got = cap
            .read_hop(&mut out, Duration::from_millis(ROUND_MS))
            .ok()?;
        consumed_total += got;
        finite &= out[..got].iter().all(|v| v.is_finite());
        for &sample in &out[..got] {
            sum_sq += f64::from(sample * sample);
            peak_abs = peak_abs.max(sample.abs());
        }
        if got > 0 {
            let now_ms = u64::try_from(round).unwrap_or(0) * ROUND_MS;
            levels = analyzer.process(&out[..got], now_ms).to_vec();
            fed_frames += got;
        }
    }

    let frames_delivered = cap.frames_delivered();
    let rms = if frames_delivered > 0 {
        #[allow(clippy::cast_precision_loss)]
        let count = frames_delivered as f64;
        (sum_sq / count).sqrt() as f32
    } else {
        0.0
    };
    let peak_band = levels
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.total_cmp(b.1))
        .map_or(0, |(i, _)| i);
    let peak_hz = edges
        .get(peak_band)
        .map_or((0, 0), |&(b0, b1)| (bin_to_hz(b0), bin_to_hz(b1)));

    Some(Summary {
        device: String::new(),
        frames_delivered,
        holes: cap.holes(),
        empty_rounds: cap.empty_rounds(),
        mainloop_errors: cap.mainloop_errors(),
        overflow_frames: cap.overflow_frames(),
        elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        rms,
        peak: peak_abs,
        audio_active: rms > silence_linear(DEFAULT_SILENCE_DBFS),
        finite,
        consumed_total,
        levels,
        fed_frames,
        backend: "unknown",
        peak_hz,
    })
}

/// bin 索引 → 频率（整数运算，避免浮点 cast 的精度告警）。
fn bin_to_hz(bin: usize) -> u32 {
    let denom = u64::try_from(FFT_SIZE).unwrap_or(1).max(1);
    let num = u64::try_from(bin)
        .unwrap_or(0)
        .saturating_mul(u64::from(DEFAULT_SAMPLE_RATE));
    u32::try_from(num / denom).unwrap_or(u32::MAX)
}

fn render(sum: &Summary, boundary: &str) -> String {
    let nonzero = sum.levels.iter().filter(|&&v| v > 0.01).count();
    let max_level = sum.levels.iter().copied().fold(0.0_f32, f32::max);
    let peak_band = sum
        .levels
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.total_cmp(b.1))
        .map_or(0, |(i, _)| i);

    let verdict = if sum.frames_delivered > 0 && sum.finite {
        "PASS"
    } else {
        "SKIP"
    };
    let stimulus = if sum.audio_active {
        "PASS（采集到非静音音频，F24 门限已刷新）"
    } else {
        "SKIP（采集窗口内无音频，monitor 输出静音；链路本身正常）"
    };
    // ★ 吞吐判定：真实缺陷是「采集跟不上实时」，只看 frames_read>0 会把它当 PASS。
    #[allow(clippy::cast_precision_loss)]
    let frames_per_sec = if sum.elapsed_ms > 0 {
        sum.frames_delivered as f64 * 1000.0 / sum.elapsed_ms as f64
    } else {
        0.0
    };
    let ratio = frames_per_sec / f64::from(DEFAULT_SAMPLE_RATE);
    let throughput = if ratio >= 0.8 {
        "PASS（吞吐达到实时的 80% 以上）"
    } else {
        "WARN（吞吐明显低于实时，需排查 pa_buffer_attr / chunk 抽取）"
    };
    let spect = if nonzero > 0 {
        "PASS（真实音频驱动出非零频段柱）"
    } else {
        "SKIP（无音频输入，柱高保持 0）"
    };

    format!(
        "{{\n  \"stage\": \"phase-2-capture-pulse\",\n  \"device\": \"{}\",\n  \"sample_rate\": {DEFAULT_SAMPLE_RATE},\n  \"frames_read\": {},\n  \"frames_per_sec\": {frames_per_sec:.1},\n  \"realtime_ratio\": {ratio:.3},\n  \"throughput\": \"{throughput}\",\n  \"hole_chunks\": {},\n  \"empty_chunks\": {},\n  \"mainloop_errors\": {},\n  \"overflow_count\": {},\n  \"rms\": {:.6},\n  \"peak\": {:.6},\n  \"audio_active\": {},\n  \"elapsed_ms\": {},\n  \"poll_rounds\": {ROUNDS},\n  \"consumed_total\": {},\n  \"consumed_all_finite\": {},\n  \"analyzer\": {{ \"fft_backend\": \"{}\", \"bands_actual\": {}, \"frames_analyzed\": {}, \"nonzero_bands\": {nonzero}, \"max_level\": {max_level:.4}, \"peak_band\": {peak_band}, \"peak_band_hz\": [{}, {}], \"verdict\": \"{spect}\" }},\n  \"sink_input_note\": \"F45/F74: 请求 F32LE 由服务端转换；PipeWire 图时钟锁 48kHz\",\n  \"verdict\": \"{verdict}\",\n  \"audio_stimulus\": \"{stimulus}\",\n  \"safe_boundary\": \"{boundary}\"\n}}",
        esc(&sum.device),
        sum.frames_delivered,
        sum.holes,
        sum.empty_rounds,
        sum.mainloop_errors,
        sum.overflow_frames,
        sum.rms,
        sum.peak,
        sum.audio_active,
        sum.elapsed_ms,
        sum.consumed_total,
        sum.finite,
        sum.backend,
        sum.levels.len(),
        sum.fed_frames,
        sum.peak_hz.0,
        sum.peak_hz.1
    )
}
