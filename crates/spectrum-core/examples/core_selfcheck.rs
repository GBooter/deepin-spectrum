//! 阶段 1 端到端自检：用真实 Rust 二进制跑一遍纯逻辑链路并输出 JSON 证据。
//!
//! 用法：
//! ```text
//! cargo run --release -p spectrum-core --example core_selfcheck -- [f32le-raw-path]
//! ```
//! 不传路径时只跑合成信号；传入时使用真实 monitor 采样（只读，不播放任何声音）。

#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::float_cmp,
    clippy::uninlined_format_args
)]

use spectrum_core::analyzer::{Analyzer, AnalyzerConfig, build_edges, sine_wave};
use spectrum_core::capture::{FrameRing, SilenceGate, silence_linear};
use spectrum_core::config::Config;
use spectrum_core::geometry::{GeometryTracker, Rect};
use spectrum_core::state::{StateMachine, Visibility};
use std::time::Instant;

const RATE: u32 = 48_000;

fn read_f32le(path: &str) -> Vec<f32> {
    let Ok(bytes) = std::fs::read(path) else {
        return Vec::new();
    };
    bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .filter(|v| v.is_finite())
        .collect()
}

// 自检是一条线性的取证流程，拆成小函数反而让证据与断言分离。
#[allow(clippy::too_many_lines)]
fn main() {
    let raw_path = std::env::args().nth(1);
    let cfg = AnalyzerConfig::default();
    let edges = build_edges(&cfg, RATE);

    // ── 1. F56 柱数表（§6.2.2 全部 11 档）──────────────────────────
    let table: [(usize, f32, u32); 11] = [
        (2048, 30.0, 48_000),
        (2048, 40.0, 48_000),
        (2048, 40.0, 44_100),
        (2048, 80.0, 48_000),
        (4096, 30.0, 48_000),
        (4096, 40.0, 48_000),
        (4096, 40.0, 44_100),
        (4096, 80.0, 48_000),
        (8192, 30.0, 48_000),
        (8192, 40.0, 48_000),
        (8192, 80.0, 48_000),
    ];
    let mut edge_counts = Vec::new();
    for (fft, fmin, rate) in table {
        let c = AnalyzerConfig {
            fft_size: fft,
            f_min: fmin,
            ..cfg.clone()
        };
        edge_counts.push(build_edges(&c, rate).len());
    }
    let no_dup = edges.windows(2).all(|w| w[0] != w[1]);

    // ── 2. F22 低频三点落在不同 band ───────────────────────────────
    let f22_bands: Vec<usize> = [40.0, 60.0, 100.0]
        .iter()
        .map(|&f| {
            let bin = (f * cfg.fft_size as f32 / RATE as f32).round() as usize;
            edges
                .iter()
                .position(|&(lo, hi)| (lo..hi).contains(&bin))
                .unwrap_or(usize::MAX)
        })
        .collect();
    let f22_pass = f22_bands
        .iter()
        .copied()
        .collect::<std::collections::BTreeSet<_>>()
        .len()
        == 3;

    // ── 3. 合成 1kHz 正弦：峰值 band 是否覆盖目标 bin ──────────────
    let mut an = Analyzer::new(&cfg, RATE).expect("Analyzer 构造失败");
    let tone = sine_wave(1_000.0, RATE, cfg.fft_size);
    let levels = an.process(&tone, 0).to_vec();
    let peak_band = levels
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.total_cmp(b.1))
        .map_or(0, |(i, _)| i);
    let peak_level = levels[peak_band];

    // ── 4. F24 静音门限 + F54 ring 语义 ─────────────────────────────
    let lin = silence_linear(-62.0);
    let mut gate = SilenceGate::new(-62.0, 800);
    gate.feed(lin, 200); // 边界：等于门限不得刷新
    let f24_boundary_kept = gate.last_sound_ms() == 0;
    gate.feed(lin * 2.0, 300);
    let f24_refreshed = gate.last_sound_ms() == 300 && gate.active(1_000) && !gate.active(1_100);
    let mut ring = FrameRing::new(8);
    ring.push_slice(&[1.0, 2.0, 3.0, 4.0, 5.0]);
    ring.push_slice(&[6.0, 7.0, 8.0, 9.0, 10.0]);
    let mut out = [0.0f32; 4];
    let got = ring.consume_latest(4, &mut out);
    let f54_pass = got == 4 && out == [5.0, 6.0, 7.0, 8.0] && ring.overflow_count() == 1;

    // ── 5. 配置 / 状态机 / 几何（含用户调整任务栏高度）──────────────
    let mut config = Config::default();
    let clean = config.normalize().expect("配置迁移失败").is_empty();
    let mut sm = StateMachine::new(150, false);
    let idle_fps = sm.target_fps(90);
    sm.update(Visibility::all_visible(), 0);
    let running_fps = sm.target_fps(90);
    let mut tracker = GeometryTracker::default();
    let heights = [78_u32, 60, 105, 45];
    for (i, h) in heights.iter().enumerate() {
        let now = i as u64 * 100;
        tracker.submit(
            Rect::new(0, 2_000 - i32::try_from(*h).unwrap_or_default(), 3200, *h),
            now,
        );
        tracker.tick(now + 16);
    }
    tracker.submit(Rect::new(0, 0, 0, 78), 500);
    let geo_ok = matches!(tracker.height(), Some(45))
        && tracker.resize_count() == 4
        && tracker.invalid_count() == 1;

    // ── 6. 真实音乐采样（可选）─────────────────────────────────────
    let (real_json, real_ok) = if let Some(path) = raw_path.as_deref() {
        let pcm = read_f32le(path);
        if pcm.len() >= cfg.fft_size {
            let rms = (pcm.iter().map(|x| x * x).sum::<f32>() / pcm.len() as f32).sqrt();
            let mut real_an = Analyzer::new(&cfg, RATE).expect("Analyzer 构造失败");
            let blocks = pcm.len() / cfg.fft_size;
            let mut nonzero_sum = 0usize;
            let mut max_level: f32 = 0.0;
            for (i, chunk) in pcm.chunks_exact(cfg.fft_size).enumerate() {
                let out = real_an.process(chunk, i as u64 * 20);
                nonzero_sum += out.iter().filter(|&&v| v > 0.01).count();
                max_level = max_level.max(out.iter().copied().fold(0.0, f32::max));
            }
            let avg_nonzero = nonzero_sum as f32 / blocks as f32;
            (
                format!(
                    "{{\"source\": \"{path}\", \"frames\": {}, \"blocks\": {}, \"rms\": {:.6}, \"avg_nonzero_bands\": {:.2}, \"max_level\": {:.4}, \"verdict\": \"{}\"}}",
                    pcm.len(),
                    blocks,
                    rms,
                    avg_nonzero,
                    max_level,
                    if rms > 1.0e-4 && avg_nonzero > 0.0 {
                        "PASS"
                    } else {
                        "SKIP（采样为静音）"
                    }
                ),
                rms > 1.0e-4 && avg_nonzero > 0.0,
            )
        } else {
            ("null".to_owned(), false)
        }
    } else {
        ("null".to_owned(), false)
    };

    // ── 7. 性能：4096 点 FFT + 全链路每帧耗时（§8 CPU 预算）────────
    let mut perf_an = Analyzer::new(&cfg, RATE).expect("Analyzer 构造失败");
    let frames = 1_000;
    let start = Instant::now();
    for i in 0..frames {
        perf_an.process(&tone, i as u64 * 11);
    }
    let total_us = start.elapsed().as_secs_f64() * 1.0e6;
    let per_frame_us = total_us / f64::from(frames);

    let verdict = no_dup
        && edges.len() == 59
        && f22_pass
        && peak_level > 0.1
        && f24_boundary_kept
        && f24_refreshed
        && f54_pass
        && clean
        && idle_fps == 0
        && running_fps == 90
        && geo_ok;

    // ★ M22：逐行收集输出并真正落盘。旧版只 println（靠 shell 重定向写文件），
    //   调用方若传了输出路径却没重定向，会「rc=0、终端有输出、文件仍是旧的」——假 PASS。
    let mut report = String::new();
    macro_rules! emit {
        ($($arg:tt)*) => {{
            let line = format!($($arg)*);
            println!("{line}");
            report.push_str(&line);
            report.push('\n');
        }};
    }

    emit!("{{");
    emit!("  \"stage\": \"phase-1\",");
    // 必须从运行时读取，不能硬编码：启用 fft-realfft 后旧值会变成假数据（§1.4）。
    emit!("  \"fft_backend\": \"{}\",", an.fft_backend_name());
    emit!(
        "  \"zero_external_deps\": {},",
        !cfg!(feature = "fft-realfft")
    );
    emit!(
        "  \"analyzer\": {{\"fft_size\": {}, \"bands_requested\": {}, \"bands_actual\": {}, \"no_adjacent_duplicate_edges\": {}, \"edge_count_table\": {:?}, \"f22_bands\": {:?}, \"f22_pass\": {}, \"sine_1k_peak_band\": {}, \"sine_1k_peak_level\": {:.4}, \"sample_rate_96k_bands\": {}}},",
        cfg.fft_size,
        cfg.bands,
        edges.len(),
        no_dup,
        edge_counts,
        f22_bands,
        f22_pass,
        peak_band,
        peak_level,
        build_edges(&cfg, 96_000).len()
    );
    emit!(
        "  \"capture\": {{\"silence_linear\": {:.9}, \"f24_boundary_kept\": {}, \"f24_refreshed\": {}, \"f54_pass\": {}}},",
        lin,
        f24_boundary_kept,
        f24_refreshed,
        f54_pass
    );
    emit!(
        "  \"config_state_geometry\": {{\"default_config_clean\": {}, \"idle_fps\": {}, \"running_fps\": {}, \"heights_tested\": {:?}, \"geo_ok\": {}, \"resize_count\": {}, \"invalid_count\": {}}},",
        clean,
        idle_fps,
        running_fps,
        heights,
        geo_ok,
        tracker.resize_count(),
        tracker.invalid_count()
    );
    emit!("  \"real_audio\": {real_json},");
    emit!("  \"real_audio_pass\": {real_ok},");
    emit!(
        "  \"performance\": {{\"frames\": {frames}, \"total_ms\": {:.3}, \"per_frame_us\": {:.2}, \"budget_note\": \"§8: 90Hz 帧间隔 11.1ms；FFT 4096 应远小于此\"}},",
        total_us / 1_000.0,
        per_frame_us
    );
    emit!(
        "  \"verdict\": \"{}\"",
        if verdict { "PASS" } else { "FAIL" }
    );
    emit!("}}");

    // 第二个参数是可选的证据输出路径；写失败必须报错退出，绝不静默「成功」。
    if let Some(path) = std::env::args().nth(2) {
        if let Some(parent) = std::path::Path::new(&path).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(err) = std::fs::write(&path, &report) {
            eprintln!("错误：无法写入证据 {path}: {err}");
            std::process::exit(1);
        }
    }
    if !verdict {
        std::process::exit(1);
    }
}
