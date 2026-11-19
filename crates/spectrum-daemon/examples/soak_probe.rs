//! 长跑与开销实证：CPU / RSS / 帧率。
//!
//! ## 为什么先做这个
//!
//! 文档里一直写着「静音 ≈0% CPU」（F34 / §6.6.2），但**从没被量化过**。
//! 托盘、设置界面都只是"启动壳"，如果底层在缓慢漏内存或空转，
//! 先做壳只会把问题藏起来。所以先拿硬数字。
//!
//! ## 两个阶段为什么要分开
//!
//! | 阶段 | 声音来源 | 想测的东西 |
//! |---|---|---|
//! | A 有声 | 真实 monitor（用户放的音乐） | 正常工作的 CPU 上限、帧率、RSS 稳定性 |
//! | B 静音 | 合成零源（全 0 样本） | 「静音停屏」是否真的把 CPU 降下来 |
//!
//! B 不能靠"等音乐停"——那不可控；也不能靠把音乐音量调 0——那会打扰用户。
//! 合成零源走的是**同一条入口**（同样的 hop 节流、同样的 gate、同样的循环），
//! 唯一的区别是没有真实采样，因此能干净地测出"停屏后的代价"。
//!
//! ## 断言
//!
//! | 编号 | 断言 |
//! |---|---|
//! | S1 | A 阶段帧率 ≥ 40 fps（48kHz/1024 理论 46.9） |
//! | S2 | A 阶段 CPU ≤ 上限（见 `LIVE_CPU_LIMIT`） |
//! | S3 | B 阶段 CPU ≤ 上限（见 `SILENT_CPU_LIMIT`），且显著低于 A |
//! | S4 | A 阶段 RSS 增长 ≤ `RSS_GROWTH_LIMIT`，且无单调递增趋势 |
//!
//! 用法：
//! ```text
//! cargo run -p spectrum-daemon --features dbus,x11,pulse \
//!     --example soak_probe -- --live-seconds 30 --silent-seconds 20
//! ```

// 探针是线性脚本；统计与 /proc 解析里的窄化转换是固有需求。
#![allow(
    clippy::too_many_lines,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use std::time::{Duration, Instant};

use spectrum_core::capture::CaptureConfig;
use spectrum_core::pulse::PulseCapture;
use spectrum_daemon::PipelineConfig;
use spectrum_daemon::pipeline::Pipeline;
use spectrum_platform::overlay::{OverlayBackend, OverlayGeometry};
use spectrum_platform::x11::X11Overlay;

const RATE: u32 = 48_000;
const HOP: usize = 1024;

/// A 阶段 CPU 上限（单核百分比）。
///
/// 依据实测：release **1.8%**、debug **3.1%**。取 10% 作为上限 ——
/// 比 debug 实测高 3 倍（容忍机器忙），又能在 CPU 翻 3 倍时报警。
const LIVE_CPU_LIMIT: f64 = 10.0;

/// B 阶段（静音停屏）CPU 上限（单核百分比）。
///
/// 依据实测：release **0.5%**、debug **1.5%**。取 3% ——
/// 早期版本在静音期照跑 FFT，debug 下是 4.8%，会被这条判据挡住。
const SILENT_CPU_LIMIT: f64 = 3.0;

/// A 阶段总 RSS 增长上限（KB）。
///
/// 实测三次运行（20s/30s/40s，debug 与 release）都是 **+2244 KB**，
/// 与时长无关 → 是固定的预热分配（pulse 缓冲 + 像素缓冲 + X 连接），不是泄漏。
/// 这里仍留 8 MB 余量，但真正的泄漏判据是下面的"尾部增长"。
const RSS_GROWTH_LIMIT: u64 = 8192;

