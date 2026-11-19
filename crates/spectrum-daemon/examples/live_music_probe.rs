//! 真实音乐实证：monitor 采集 → 分析 → 渲染 → 上屏 → 回读。
//!
//! 上一轮 `pipeline_probe` 证明的是"给定 PCM，画面正确"（声卡换成合成音）。
//! 本探针补的是最后一段：**声音来自真实播放的音乐**。
//!
//! ## 断言
//!
//! | 编号 | 断言 | 说明 |
//! |---|---|---|
//! | L1 | 采集吞吐接近实时 | hop 数 ≈ 时长 × `rate/hop`，容差 ±30% |
//! | L2 | 确实有声音 | 活跃帧占比 ≥ 50%，峰值 RMS 远超静音门限 |
//! | L3 | 柱高在动 | 峰值频段下标出现 ≥3 种，或帧峰值电平标准差 > 0.03 |
//! | L4 | 频谱是宽带的 | 至少 8 个频段在整段里曾超过 0.3（音乐不是单音） |
//! | L5 | 上屏像素 == 缓冲像素 | 回读逐 u32 相等 |
//!
//! L3 是这次的核心：没有它，"画出了柱子"和"跟着音乐动"无法区分 ——
//! 一张静止的柱子图也能通过前四条。
//!
//! ## 用法
//!
//! ```text
//! cargo run -p spectrum-daemon --features dbus,x11,pulse \
//!     --example live_music_probe -- --seconds 12 [--hold 5]
//! ```
//!
//! 产物：`docs/spike-live-music.json`、`docs/live-preview.rgba` + `.meta.json`。

// 探针是线性脚本，拆函数反而更难对照断言编号；统计里的窄化转换是固有需求。
#![allow(
    clippy::too_many_lines,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use spectrum_core::capture::{CaptureConfig, DEFAULT_SILENCE_DBFS, silence_linear};
use spectrum_core::pulse::PulseCapture;
use spectrum_daemon::PipelineConfig;
use spectrum_daemon::pipeline::Pipeline;
use spectrum_platform::overlay::{OverlayBackend, OverlayGeometry};
use spectrum_platform::x11::X11Overlay;

const RATE: u32 = 48_000;
const HOP: usize = 1024;

/// 能量最高的一帧：用于出预览图与 L5 比对。
struct BestFrame {
    energy: f64,
    pixels: Vec<u32>,
    bands: Vec<f32>,
    width: u16,
    height: u16,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let seconds: f64 = arg_value("--seconds")
        .and_then(|v| v.parse().ok())
        .unwrap_or(12.0);
    let hold: u64 = arg_value("--hold")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    let base = read_dock_geometry()?;
    let mut overlay = X11Overlay::create(base)?;
    let actual = overlay.geometry()?;
    println!(
        "overlay 0x{:x} {}x{} @ ({}, {})",
        overlay.window_id(),
        actual.width,
        actual.height,
        actual.x,
        actual.y
    );

    let mut pipeline = Pipeline::new(PipelineConfig::for_geometry(actual.width, actual.height))?;
    pipeline.resize(actual.width, actual.height);
    println!(
        "pipeline {} 频段 / hop {HOP} / {RATE}Hz；采集 {} s",
        pipeline.band_count(),
        seconds
    );

    let mut capture = PulseCapture::open(None, RATE, CaptureConfig::default())
        .map_err(|e| format!("打开 monitor 失败：{e}（是否没有音频服务？）"))?;

    // 门限必须与 Pipeline 内部那个一致，否则 L2 的对比没有意义
    let threshold = silence_linear(DEFAULT_SILENCE_DBFS);
    let mut hop_buf = vec![0.0_f32; HOP];
    let started = Instant::now();
    let budget = Duration::from_secs_f64(seconds.max(1.0));

    let mut hops = 0_u64;
    let mut active_frames = 0_u64;
    let mut max_rms = 0.0_f32;
    let mut rms_sum = 0.0_f64;
    let mut frame_max_levels: Vec<f32> = Vec::new();
    let mut argmax_hist: BTreeMap<usize, u32> = BTreeMap::new();
    let mut peak_hz_hist: BTreeMap<String, u32> = BTreeMap::new();
    let mut band_max: Vec<f32> = vec![0.0; pipeline.band_count()];
    // 能量最高的一帧：用于出预览图（不能拿最后一帧，可能正好是静音段）
    let mut best: Option<BestFrame> = None;

