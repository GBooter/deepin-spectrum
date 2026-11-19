//! 端到端实证：PCM → 分析 → 渲染 → 上屏 → 回读 → 断言。
//!
//! 这是「频谱跟着音乐跳」这条链路的**整链验证**，只把「声卡」换成合成 PCM：
//! 采集部分单独由 `spectrum_core::pulse` 负责（见 `--live` 说明），
//! 本探针证明的是**声音内容真的改变了画面内容**，而不只是"窗口里有东西"。
//!
//! ## 断言
//!
//! | 编号 | 断言 | 为什么重要 |
//! |---|---|---|
//! | P1 | 有声音时触发上屏，静音超时后停止上屏 | F34：静音 0 fps，否则白烧 CPU |
//! | P2 | 主频识别正确 | 分析链路没接错（bin/Hz 换算、采样率） |
//! | P3 | 像素柱高 == 柱高 × 可用高度 | 渲染层与数据层对得上 |
//! | P4 | 画面最高柱所在的柱 == 数据里最响的频段 | 布局没错位（槽宽/余数分配） |
//! | P5 | 500 Hz 与 4 kHz 的峰值柱**位置右移** | 频率→横坐标映射正确，不是"随便跳动" |
//! | P6 | 静音边沿补一帧全透明 | 不留残影 |
//!
//! 用法：
//! ```text
//! cargo run -p spectrum-daemon --features dbus,x11 --example pipeline_probe -- [--hold N]
//! ```

// 探针是线性脚本，拆函数反而更难对照断言编号；像素统计里的窄化转换是固有需求。
#![allow(
    clippy::too_many_lines,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use std::time::{Duration, Instant};

use spectrum_core::analyzer::sine_wave;
use spectrum_daemon::PipelineConfig;
use spectrum_daemon::pipeline::Pipeline;
use spectrum_platform::overlay::{OverlayBackend, OverlayGeometry};
use spectrum_platform::x11::X11Overlay;