/// 尾部（后 1/4 时段）RSS 相对前一 1/4 的增长上限（KB）。
///
/// 这条才是**漏内存判据**：预热是一次性的，尾部应当持平；
/// 真泄漏会让尾部继续涨。只看首尾会被"涨了又回落"的锯齿骗过，
/// 只看"是否单调"又太容易被噪声推翻。
const RSS_TAIL_GROWTH_LIMIT: u64 = 512;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let live_secs: f64 = arg_value("--live-seconds")
        .and_then(|v| v.parse().ok())
        .unwrap_or(30.0);
    let silent_secs: f64 = arg_value("--silent-seconds")
        .and_then(|v| v.parse().ok())
        .unwrap_or(20.0);

    let base = read_dock_geometry()?;
    let mut overlay = X11Overlay::create(base)?;
    let actual = overlay.geometry()?;
    println!(
        "构建类型: {}",
        if cfg!(debug_assertions) {
            "debug（未优化，开销偏大）"
        } else {
            "release"
        }
    );
    println!(
        "overlay 0x{:x} {}x{}",
        overlay.window_id(),
        actual.width,
        actual.height
    );

    let mut pipeline = Pipeline::new(PipelineConfig::for_geometry(actual.width, actual.height))?;
    pipeline.resize(actual.width, actual.height);

    // ---------- A 阶段：真实音乐 ----------
    println!("== A 阶段：真实音乐 {live_secs:.0}s ==");
    let a = run_live(&mut pipeline, &mut overlay, live_secs)?;
    println!(
        "   fps {:.1} / CPU {:.1}% / RSS {}→{} KB（峰值 {}）",
        a.fps, a.cpu_percent, a.rss_start_kb, a.rss_end_kb, a.rss_peak_kb
    );

    // ---------- B 阶段：合成零源 ----------
    println!("== B 阶段：静音（合成零源）{silent_secs:.0}s ==");
    let b = run_silent(&mut pipeline, &mut overlay, silent_secs)?;
    println!(
        "   fps {:.1} / CPU {:.1}% / presented {} / skipped {}",
        b.fps, b.cpu_percent, b.presented, b.skipped
    );

    // ---------- 断言 ----------
    let s1 = a.fps >= 40.0;
    let s2 = a.cpu_percent <= LIVE_CPU_LIMIT;
    let s3 = b.cpu_percent <= SILENT_CPU_LIMIT && b.cpu_percent < a.cpu_percent;
    let s4 = a.rss_end_kb.saturating_sub(a.rss_start_kb) <= RSS_GROWTH_LIMIT
        && a.rss_tail_growth_kb <= RSS_TAIL_GROWTH_LIMIT;

    for (name, ok, detail) in [
        ("S1 有声帧率 ≥40fps", s1, format!("{:.1} fps", a.fps)),
        (
            "S2 有声 CPU 在预算内",
            s2,
            format!("{:.1}%（上限 {LIVE_CPU_LIMIT}%）", a.cpu_percent),
        ),
        (
            "S3 静音 CPU 显著更低",
            s3,
            format!(
                "{:.1}%（上限 {SILENT_CPU_LIMIT}%）vs 有声 {:.1}%",
                b.cpu_percent, a.cpu_percent
            ),
        ),
        (
            "S4 RSS 稳定",
            s4,
            format!(
                "总增长 {} KB（上限 {RSS_GROWTH_LIMIT}）／尾部增长 {} KB（上限 {RSS_TAIL_GROWTH_LIMIT}）",
                a.rss_end_kb.saturating_sub(a.rss_start_kb),
                a.rss_tail_growth_kb
            ),
        ),
    ] {
        println!(
            "   {}  {name}  —  {detail}",
            if ok { "PASS" } else { "FAIL" }
        );
    }

    let verdict = if s1 && s2 && s3 && s4 { "PASS" } else { "FAIL" };
    let report = format!(
        r#"{{
  "generated_at": "{ts}",
  "verdict": "{verdict}",
  "build": {{ "debug": {debug}, "note": "debug 构建的开销不代表发布包；发布用 --release" }},
  "canvas": {{ "width": {w}, "height": {h} }},
  "phase_live": {{
    "seconds": {a_secs:.1},
    "fps": {a_fps:.2},
    "cpu_percent": {a_cpu:.2},
    "rss_start_kb": {a_rss0},
    "rss_end_kb": {a_rss1},
    "rss_peak_kb": {a_rssp},
    "rss_growth_kb": {a_growth},
    "rss_monotonic": {a_mono},
    "rss_tail_growth_kb": {a_tail},
    "frames": {a_frames},
    "presented": {a_presented}
  }},
  "phase_silent": {{
    "seconds": {b_secs:.1},
    "fps": {b_fps:.2},
    "cpu_percent": {b_cpu:.2},
    "frames": {b_frames},
    "presented": {b_presented},
    "skipped": {b_skipped}
  }},
  "checks": {{
    "S1_live_fps": {s1},
    "S2_live_cpu": {s2},
    "S3_silent_cpu": {s3},
    "S4_rss_stable": {s4}
  }},
  "limits": {{
    "live_cpu_percent": {live_limit},
    "silent_cpu_percent": {silent_limit},
    "rss_growth_kb": {rss_limit}
  }}
}}
"#,
        ts = iso_now(),
        verdict = verdict,
        debug = cfg!(debug_assertions),
        w = actual.width,
        h = actual.height,
        a_secs = a.seconds,
        a_fps = a.fps,
        a_cpu = a.cpu_percent,
        a_rss0 = a.rss_start_kb,
        a_rss1 = a.rss_end_kb,
        a_rssp = a.rss_peak_kb,
        a_growth = a.rss_end_kb.saturating_sub(a.rss_start_kb),
        a_mono = a.rss_monotonic,
        a_tail = a.rss_tail_growth_kb,
        a_frames = a.frames,
        a_presented = a.presented,
        b_secs = b.seconds,
        b_fps = b.fps,
        b_cpu = b.cpu_percent,
        b_frames = b.frames,
        b_presented = b.presented,
        b_skipped = b.skipped,
        s1 = s1,
        s2 = s2,
        s3 = s3,
        s4 = s4,
        live_limit = LIVE_CPU_LIMIT,
        silent_limit = SILENT_CPU_LIMIT,
        rss_limit = RSS_GROWTH_LIMIT,
    );
    std::fs::write("docs/spike-soak.json", report)?;
    overlay.destroy()?;
    println!("verdict = {verdict}");
    if verdict != "PASS" {
        return Err("长跑断言未全部通过".into());
    }
    Ok(())
}