    while started.elapsed() < budget {
        let filled = capture
            .read_hop(&mut hop_buf, Duration::from_millis(50))
            .map_err(|e| e.to_string())?;
        if filled == 0 {
            std::thread::sleep(Duration::from_millis(2));
            continue;
        }
        hops += 1;

        let rms = rms_of(&hop_buf[..filled]);
        rms_sum += f64::from(rms);
        max_rms = max_rms.max(rms);

        let now_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(0);
        let out = pipeline.push(&hop_buf[..filled], now_ms);
        if out.active {
            active_frames += 1;
        }
        for (index, &level) in out.bands.iter().enumerate() {
            if level > band_max[index] {
                band_max[index] = level;
            }
        }
        let frame_max = out.bands.iter().copied().fold(0.0_f32, f32::max);
        frame_max_levels.push(frame_max);
        if let Some(index) = argmax(&out.bands) {
            *argmax_hist.entry(index).or_insert(0) += 1;
        }
        if let Some(hz) = out.peak_hz {
            // 按 1/3 倍频程粗分桶，避免把每个 bin 都记成一个"频段"
            let bucket = format!("{:.0}", round_to_third_octave(hz));
            *peak_hz_hist.entry(bucket).or_insert(0) += 1;
        }

        // 记录能量最高帧（含上屏后的回读用来做 L5）
        let energy: f64 = out.bands.iter().map(|&v| f64::from(v)).sum();
        if best.as_ref().is_none_or(|b| energy > b.energy) {
            let surface = pipeline.surface();
            let (w, h) = (surface.width(), surface.height());
            if out.should_present {
                overlay.put_image(surface.pixels(), w, h)?;
                let readback = overlay.get_image(0, 0, w, h)?;
                let mismatch = readback
                    .iter()
                    .zip(surface.pixels().iter())
                    .filter(|(a, b)| a != b)
                    .count();
                if mismatch != 0 {
                    println!("  ⚠ 帧回读有 {mismatch} 个像素不一致");
                }
                best = Some(BestFrame {
                    energy,
                    pixels: surface.pixels().to_vec(),
                    bands: out.bands.clone(),
                    width: w,
                    height: h,
                });
            }
        }
    }

    let elapsed = started.elapsed().as_secs_f64();
    let expected_hops = elapsed * f64::from(RATE) / HOP as f64;
    let active_ratio = if hops == 0 {
        0.0
    } else {
        active_frames as f64 / hops as f64
    };
    let mean_rms = if hops == 0 {
        0.0
    } else {
        rms_sum / hops as f64
    };
    let distinct_argmax = argmax_hist.len();
    let level_std = std_dev(&frame_max_levels);
    let wide_bands = band_max.iter().filter(|&&v| v > 0.3).count();

    let BestFrame {
        energy,
        pixels: best_pixels,
        bands: best_bands,
        width: best_w,
        height: best_h,
    } = best.ok_or("整段没有采集到可上屏的帧（是否有音乐在播放？）")?;

    // ---------- 断言 ----------
    let l1 = expected_hops > 0.0 && (hops as f64) >= expected_hops * 0.7;
    let l2 = active_ratio >= 0.5 && max_rms > threshold * 3.0;
    let l3 = distinct_argmax >= 3 || level_std > 0.03;
    let l4 = wide_bands >= 8;
    let readback = overlay.get_image(0, 0, best_w, best_h)?;
    let l5 = readback.iter().zip(best_pixels.iter()).all(|(a, b)| a == b);
    let lit = best_pixels.iter().filter(|&&p| p >> 24 != 0).count();

    for (name, ok, detail) in [
        (
            "L1 采集吞吐接近实时",
            l1,
            format!("{hops} hop / {elapsed:.2}s（理论 {expected_hops:.1}）"),
        ),
        (
            "L2 确实有声音",
            l2,
            // ⚠ 比率要乘 100 再格式化：之前直接 `{active_ratio:.0}%` 会把 0.99 显示成 "1%"，
            // 看着像"几乎没声音"，实际是满活跃（JSON 里是正确的 1.0）
            format!(
                "活跃帧 {:.1}%，峰值 RMS {max_rms:.4}（门限 {threshold:.5}）",
                active_ratio * 100.0
            ),
        ),
        (
            "L3 柱高在动",
            l3,
            format!("峰值频段 {distinct_argmax} 种，帧峰值电平标准差 {level_std:.3}"),
        ),
        ("L4 频谱是宽带的", l4, format!("{wide_bands} 个频段曾 >0.3")),
        (
            "L5 上屏像素一致",
            l5,
            format!("回读 {} 像素", best_w as usize * best_h as usize),
        ),
    ] {
        println!(
            "   {}  {name}  —  {detail}",
            if ok { "PASS" } else { "FAIL" }
        );
    }

    // 前 5 个最常出现的峰值频率（供人眼判断"像不像音乐"）
    let mut hz_sorted: Vec<(&String, &u32)> = peak_hz_hist.iter().collect();
    hz_sorted.sort_by(|a, b| b.1.cmp(a.1));
    let top_hz: Vec<String> = hz_sorted
        .iter()
        .take(5)
        .map(|(hz, count)| format!("{{ \"hz\": {hz}, \"hops\": {count} }}"))
        .collect();
    println!(
        "  峰值频率 top5: {}",
        hz_sorted
            .iter()
            .take(5)
            .map(|(hz, c)| format!("{hz}Hz×{c}"))
            .collect::<Vec<_>>()
            .join(", ")
    );