const RATE: u32 = 48_000;
const HOP: usize = 1024;
/// 每个音高喂多少帧（1024 × 24 / 48000 = 512ms，足够 attack/release 收敛）。
const HOPS: usize = 24;
/// 柱高比对容差：布局是整数运算，理论上应当零误差。
const TOLERANCE_PX: u16 = 0;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let hold_secs: u64 = arg_value("--hold")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    let base = read_dock_geometry()?;
    let mut overlay = X11Overlay::create(base)?;
    let actual = overlay.geometry()?;
    println!(
        "dock {}x{} @ ({}, {}) → overlay 0x{:x}",
        actual.width,
        actual.height,
        actual.x,
        actual.y,
        overlay.window_id()
    );

    let mut pip = Pipeline::new(PipelineConfig::for_geometry(actual.width, actual.height))?;
    println!(
        "pipeline: {} 频段 / hop {} 帧 / {}x{}",
        pip.band_count(),
        HOP,
        actual.width,
        actual.height
    );

    // ---------- 场景 A：500 Hz ----------
    let low = run_tone(&mut pip, &mut overlay, 500.0, 0)?;
    println!(
        "A 500Hz  : peak={:?}Hz argmax=#{} h={}px presented={}",
        low.peak_hz.map(|f| (f * 10.0).round() / 10.0),
        low.measured_argmax,
        low.measured_peak_px,
        low.presented
    );

    // ---------- 场景 B：4 kHz ----------
    let high = run_tone(&mut pip, &mut overlay, 4_000.0, 1_000)?;
    println!(
        "B 4kHz   : peak={:?}Hz argmax=#{} h={}px presented={}",
        high.peak_hz.map(|f| (f * 10.0).round() / 10.0),
        high.measured_argmax,
        high.measured_peak_px,
        high.presented
    );

    // ★ 立即快照"有内容"的那一帧。
    // 如果等到最后再取 surface，拿到的是静音清屏后的全透明帧 ——
    // 预览图会是一片空白，看起来像"渲染失败"（本轮实测踩到）。
    let lit_snapshot = pip.surface().pixels().to_vec();
    let lit_pixels = lit_snapshot.iter().filter(|&&p| p >> 24 != 0).count();

    // ---------- 场景 C：静音边沿 ----------
    let silence = run_silence(&mut pip, &mut overlay, 2_000)?;
    println!(
        "C 静音   : 清屏帧全透明={} 之后停止上屏={}",
        silence.cleared, silence.stopped
    );

    // ---------- 断言 ----------
    let p1 = low.presented > 0 && high.presented > 0 && silence.stopped;
    let p2a = low.peak_hz.is_some_and(|f| (f - 500.0).abs() < 250.0);
    let p2b = high.peak_hz.is_some_and(|f| (f - 4_000.0).abs() < 600.0);
    let p3 = low.height_mismatches == 0 && high.height_mismatches == 0;
    let p4 = low.measured_argmax == low.data_argmax && high.measured_argmax == high.data_argmax;
    let p5 = high.measured_argmax > low.measured_argmax;
    let p6 = silence.cleared;
    // 预览帧自检：证据里的图必须真的画了东西，空白图不能算证据
    let p7 = lit_pixels > 0;

    for (name, ok, detail) in [
        (
            "P1 上屏/停屏时机",
            p1,
            format!(
                "presented A={} B={} 静音后停止={}",
                low.presented, high.presented, silence.stopped
            ),
        ),
        (
            "P2 主频识别",
            p2a && p2b,
            format!(
                "500Hz→{:?} / 4kHz→{:?}",
                low.peak_hz.map(f32::round),
                high.peak_hz.map(f32::round)
            ),
        ),
        (
            "P3 柱高一致",
            p3,
            format!(
                "不符 A={} B={}（容差 {TOLERANCE_PX}px）",
                low.height_mismatches, high.height_mismatches
            ),
        ),
        (
            "P4 最高柱位置 == 最响频段",
            p4,
            format!(
                "A {}vs{} / B {}vs{}",
                low.measured_argmax, low.data_argmax, high.measured_argmax, high.data_argmax
            ),
        ),
        (
            "P5 频率升高→峰值柱右移",
            p5,
            format!("#{} → #{}", low.measured_argmax, high.measured_argmax),
        ),
        (
            "P6 静音边沿清屏",
            p6,
            format!("清屏帧全透明={}", silence.cleared),
        ),
    ] {
        println!(
            "   {}  {name}  —  {detail}",
            if ok { "PASS" } else { "FAIL" }
        );
    }

    let verdict = if p1 && p2a && p2b && p3 && p4 && p5 && p6 && p7 {
        "PASS"
    } else {
        "FAIL"
    };

    // ---------- 落盘证据 ----------
    let report = format!(
        r#"{{
  "generated_at": "{ts}",
  "verdict": "{verdict}",
  "canvas": {{ "width": {w}, "height": {h} }},
  "pipeline": {{ "bands": {bands}, "hop_frames": {hop}, "sample_rate": {rate} }},
  "scenario_low": {low},
  "scenario_high": {high},
  "scenario_silence": {sil},
  "checks": {{
    "P1_present_timing": {p1},
    "P2_peak_hz": {p2},
    "P3_bar_heights": {p3},
    "P4_argmax_alignment": {p4},
    "P5_frequency_shifts_position": {p5},
    "P6_silence_clears": {p6},
    "P7_preview_not_blank": {p7},
    "P7_preview_lit_pixels": {lit}
  }},
  "totals": {{ "frames": {frames}, "presented": {presented}, "skipped_silent": {skipped} }}
}}
"#,
        ts = iso_now(),
        verdict = verdict,
        w = actual.width,
        h = actual.height,
        bands = pip.band_count(),
        hop = HOP,
        rate = RATE,
        low = low.to_json(),
        high = high.to_json(),
        sil = silence.to_json(),
        p1 = p1,
        p2 = p2a && p2b,
        p3 = p3,
        p4 = p4,
        p5 = p5,
        p6 = p6,
        p7 = p7,
        lit = lit_pixels,
        frames = pip.stats().frames,
        presented = pip.stats().presented,
        skipped = pip.stats().skipped_silent,
    );
    std::fs::write("docs/spike-pipeline.json", report)?;
    write_preview(&lit_snapshot, actual.width, actual.height)?;

    if hold_secs > 0 {
        // 静音场景已经把窗口清空了，这里把 4kHz 那一帧重新送上去，
        // 否则 --hold 期间用户看到的是空白
        overlay.put_image(&lit_snapshot, actual.width, actual.height)?;
        println!("保持 {hold_secs}s（画面为 4kHz 场景）");
        std::thread::sleep(Duration::from_secs(hold_secs));
    }
    overlay.destroy()?;
    println!("verdict = {verdict}");
    if verdict != "PASS" {
        return Err("流水线断言未全部通过".into());
    }
    Ok(())
}