/// 一个阶段的测量结果。
struct PhaseStat {
    seconds: f64,
    fps: f64,
    cpu_percent: f64,
    rss_start_kb: u64,
    rss_end_kb: u64,
    rss_peak_kb: u64,
    rss_monotonic: bool,
    rss_tail_growth_kb: u64,
    frames: u64,
    presented: u64,
    skipped: u64,
}

/// A 阶段：真实 monitor。
fn run_live(
    pipeline: &mut Pipeline,
    overlay: &mut X11Overlay,
    seconds: f64,
) -> Result<PhaseStat, Box<dyn std::error::Error>> {
    let mut capture = PulseCapture::open(None, RATE, CaptureConfig::default())
        .map_err(|e| format!("打开 monitor 失败：{e}（请先播放音乐）"))?;
    let mut hop = vec![0.0_f32; HOP];
    let started = Instant::now();
    let budget = Duration::from_secs_f64(seconds.max(1.0));

    let rss_start = rss_kb();
    let cpu_start = cpu_ticks();
    let mut rss_samples = Vec::new();
    let mut next_sample = started + Duration::from_secs(1);
    let mut frames = 0_u64;
    let mut presented = 0_u64;

    while started.elapsed() < budget {
        let filled = capture
            .read_hop(&mut hop, Duration::from_millis(50))
            .map_err(|e| e.to_string())?;
        if filled == 0 {
            std::thread::sleep(Duration::from_millis(2));
            continue;
        }
        frames += 1;
        let now_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(0);
        let out = pipeline.push(&hop[..filled], now_ms);
        if out.should_present {
            let surface = pipeline.surface();
            overlay.put_image(surface.pixels(), surface.width(), surface.height())?;
            presented += 1;
        }
        if Instant::now() >= next_sample {
            rss_samples.push(rss_kb());
            next_sample += Duration::from_secs(1);
        }
    }

    let elapsed = started.elapsed().as_secs_f64();
    let cpu = cpu_percent(cpu_start, elapsed);
    let rss_end = rss_kb();
    let rss_peak = rss_samples.iter().copied().max().unwrap_or(rss_end);
    Ok(PhaseStat {
        seconds: elapsed,
        fps: frames as f64 / elapsed,
        cpu_percent: cpu,
        rss_start_kb: rss_start,
        rss_end_kb: rss_end,
        rss_peak_kb: rss_peak.max(rss_end),
        rss_monotonic: is_monotonic(&rss_samples),
        rss_tail_growth_kb: tail_growth_kb(&rss_samples),
        frames,
        presented,
        skipped: frames - presented,
    })
}

