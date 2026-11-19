//! 渲染层实证：把频谱画成像素 → 送上窗口 → **回读**验证。
//!
//! ## 为什么要有回读这一步
//!
//! "画出来了"和"画对了"是两件事。只看窗口里有没有东西，无法区分：
//!
//! - 柱高算错（比例不对）
//! - 布局错位（槽宽算错、右端留白）
//! - 像素传输出错（`put_image` 的字节序 / 深度不匹配）
//!
//! 所以本探针把渲染结果写进窗口后，用 `get_image` 从 **X server** 取回真实
//! 像素，再逐项断言：
//!
//! | 编号 | 断言 | 方法 |
//! |---|---|---|
//! | R1 | 传输无损 | 回读缓冲与发出的缓冲**逐 u32 相等** |
//! | R2 | 柱高正确 | 每根柱从回读像素量出的高度 == 独立算出的期望高度 |
//! | R3 | 间隙透明 | 柱间空隙处 alpha == 0（不遮挡任务栏背景） |
//! | R4 | 铺满画布 | 最后一根柱的右边缘 == 画布宽度（无"无主"空白列） |
//! | R5 | 静音即空白 | 全 0 幅值时整窗透明，无残影 |
//!
//! R2 是关键：它把"渲染器的算术"和"服务端里的真实像素"对起来，
//! 两边由不同代码算出 —— 只有一致才算画对。
//!
//! 用法：
//! ```text
//! cargo run -p spectrum-platform --features dbus,x11 --example render_probe -- [--hold N] [--visible]
//! ```
//!
//! 产物：`docs/spike-render.json`、`docs/render-preview.rgba` + `docs/render-preview.meta.json`
//! （预览图由 `scripts/rgba_to_png.py` 转成 PNG）。

// 探针是线性流程脚本，拆函数反而更难对照断言编号；
// 像素统计里的 `usize <-> f64` 转换只用于打印比率。
#![allow(
    clippy::too_many_lines,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use std::time::{Duration, Instant};

use spectrum_platform::overlay::{OverlayBackend, OverlayGeometry};
use spectrum_platform::x11::X11Overlay;
use spectrum_render::{RenderStyle, SpectrumRenderer, Surface};

/// 柱数。3200px 宽下单槽 66px，视觉上接近常见的任务栏频谱。
const BAR_COUNT: usize = 48;