/// 单场景结果。
struct Scenario {
    peak_hz: Option<f32>,
    /// 数据侧最响频段下标。
    data_argmax: usize,
    /// 像素侧最高柱下标。
    measured_argmax: usize,
    /// 像素侧最高柱高度。
    measured_peak_px: u16,
    /// 柱高不符的根数。
    height_mismatches: usize,
    /// 本场景上屏次数。
    presented: u64,
    /// 逐柱实测高度。
    measured: Vec<u16>,
    /// 逐柱期望高度。
    expected: Vec<u16>,
    /// 峰值柱序号（供 JSON 阅读）。
    tone_hz: f32,
}

impl Scenario {
    fn to_json(&self) -> String {
        format!(
            r#"{{ "tone_hz": {tone}, "peak_hz": {peak}, "data_argmax": {da}, "measured_argmax": {ma}, "measured_peak_px": {mp}, "height_mismatches": {hm}, "presented": {pr}, "expected_heights_sample": {ex}, "measured_heights_sample": {me} }}"#,
            tone = self.tone_hz,
            peak = self
                .peak_hz
                .map_or("null".to_string(), |f| format!("{f:.1}")),
            da = self.data_argmax,
            ma = self.measured_argmax,
            mp = self.measured_peak_px,
            hm = self.height_mismatches,
            pr = self.presented,
            ex = json_u16(&self.expected),
            me = json_u16(&self.measured),
        )
    }
}

/// 静音场景结果。
struct SilenceResult {
    cleared: bool,
    stopped: bool,
}

impl SilenceResult {
    fn to_json(&self) -> String {
        format!(
            r#"{{ "cleared_on_edge": {}, "stopped_presenting": {} }}"#,
            self.cleared, self.stopped
        )
    }
}