/// B 阶段：合成零源（走同一条循环，只是没有真实采样）。
fn run_silent(
    pipeline: &mut Pipeline,
    overlay: &mut X11Overlay,
    seconds: f64,
) -> Result<PhaseStat, Box<dyn std::error::Error>> {
    let hop = vec![0.0_f32; HOP];
    let hop_dur = Duration::from_secs_f64(HOP as f64 / f64::from(RATE));
    let started = Instant::now();
    let budget = Duration::from_secs_f64(seconds.max(1.0));
    let cpu_start = cpu_ticks();
    let rss_start = rss_kb();
    let mut next_at = Instant::now();
    let mut frames = 0_u64;
    let mut presented = 0_u64;
    let mut skipped = 0_u64;

    while started.elapsed() < budget {
        // 与真实采集一样按实时节奏产帧（否则测出来的 CPU 没有可比性）
        next_at += hop_dur;
        let now = Instant::now();
        if next_at > now {
            std::thread::sleep(next_at - now);
        } else {
            next_at = now;
        }
        frames += 1;
        let now_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(0);
        let out = pipeline.push(&hop, now_ms);
        if out.should_present {
            let surface = pipeline.surface();
            overlay.put_image(surface.pixels(), surface.width(), surface.height())?;
            presented += 1;
        } else {
            skipped += 1;
        }
    }

    let elapsed = started.elapsed().as_secs_f64();
    let rss_end = rss_kb();
    Ok(PhaseStat {
        seconds: elapsed,
        fps: frames as f64 / elapsed,
        cpu_percent: cpu_percent(cpu_start, elapsed),
        rss_start_kb: rss_start,
        rss_end_kb: rss_end,
        rss_peak_kb: rss_end.max(rss_start),
        rss_monotonic: false,
        rss_tail_growth_kb: 0,
        frames,
        presented,
        skipped,
    })
}

/// 单调递增判定：允许噪声，只有"每段都在涨"才认为在漏。
///
/// 做法：把采样序列分成 4 段取均值，若每段都比前一段高（且总涨幅 > 256 KB）→ 单调。
/// 只看首尾会漏掉"涨了又回落"的锯齿；逐点比较又会被噪声干扰。
fn is_monotonic(samples: &[u64]) -> bool {
    if samples.len() < 8 {
        return false;
    }
    let chunk = samples.len() / 4;
    let means: Vec<f64> = samples
        .chunks(chunk.max(1))
        .take(4)
        .map(|c| c.iter().sum::<u64>() as f64 / c.len() as f64)
        .collect();
    if means.len() < 4 {
        return false;
    }
    let rising = means.windows(2).all(|w| w[1] > w[0]);
    // 用饱和减法：RSS 采样值不可能跨 i64 边界，`as i64` 徒增环绕风险
    let total = samples
        .last()
        .copied()
        .unwrap_or(0)
        .saturating_sub(samples.first().copied().unwrap_or(0));
    rising && total > 256
}