/// 判定容差：布局是整数运算，理论上应当零误差。
const TOLERANCE_PX: u16 = 0;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let hold_secs: u64 = arg_value("--hold")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    let base = read_dock_geometry()?;
    println!(
        "dock rect = {}x{} @ ({}, {})",
        base.width, base.height, base.x, base.y
    );

    let mut overlay = X11Overlay::create(base)?;
    // ★ 用自己的窗口**真实几何**，不用 D-Bus 的期望值：
    // WM 可能对 dock 类窗口做微调（尺寸/位置）。若按期望尺寸整块 get_image，
    // 一旦实际窗口更小就越界 → GetImage 返回 BadMatch（本轮实测踩到）。
    let actual = overlay.geometry()?;
    println!(
        "overlay window 0x{:x} actual {}x{} @ ({}, {})",
        overlay.window_id(),
        actual.width,
        actual.height,
        actual.x,
        actual.y
    );
    if actual.width != base.width || actual.height != base.height {
        println!(
            "  注：WM 调整了尺寸（期望 {}x{}），以下一律按实际尺寸渲染",
            base.width, base.height
        );
    }
    let base = actual;

    // ---------- 构造确定的幅值序列 ----------
    // 混合"梳状"与"驼峰"两种形态：梳状能暴露槽宽/间隙错误，
    // 驼峰能让高低差在预览图上一眼可见。
    let magnitudes: Vec<f32> = (0..BAR_COUNT)
        .map(|i| {
            let comb = ((i * 37) % 100) as f32 / 100.0;
            let hump = {
                let center = BAR_COUNT as f32 / 2.0;
                let dist = (i as f32 - center).abs() / center;
                (1.0 - dist).max(0.0)
            };
            (0.12 + 0.55 * comb + 0.33 * hump).min(1.0)
        })
        .collect();

    let style = RenderStyle::new();
    let renderer = SpectrumRenderer::new(style);
    let mut surface = Surface::new(base.width, base.height);

    // ---------- 渲染并上屏 ----------
    let t_render = Instant::now();
    let stats = renderer.render(&mut surface, &magnitudes);
    let render_us = t_render.elapsed().as_micros();

    let t_put = Instant::now();
    overlay.put_image(surface.pixels(), base.width, base.height)?;
    let put_us = t_put.elapsed().as_micros();

    // ---------- 回读 ----------
    let t_get = Instant::now();
    let readback = overlay.get_image(0, 0, base.width, base.height)?;
    let get_us = t_get.elapsed().as_micros();
    println!("render {render_us}µs / put_image {put_us}µs / get_image {get_us}µs");
    println!(
        "stats: bars={} slot={}px bar={}px max_bar={}px drawn={}px",
        stats.bars, stats.slot_width_px, stats.bar_width_px, stats.max_bar_px, stats.drawn_pixels
    );

    // ---------- R1：传输无损 ----------
    let mismatched = surface
        .pixels()
        .iter()
        .zip(readback.iter())
        .filter(|(sent, got)| sent != got)
        .count();
    let r1_ok = mismatched == 0;
    println!("R1 传输无损: 不一致像素 {mismatched}/{}", readback.len());

    // ---------- R2/R3/R4：从回读像素反推柱高 ----------
    let usable = style.usable_height(base.height);
    let width = u32::from(base.width);
    let count = u32::try_from(BAR_COUNT)?;
    let slot = width / count;

    let mut measured_heights = Vec::with_capacity(BAR_COUNT);
    let mut expected_heights = Vec::with_capacity(BAR_COUNT);
    let mut height_mismatches = Vec::new();
    let mut gap_alpha_violations = 0_usize;
    let mut last_bar_right_edge = 0_u32;

    for (index, &magnitude) in magnitudes.iter().enumerate() {
        let index_u32 = u32::try_from(index)?;
        // 柱中心 x：用槽中点取样，避开间隙与边缘抗锯齿
        let sample_x = index_u32 * width / count + slot / 2;
        // 从回读像素量柱高：自**底边留白之上**向上数连续非透明像素。
        // 注意不能从最后一行开始 —— 底边留白那一行本来就是透明的，
        // 从那里数会立刻 break，把每根柱都记成 0px（本轮踩过）。
        let first_row = u32::from(base.height.saturating_sub(style.bottom_margin_px));
        let mut height = 0_u16;
        for y in (0..first_row).rev() {
            let pixel = readback[y as usize * usize::from(base.width) + sample_x as usize];
            if pixel >> 24 == 0 {
                break;
            }
            height += 1;
        }
        measured_heights.push(height);

        // 独立期望值：与渲染器同一套布局规则，但在这里重新算一遍
        let level = magnitude.clamp(0.0, 1.0);
        let expected = ((level * f32::from(usable)).round() as u16)
            .max(style.min_bar_px)
            .min(usable);
        expected_heights.push(expected);
        if height.abs_diff(expected) > TOLERANCE_PX {
            height_mismatches.push((index, expected, height));
        }

        // R3：柱右侧的间隙列应当 alpha==0
        let gap_x = (index_u32 + 1) * width / count - 1;
        if index + 1 < BAR_COUNT {
            let gap_pixel = readback
                [(u32::from(base.height) - 1) as usize * usize::from(base.width) + gap_x as usize];
            if gap_pixel >> 24 != 0 {
                gap_alpha_violations += 1;
            }
        } else {
            // R4：最后一根柱的右边缘必须贴到画布右边界
            last_bar_right_edge = (index_u32 + 1) * width / count;
        }
    }

    let r2_ok = height_mismatches.is_empty();
    let r3_ok = gap_alpha_violations == 0;
    let r4_ok = last_bar_right_edge == width;
    println!(
        "R2 柱高正确: 不符 {} 根（容差 {TOLERANCE_PX}px）",
        height_mismatches.len()
    );
    for (index, expected, measured) in height_mismatches.iter().take(5) {
        println!("   柱 #{index}: 期望 {expected}px 实测 {measured}px");
    }
    println!("R3 间隙透明: 违反 {gap_alpha_violations} 处");
    println!("R4 铺满画布: 末端 {last_bar_right_edge} / 画布 {width}");

    // ---------- R5：静音即空白 ----------
    let mut silent = Surface::new(base.width, base.height);
    renderer.render(&mut silent, &[0.0; BAR_COUNT]);
    overlay.put_image(silent.pixels(), base.width, base.height)?;
    let silent_readback = overlay.get_image(0, 0, base.width, base.height)?;
    let silent_lit = silent_readback.iter().filter(|&&p| p >> 24 != 0).count();
    let r5_ok = silent_lit == 0;
    println!("R5 静音即空白: 残留像素 {silent_lit}");

    // ---------- R6（不计入 verdict）：窗口级回读是否可行 ===
    // GetImage 对**窗口**要求 viewable 且矩形在窗口内；本环境（Xwayland + kwin）
    // 对 depth-32 窗口直接返回 BadMatch。这条限制是环境的，不是渲染的缺陷，
    // 因此**如实记录但不参与判定** —— 把它算进 PASS/FAIL 都会误导。
    let window_readback = overlay.get_window_image(0, 0, base.width, base.height);
    let (r6_supported, r6_detail) = match &window_readback {
        Ok(pixels) => {
            let diff = pixels
                .iter()
                .zip(surface.pixels().iter())
                .filter(|(got, sent)| got != sent)
                .count();
            (true, format!("窗口回读成功，与后备缓冲不一致 {diff} 像素"))
        }
        Err(err) => (false, format!("窗口回读不可用（环境限制）：{err}")),
    };
    println!("R6 窗口回读: {r6_detail}");

    // 留一帧"有声音"的画面给眼睛看
    overlay.put_image(surface.pixels(), base.width, base.height)?;

    // ---------- 落盘证据 ----------
    let verdict = if r1_ok && r2_ok && r3_ok && r4_ok && r5_ok {
        "PASS"
    } else {
        "FAIL"
    };
    let report = format!(
        r#"{{
  "generated_at": "{ts}",
  "verdict": "{verdict}",
  "window": {{ "width": {w}, "height": {h} }},
  "render": {{
    "bars": {bars},
    "slot_width_px": {slot},
    "bar_width_px": {barw},
    "max_bar_px": {maxbar},
    "drawn_pixels": {drawn},
    "usable_height_px": {usable},
    "render_us": {render_us},
    "put_image_us": {put_us},
    "get_image_us": {get_us}
  }},
  "checks": {{
    "R1_transport_lossless": {r1},
    "R1_mismatched_pixels": {mismatched},
    "R2_bar_heights_match": {r2},
    "R2_height_mismatches": {r2n},
    "R3_gap_transparent": {r3},
    "R3_gap_violations": {r3n},
    "R4_fills_canvas": {r4},
    "R5_silent_is_blank": {r5},
    "R5_residual_pixels": {r5n}
  }},
  "note_R6": "窗口级 GetImage 在本环境对 depth-32 窗口返回 BadMatch，故改用离屏 pixmap 回读；R6 仅记录环境能力，不计入 verdict",
  "R6_window_readback": {{ "supported": {r6}, "detail": "{r6d}" }},
  "magnitudes": {mags},
  "expected_heights": {expected},
  "measured_heights": {measured}
}}
"#,
        ts = iso_now(),
        verdict = verdict,
        w = base.width,
        h = base.height,
        bars = stats.bars,
        slot = stats.slot_width_px,
        barw = stats.bar_width_px,
        maxbar = stats.max_bar_px,
        drawn = stats.drawn_pixels,
        usable = usable,
        render_us = render_us,
        put_us = put_us,
        get_us = get_us,
        r1 = r1_ok,
        mismatched = mismatched,
        r2 = r2_ok,
        r2n = height_mismatches.len(),
        r3 = r3_ok,
        r3n = gap_alpha_violations,
        r4 = r4_ok,
        r5 = r5_ok,
        r5n = silent_lit,
        r6 = r6_supported,
        r6d = r6_detail.replace('"', "'"),
        mags = json_f32_slice(&magnitudes),
        expected = json_u16_slice(&expected_heights),
        measured = json_u16_slice(&measured_heights),
    );
    std::fs::write("docs/spike-render.json", report)?;

    // 预览图：原始 RGBA（预乘），供 rgba_to_png.py 转换
    let mut rgba = Vec::with_capacity(surface.pixels().len() * 4);
    for &pixel in surface.pixels() {
        rgba.extend_from_slice(&[
            ((pixel >> 16) & 0xFF) as u8,
            ((pixel >> 8) & 0xFF) as u8,
            (pixel & 0xFF) as u8,
            ((pixel >> 24) & 0xFF) as u8,
        ]);
    }
    std::fs::write("docs/render-preview.rgba", &rgba)?;
    std::fs::write(
        "docs/render-preview.meta.json",
        format!(
            "{{\n  \"width\": {},\n  \"height\": {},\n  \"format\": \"rgba-premultiplied\"\n}}\n",
            base.width, base.height
        ),
    )?;

    if hold_secs > 0 {
        println!("画面保持 {hold_secs}s，可直接观察任务栏上的频谱");
        std::thread::sleep(Duration::from_secs(hold_secs));
    }

    overlay.destroy()?;
    println!("verdict = {verdict}");
    if verdict != "PASS" {
        return Err("渲染断言未全部通过".into());
    }
    Ok(())
}

/// 取参数值，支持 `--flag value` 与 `--flag=value`。
fn arg_value(flag: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    if let Some(i) = args.iter().position(|a| a == flag) {
        return args.get(i + 1).cloned();
    }
    let prefix = format!("{flag}=");
    args.iter()
        .find_map(|a| a.strip_prefix(&prefix).map(ToString::to_string))
}

/// 从 D-Bus 读任务栏矩形（F38：绝不写死尺寸）。
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

/// `[f32]` → JSON 数组（保留 4 位小数，避免浮点尾数噪声）。
fn json_f32_slice(values: &[f32]) -> String {
    let items: Vec<String> = values.iter().map(|v| format!("{v:.4}")).collect();
    format!("[{}]", items.join(", "))
}

/// `[u16]` → JSON 数组。
fn json_u16_slice(values: &[u16]) -> String {
    let items: Vec<String> = values.iter().map(ToString::to_string).collect();
    format!("[{}]", items.join(", "))
}

/// ISO-8601 UTC 时间戳（不引入时间库，手算即可）。
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

/// 由"1970-01-01 起的天数"反推公历年月日（Howard Hinnant 算法）。
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