    let verdict = if l1 && l2 && l3 && l4 && l5 {
        "PASS"
    } else {
        "FAIL"
    };

    // ---------- 落盘 ----------
    let report = format!(
        r#"{{
  "generated_at": "{ts}",
  "verdict": "{verdict}",
  "capture": {{
    "device": "{device}",
    "sample_rate": {rate},
    "hop_frames": {hop},
    "seconds": {elapsed:.2},
    "hops": {hops},
    "expected_hops": {expected:.1},
    "frames_delivered": {delivered},
    "empty_rounds": {empty},
    "holes": {holes}
  }},
  "signal": {{
    "max_rms": {max_rms:.5},
    "mean_rms": {mean_rms:.5},
    "silence_threshold": {threshold:.6},
    "active_frame_ratio": {active_ratio:.3},
    "distinct_peak_bands": {distinct_argmax},
    "frame_peak_level_std": {level_std:.4},
    "wide_bands_over_0_3": {wide_bands},
    "total_bands": {bands}
  }},
  "peak_hz_top5": [{top_hz}],
  "best_frame": {{
    "energy": {energy:.3},
    "lit_pixels": {lit},
    "width": {best_w},
    "height": {best_h},
    "levels_sample": [{levels}]
  }},
  "checks": {{
    "L1_throughput_realtime": {l1},
    "L2_has_sound": {l2},
    "L3_bars_move": {l3},
    "L4_broadband": {l4},
    "L5_present_matches_buffer": {l5}
  }}
}}
"#,
        ts = iso_now(),
        verdict = verdict,
        device = spectrum_core::pulse::DEFAULT_MONITOR,
        rate = RATE,
        hop = HOP,
        elapsed = elapsed,
        hops = hops,
        expected = expected_hops,
        delivered = capture.frames_delivered(),
        empty = capture.empty_rounds(),
        holes = capture.holes(),
        max_rms = max_rms,
        mean_rms = mean_rms,
        threshold = threshold,
        active_ratio = active_ratio,
        distinct_argmax = distinct_argmax,
        level_std = level_std,
        wide_bands = wide_bands,
        bands = pipeline.band_count(),
        top_hz = top_hz.join(", "),
        energy = energy,
        lit = lit,
        best_w = best_w,
        best_h = best_h,
        levels = best_bands
            .iter()
            .take(16)
            .map(|v| format!("{v:.3}"))
            .collect::<Vec<_>>()
            .join(", "),
        l1 = l1,
        l2 = l2,
        l3 = l3,
        l4 = l4,
        l5 = l5,
    );
    std::fs::write("docs/spike-live-music.json", report)?;
    write_preview(&best_pixels, best_w, best_h)?;

    if hold > 0 {
        overlay.put_image(&best_pixels, best_w, best_h)?;
        println!("保持 {hold}s（展示能量最高的一帧）");
        std::thread::sleep(Duration::from_secs(hold));
    }
    overlay.destroy()?;
    println!("verdict = {verdict}");
    if verdict != "PASS" {
        return Err("真实音乐断言未全部通过".into());
    }
    Ok(())
}

fn rms_of(pcm: &[f32]) -> f32 {
    if pcm.is_empty() {
        return 0.0;
    }
    let sum: f32 = pcm.iter().map(|s| s * s).sum();
    (sum / pcm.len() as f32).sqrt()
}

fn argmax(values: &[f32]) -> Option<usize> {
    values
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(index, _)| index)
}

fn std_dev(values: &[f32]) -> f32 {
    if values.len() < 2 {
        return 0.0;
    }
    let mean = values.iter().sum::<f32>() / values.len() as f32;
    let var = values.iter().map(|v| (v - mean).powi(2)).sum::<f32>() / values.len() as f32;
    var.sqrt()
}

/// 归到 1/3 倍频程中心频率，便于把峰值频率做成直方图。
fn round_to_third_octave(hz: f32) -> f32 {
    if hz <= 0.0 {
        return 0.0;
    }
    let n = (hz / 1000.0).log2() * 3.0;
    (2.0_f32.powf(n.round() / 3.0)) * 1000.0
}

fn write_preview(
    pixels: &[u32],
    width: u16,
    height: u16,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut rgba = Vec::with_capacity(pixels.len() * 4);
    for &pixel in pixels {
        rgba.extend_from_slice(&[
            ((pixel >> 16) & 0xFF) as u8,
            ((pixel >> 8) & 0xFF) as u8,
            (pixel & 0xFF) as u8,
            ((pixel >> 24) & 0xFF) as u8,
        ]);
    }
    std::fs::write("docs/live-preview.rgba", &rgba)?;
    std::fs::write(
        "docs/live-preview.meta.json",
        format!(
            "{{\n  \"width\": {width},\n  \"height\": {height},\n  \"format\": \"rgba-premultiplied\"\n}}\n"
        ),
    )?;
    Ok(())
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