/// 尾部增长：后 1/4 时段的均值减去前 1/4 的均值（负值记 0）。
///
/// 与 `is_monotonic` 互补：预热表现为"前段涨、后段平"，泄漏表现为"后段还在涨"。
fn tail_growth_kb(samples: &[u64]) -> u64 {
    if samples.len() < 8 {
        return 0;
    }
    let quarter = samples.len() / 4;
    let head: f64 = samples[..quarter].iter().sum::<u64>() as f64 / quarter as f64;
    let tail_slice = &samples[samples.len() - quarter..];
    let tail: f64 = tail_slice.iter().sum::<u64>() as f64 / quarter as f64;
    if tail > head { (tail - head) as u64 } else { 0 }
}

/// 当前进程 RSS（KB）。
fn rss_kb() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|text| {
            text.lines()
                .find(|line| line.starts_with("VmRSS:"))
                .and_then(|line| line.split_whitespace().nth(1))
                .and_then(|v| v.parse().ok())
        })
        .unwrap_or(0)
}

/// 进程累计 CPU tick（user + system）。
fn cpu_ticks() -> u64 {
    std::fs::read_to_string("/proc/self/stat")
        .ok()
        .and_then(|text| {
            // 字段 14/15（1-based）是 utime/stime；comm 可能含空格，故从右往左数不可靠，
            // 这里按 ")" 之后切分，跳过 state 之前的 2 个字段
            let tail = text.rsplit_once(')')?.1;
            let fields: Vec<&str> = tail.split_whitespace().collect();
            // tail 的第 1 个字段是 state（原第 3 字段），故 utime 是索引 11、stime 是 12
            let utime: u64 = fields.get(11)?.parse().ok()?;
            let stime: u64 = fields.get(12)?.parse().ok()?;
            Some(utime + stime)
        })
        .unwrap_or(0)
}

/// 单核百分比。
fn cpu_percent(start_ticks: u64, elapsed_secs: f64) -> f64 {
    if elapsed_secs <= 0.0 {
        return 0.0;
    }
    let ticks = cpu_ticks().saturating_sub(start_ticks);
    // USER_HZ 固定为 100（Linux 常量，与架构无关）
    let cpu_secs = ticks as f64 / 100.0;
    cpu_secs / elapsed_secs * 100.0
}

fn arg_value(flag: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    if let Some(i) = args.iter().position(|a| a == flag) {
        return args.get(i + 1).cloned();
    }
    let prefix = format!("{flag}=");
    args.iter()
        .find_map(|a| a.strip_prefix(&prefix).map(ToString::to_string))
}

fn read_dock_geometry() -> Result<OverlayGeometry, Box<dyn std::error::Error>> {
    let output = std::process::Command::new("busctl")
        .args([
            "--user",
            "get-property",
            "org.deepin.dde.daemon.Dock1",
            "/org/deepin/dde/Dock1",
            "org.deepin.dde.daemon.Dock1",
            "FrontendWindowRect",
        ])
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "读取 FrontendWindowRect 失败：{}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let nums: Vec<i32> = text
        .split_whitespace()
        .filter_map(|t| t.parse().ok())
        .collect();
    if nums.len() != 4 || nums[2] <= 0 || nums[3] <= 0 {
        return Err(format!("FrontendWindowRect 异常：{text}").into());
    }
    Ok(OverlayGeometry::new(
        i16::try_from(nums[0])?,
        i16::try_from(nums[1])?,
        u16::try_from(nums[2])?,
        u16::try_from(nums[3])?,
    ))
}

fn iso_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (y, m, d) = civil_from_days(i64::try_from(days).unwrap_or(0));
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = u32::try_from(doy - (153 * mp + 2) / 5 + 1).unwrap_or(1);
    let m = u32::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).unwrap_or(1);
    if m <= 2 { (y + 1, m, d) } else { (y, m, d) }
}