/// 注入一个纯音，跑 `HOPS` 帧，然后上屏 + 回读 + 逐柱比对。
fn run_tone(
    pip: &mut Pipeline,
    overlay: &mut X11Overlay,
    tone_hz: f32,
    start_ms: u64,
) -> Result<Scenario, Box<dyn std::error::Error>> {
    let tone = sine_wave(tone_hz, RATE, HOP * 2);
    let mut presented = 0_u64;
    let mut last = None;
    for step in 0..HOPS {
        let offset = (step * HOP) % HOP; // 相位连续：同一段波形循环，避免爆音式跳变
        let now_ms = start_ms + (step as u64) * (u64::try_from(HOP)? * 1000 / u64::from(RATE));
        let out = pip.push(&tone[offset..offset + HOP], now_ms);
        if out.should_present {
            presented += 1;
        }
        last = Some(out);
    }
    let out = last.ok_or("没有产出任何帧")?;
    let stats = out.stats.ok_or("有声音却没有上屏")?;

    // 上屏 + 回读
    let surface = pip.surface();
    let (w, h) = (surface.width(), surface.height());
    let t0 = Instant::now();
    overlay.put_image(surface.pixels(), w, h)?;
    let put_us = t0.elapsed().as_micros();
    let readback = overlay.get_image(0, 0, w, h)?;

    // 逐柱比对：期望高度由 bands 直接算出（与渲染器同一套规则，此处独立重算）
    let style = pip.config().style;
    let usable = style.usable_height(h);
    let bottom = h.saturating_sub(style.bottom_margin_px);
    let count = u32::try_from(out.bands.len())?;
    let width = u32::from(w);
    let slot = width / count.max(1);

    let mut expected = Vec::with_capacity(out.bands.len());
    let mut measured = Vec::with_capacity(out.bands.len());
    let mut mismatches = 0_usize;
    for (index, &level) in out.bands.iter().enumerate() {
        let level = level.clamp(0.0, 1.0);
        let exp = if level <= 0.0 {
            0
        } else {
            ((level * f32::from(usable)).round() as u16)
                .max(style.min_bar_px.min(usable))
                .min(usable)
        };
        let sample_x = u32::try_from(index)? * width / count + slot / 2;
        let mut got = 0_u16;
        for y in (0..u32::from(bottom)).rev() {
            let pixel = readback[y as usize * usize::from(w) + sample_x as usize];
            if pixel >> 24 == 0 {
                break;
            }
            got += 1;
        }
        expected.push(exp);
        measured.push(got);
        if got.abs_diff(exp) > TOLERANCE_PX {
            mismatches += 1;
        }
    }

    let data_argmax = out
        .bands
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
        .map_or(0, |(index, _)| index);
    let (measured_argmax, measured_peak_px) = measured
        .iter()
        .enumerate()
        .max_by_key(|(_, h)| *h)
        .map_or((0, 0), |(index, &h)| (index, h));

    println!(
        "   渲染统计: bars={} max={}px drawn={}px put_image={}µs",
        stats.bars, stats.max_bar_px, stats.drawn_pixels, put_us
    );

    Ok(Scenario {
        peak_hz: out.peak_hz,
        data_argmax,
        measured_argmax,
        measured_peak_px,
        height_mismatches: mismatches,
        presented,
        measured,
        expected,
        tone_hz,
    })
}

/// 注入静音，验证「边沿清屏一次、之后停上屏」。
fn run_silence(
    pip: &mut Pipeline,
    overlay: &mut X11Overlay,
    start_ms: u64,
) -> Result<SilenceResult, Box<dyn std::error::Error>> {
    let silence = vec![0.0_f32; HOP];
    let step_ms = u64::try_from(HOP)? * 1000 / u64::from(RATE);
    let mut cleared = false;
    let mut stopped = false;

    for step in 0..40_u64 {
        let now_ms = start_ms + step * step_ms;
        let out = pip.push(&silence, now_ms);
        if !out.active {
            if !cleared {
                // 边沿：必须补一帧，且这一帧应当全透明
                if out.should_present {
                    let surface = pip.surface();
                    let (w, h) = (surface.width(), surface.height());
                    overlay.put_image(surface.pixels(), w, h)?;
                    let pixels = overlay.get_image(0, 0, w, h)?;
                    let lit = pixels.iter().filter(|&&p| p >> 24 != 0).count();
                    cleared = lit == 0;
                    if !cleared {
                        println!("   ⚠ 清屏帧仍有 {lit} 个不透明像素");
                    }
                }
            } else if !out.should_present {
                stopped = true;
                break;
            }
        }
    }
    Ok(SilenceResult { cleared, stopped })
}

/// 预览图：预乘 RGBA 原始缓冲 + meta。
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
    std::fs::write("docs/pipeline-preview.rgba", &rgba)?;
    std::fs::write(
        "docs/pipeline-preview.meta.json",
        format!(
            "{{\n  \"width\": {width},\n  \"height\": {height},\n  \"format\": \"rgba-premultiplied\"\n}}\n"
        ),
    )?;
    Ok(())
}

fn json_u16(values: &[u16]) -> String {
    let tail: Vec<String> = values.iter().take(12).map(ToString::to_string).collect();
    format!("[{}]", tail.join(", "))
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
    (if m <= 2 { y + 1 } else { y }, m, d)
}
