//! 叠加层跟随实证探针（阶段 2 · 平台后端）。
//!
//! 目的：把 `DockSignalBridge` 产出的 `Commit(Rect)` **真的变成窗口尺寸变化**，
//! 并量化「跟随延迟」与「几何一致性」——这正是 T-H1 / G0-f 的两条判据。
//!
//! ## 两种模式
//!
//! - `--synthetic`（默认）：自测，不需要用户操作。按预设高度序列反复改窗口尺寸，
//!   逐次测量「下发 → 服务端确认」的耗时与几何误差。用于验证 `set_geometry`
//!   与「尺寸变化后重设空输入区」是否真的工作。
//! - `--follow <秒>`：接 `DockSubscription` 跟随**真实**拖动。用户拖动任务栏，
//!   探针在每帧排空事件、提交 `Commit` 对应的几何，逐次记录延迟与误差。
//!
//! ## 可视化
//!
//! `--visible` 会把窗口填成 25% 红的半透明色，肉眼可确认叠加层真的盖在任务栏区域上。
//! 不加则窗口全透明（默认 `background_pixel=0`），不影响观感。
//!
//! ## 用法
//!
//! ```bash
//! # 自测（无需用户操作）
//! cargo run -p spectrum-platform --features dbus,x11 --example overlay_follow_probe
//! # 加上可视确认
//! cargo run ... -- --synthetic --visible
//! # 跟随真实拖动 30 秒
//! cargo run ... -- --follow 30 --visible
//! ```
//!
//! 输出：`docs/spike-overlay.json`（每次运行覆盖）。

// 实证探针而非库代码：单函数顺序罗列各段测量，拆碎反而割裂可读性。
#![allow(clippy::too_many_lines, clippy::cast_precision_loss)]

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use spectrum_core::dock_signal::{DockOutcome, DockSignalBridge, RectSource};
use spectrum_core::geometry::Rect;
use spectrum_platform::dock_geom::DockSubscription;
use spectrum_platform::overlay::{OverlayBackend, OverlayGeometry};
use spectrum_platform::x11::{OverlayWindowType, X11Overlay};

/// 90Hz 屏幕的 3 帧预算：T-H1 要求「3 帧内跟随」。
const THREE_FRAMES_MS: f64 = 33.3;

/// 几何一致性容差（T-H1 判据：±1px）。
const TOLERANCE_PX: u16 = 1;

/// 半透明红（预乘 alpha，25%）。
const VISIBLE_ARGB: u32 = 0x40FF_0000;

/// 一次几何下发的测量记录。
struct ApplyRecord {
    /// 目标几何。
    target: OverlayGeometry,
    /// 下发耗时（`configure_window` + `flush`）。
    apply_us: u128,
    /// 下发到服务端确认的总耗时（含 `get_geometry` 往返）。
    confirm_us: u128,
    /// 服务端实际几何与目标的差异（各字段中最大者，像素）。
    error_px: u16,
}

/// ISO 时间戳（自实现，不引 chrono）。
fn timestamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = i64::try_from(secs / 86_400).unwrap_or(0);
    let rem = secs % 86_400;
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let mut y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    if m <= 2 {
        y += 1;
    }
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

/// 取参数值，同时支持 `--flag value` 与 `--flag=value`。
///
/// # 为什么必须支持两种写法
///
/// 第一版只支持空格分隔，于是 `--input-region=full` 被**静默忽略**：
/// 反例模式没有生效，反例探针和正常探针跑出完全一样的结果，
/// 差点把「判据无法区分」误判成「判据可用」。
/// 参数被静默丢弃是假 PASS 的高发源头，故这里两种写法都接受，
/// 并在 [`reject_unknown_args`] 里让无法识别的参数**直接报错**。
fn arg_value(flag: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    if let Some(i) = args.iter().position(|a| a == flag) {
        return args.get(i + 1).cloned();
    }
    let prefix = format!("{flag}=");
    args.iter()
        .find_map(|a| a.strip_prefix(&prefix).map(ToString::to_string))
}

/// 校验没有未知参数。
///
/// 已知 flag 清单在这里集中维护；拼错或用了未实现的开关会**立即失败**，
/// 而不是悄悄退化成默认行为（那会让「反例验证」变成「重复一次正常路径」）。
fn reject_unknown_args() -> Result<(), String> {
    /// 带值的开关。
    const WITH_VALUE: &[&str] = &["--follow", "--hold", "--window-type", "--input-region"];
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if arg == "--synthetic" || arg == "--visible" {
            i += 1;
            continue;
        }
        if let Some(flag) = WITH_VALUE.iter().find(|f| *f == arg) {
            match args.get(i + 1) {
                Some(v) if !v.starts_with("--") => {
                    let _ = flag;
                    i += 2;
                    continue;
                }
                _ => return Err(format!("{arg} 缺少取值")),
            }
        }
        if WITH_VALUE.iter().any(|f| arg.starts_with(&format!("{f}="))) {
            i += 1;
            continue;
        }
        return Err(format!(
            "未知参数 {arg}（已知：--synthetic / --visible / --follow N / --hold N / \
             --window-type dock|normal|notification|osd / --input-region empty|full）"
        ));
    }
    Ok(())
}

/// 几何差异（各字段中最大者）。
fn max_delta(a: OverlayGeometry, b: OverlayGeometry) -> u16 {
    [
        a.x.abs_diff(b.x),
        a.y.abs_diff(b.y),
        a.width.abs_diff(b.width),
        a.height.abs_diff(b.height),
    ]
    .into_iter()
    .max()
    .unwrap_or(0)
}

/// 等待窗口真正变成目标尺寸的上限。
///
/// **为什么需要等**：受管窗口（`override_redirect=0`）的 `configure_window` 是
/// 发给 **WM** 的 ConfigureRequest，kwin 会在自己的事件循环里异步兑现。
/// 客户端刚发完就读 `get_geometry`，拿到的往往还是**旧尺寸** ——
/// 直接比较会得到"误差刚好等于上一次变化量"的假失败（本轮实测踩到）。
/// 因此必须轮询到真正生效，这才是 T-H1「3 帧内跟随」该测的口径。
const SETTLE_TIMEOUT: Duration = Duration::from_millis(200);

/// 轮询间隔：远小于一帧（11.1ms @90Hz），避免把测量精度吃掉。
const SETTLE_POLL: Duration = Duration::from_micros(200);

/// 应用一次几何并测量「下发 → 真正生效」。
fn apply_and_measure(overlay: &mut X11Overlay, target: OverlayGeometry) -> ApplyRecord {
    let t0 = Instant::now();
    if let Err(err) = overlay.set_geometry(target) {
        println!("  set_geometry 失败：{err}");
        return ApplyRecord {
            target,
            apply_us: 0,
            confirm_us: 0,
            error_px: u16::MAX,
        };
    }
    let apply_us = t0.elapsed().as_micros();

    // 从服务端回读实际几何：不信本地缓存，只信 X server。
    let mut actual = overlay.geometry();
    while t0.elapsed() < SETTLE_TIMEOUT {
        match actual {
            Ok(current) if max_delta(target, current) <= TOLERANCE_PX => break,
            _ => {
                std::thread::sleep(SETTLE_POLL);
                actual = overlay.geometry();
            }
        }
    }
    let confirm_us = t0.elapsed().as_micros();
    let error_px = match actual {
        Ok(current) => max_delta(target, current),
        Err(_) => u16::MAX,
    };

    ApplyRecord {
        target,
        apply_us,
        confirm_us,
        error_px,
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 先校验参数：拼错/未知开关立即失败，绝不静默退化成默认行为。
    reject_unknown_args()?;

    let visible = std::env::args().any(|a| a == "--visible");
    let follow_secs: Option<u64> = arg_value("--follow").and_then(|s| s.parse().ok());

    // 基准几何：从真实任务栏取，绝不写死（F38）。
    let base = read_dock_geometry()?;
    println!(
        "dock rect = {}x{} @ ({}, {})",
        base.width, base.height, base.x, base.y
    );

    let window_type = match arg_value("--window-type").as_deref() {
        Some("normal") => OverlayWindowType::Normal,
        Some("notification") => OverlayWindowType::Notification,
        Some("osd") => OverlayWindowType::OnScreenDisplay,
        Some("dock") | None => OverlayWindowType::Dock,
        Some(other) => return Err(format!("--window-type 取值非法：{other}").into()),
    };
    let mut overlay = X11Overlay::create_with(base, window_type)?;
    println!(
        "overlay window 0x{:x} created, screen {}x{}",
        overlay.window_id(),
        overlay.screen_size().0,
        overlay.screen_size().1
    );
    if visible {
        overlay.fill(VISIBLE_ARGB)?;
        println!("已填充 25% 红，肉眼可见窗口应精确覆盖任务栏区域");
    }

    // --hold N：只建窗并保持 N 秒，供外部工具核验窗口属性（F7/F28/F77）。
    if let Some(secs) = arg_value("--hold").and_then(|v| v.parse::<u64>().ok()) {
        // 反例模式：把输入区设成整窗，用来验证「穿透」断言确实会红。
        // 取值非法时直接报错 —— 静默忽略会让反例变成「又跑了一遍正常路径」。
        match arg_value("--input-region").as_deref() {
            Some("full") => {
                overlay.debug_set_full_input_region()?;
                println!("⚠ 反例模式：输入区已设为整窗（应当吃掉鼠标事件）");
            }
            Some("empty") | None => {}
            Some(other) => return Err(format!("--input-region 取值非法：{other}").into()),
        }
        println!(
            "hold 模式 {secs}s：窗口 0x{:x} 保持不动，可用 xprop/xwininfo 核验",
            overlay.window_id()
        );
        std::thread::sleep(Duration::from_secs(secs));
        overlay.destroy()?;
        println!("窗口已销毁");
        return Ok(());
    }

    let records = if let Some(secs) = follow_secs {
        println!("跟随模式 {secs}s：请拖动任务栏改变高度");
        follow(&mut overlay, secs, base)?
    } else {
        println!("自测模式：按预设高度序列改窗口尺寸（无需操作）");
        synthetic(&mut overlay, base)
    };

    // 收尾：把窗口恢复到真实任务栏几何，再销毁（Drop 也会兜底）。
    let _ = overlay.set_geometry(base);
    let input_reapplied = overlay.input_region_reapplied();
    overlay.destroy()?;

    write_report(
        base,
        &records,
        input_reapplied,
        follow_secs.is_some(),
        visible,
    )
}

/// 自测：按预设高度序列改尺寸，验证 `set_geometry` 与输入区重设。
fn synthetic(overlay: &mut X11Overlay, base: OverlayGeometry) -> Vec<ApplyRecord> {
    // 覆盖「变高 / 变矮 / 回到原值 / 大跨度」四类变化
    let deltas: [i32; 6] = [0, 20, -12, 60, -30, 0];
    let mut records = Vec::new();

    for delta in deltas {
        let height = i32::from(base.height) + delta;
        let height = u16::try_from(height.max(8)).unwrap_or(8);
        let y = base
            .y
            .saturating_add(i16::try_from(i32::from(base.height) - i32::from(height)).unwrap_or(0));
        let target = OverlayGeometry::new(base.x, y, base.width, height);

        let record = apply_and_measure(overlay, target);
        println!(
            "  h={:<4} apply={:>5}µs confirm={:>5}µs error={}px",
            target.height, record.apply_us, record.confirm_us, record.error_px
        );
        records.push(record);
        std::thread::sleep(Duration::from_millis(350));
    }
    records
}

/// 跟随：把 `DockSignalBridge` 的 `Commit` 变成窗口几何变化。
fn follow(
    overlay: &mut X11Overlay,
    secs: u64,
    base: OverlayGeometry,
) -> Result<Vec<ApplyRecord>, Box<dyn std::error::Error>> {
    let mut subscription = DockSubscription::start(RectSource::Physical)?;
    let mut bridge = DockSignalBridge::new(RectSource::Physical);
    let mut records = Vec::new();

    // 先吃掉订阅线程推送的初始事件，拿到当前真实高度。
    let drain_until = Instant::now() + Duration::from_secs(2);
    while Instant::now() < drain_until && bridge.height().is_none() {
        if let Ok(Some(event)) = subscription.try_recv() {
            bridge.submit(event, 0);
        } else {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    bridge.tick(16);
    println!("initial height = {:?}", bridge.height());

    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut events = 0_u32;
    let mut rejected = 0_u32;

    while Instant::now() < deadline {
        let now_ms = u64::try_from(
            deadline
                .saturating_duration_since(Instant::now())
                .as_millis(),
        )
        .unwrap_or(0);
        let _ = now_ms;

        match subscription.try_recv() {
            Ok(Some(event)) => {
                events += 1;
                let now = u64::try_from(
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map(|d| d.as_millis())
                        .unwrap_or(0),
                )
                .unwrap_or(0);
                match bridge.submit(event, now) {
                    DockOutcome::Commit(rect) => {
                        records.push(apply_rect(overlay, rect, base));
                    }
                    DockOutcome::Rejected => {
                        rejected += 1;
                        println!("  非法几何，已拒绝（窗口保持上一次有效尺寸）");
                    }
                    _ => {}
                }
            }
            Ok(None) => {
                // ★ 生产 daemon 应**每帧**调用 tick；这里用 4ms 近似 90Hz 的一帧。
                let now = u64::try_from(
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map(|d| d.as_millis())
                        .unwrap_or(0),
                )
                .unwrap_or(0);
                if let DockOutcome::Commit(rect) = bridge.tick(now) {
                    records.push(apply_rect(overlay, rect, base));
                }
                std::thread::sleep(Duration::from_millis(4));
            }
            Err(err) => {
                println!("订阅中断：{err}");
                break;
            }
        }
    }

    println!(
        "events={events} applies={} rejected={rejected}",
        records.len()
    );
    Ok(records)
}

/// 把 core 的 `Rect` 应用到窗口并测量（几何非法时计为错误，不下发）。
fn apply_rect(overlay: &mut X11Overlay, rect: Rect, base: OverlayGeometry) -> ApplyRecord {
    match OverlayGeometry::try_from_rect(rect) {
        Ok(target) => {
            let record = apply_and_measure(overlay, target);
            if record.error_px > TOLERANCE_PX {
                println!(
                    "  ⚠ 目标 h={} 误差 {}px（apply {}µs confirm {}µs）",
                    record.target.height, record.error_px, record.apply_us, record.confirm_us
                );
            }
            record
        }
        Err(err) => {
            println!("  Rect {rect:?} 无法转为几何：{err}");
            ApplyRecord {
                target: base,
                apply_us: 0,
                confirm_us: 0,
                error_px: u16::MAX,
            }
        }
    }
}

/// 从 D-Bus 读一次真实任务栏几何（同步，仅用于取基准值）。
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

/// 统计并落盘。
fn write_report(
    base: OverlayGeometry,
    records: &[ApplyRecord],
    input_reapplied: u64,
    follow_mode: bool,
    visible: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let applied: Vec<&ApplyRecord> = records.iter().filter(|r| r.error_px != u16::MAX).collect();
    let confirmed = applied
        .iter()
        .filter(|r| r.error_px <= TOLERANCE_PX)
        .count();

    // 「冷启动」单独计：第一次回读窗口几何包含连接/服务端预热（实测可达 40ms+），
    // 而它通常还是一次无操作的下发（窗口建出来就已经是目标尺寸）。
    // 混进「跟随延迟」会得出误导性结论，故单列出来、**不隐藏**。
    let cold_start_ms = applied
        .first()
        .map_or(0.0, |r| (r.confirm_us as f64) / 1000.0);
    // 稳态统计从第二次下发开始 —— 这才是「跟随拖动」该看的口径。
    let steady: Vec<&ApplyRecord> = applied.iter().skip(1).copied().collect();
    let steady_total = steady.len();
    let within_3_frames = steady
        .iter()
        .filter(|r| (r.confirm_us as f64) / 1000.0 <= THREE_FRAMES_MS)
        .count();

    let mut confirm_ms: Vec<f64> = if steady.is_empty() {
        applied
            .iter()
            .map(|r| (r.confirm_us as f64) / 1000.0)
            .collect()
    } else {
        steady
            .iter()
            .map(|r| (r.confirm_us as f64) / 1000.0)
            .collect()
    };
    confirm_ms.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median_ms = if confirm_ms.is_empty() {
        0.0
    } else {
        confirm_ms[confirm_ms.len() / 2]
    };
    let max_ms = confirm_ms.last().copied().unwrap_or(0.0);

    // 高度多样性：证明测的是「跟随拖动」，不是一次跳变。
    let heights: Vec<u16> = applied.iter().map(|r| r.target.height).collect();
    let height_min = heights.iter().copied().min().unwrap_or(0);
    let height_max = heights.iter().copied().max().unwrap_or(0);
    let mut unique_heights: Vec<u16> = heights.clone();
    unique_heights.sort_unstable();
    unique_heights.dedup();

    let max_err = applied.iter().map(|r| r.error_px).max().unwrap_or(0);
    let geometry_ok = !applied.is_empty() && max_err <= TOLERANCE_PX;
    let latency_ok = steady_total > 0 && within_3_frames == steady_total;

    let verdict = if applied.is_empty() {
        "NO_APPLY_OBSERVED"
    } else if geometry_ok && latency_ok {
        "PASS"
    } else {
        "FAIL"
    };

    let json = format!(
        r#"{{
  "generated_at": "{ts}",
  "mode": "{mode}",
  "visible": {visible},
  "dock_rect": {{ "x": {bx}, "y": {by}, "w": {bw}, "h": {bh} }},
  "applies": {n},
  "height_range": {{ "min": {hmin}, "max": {hmax}, "unique": {huniq} }},
  "confirmed_within_tolerance": {confirmed},
  "tolerance_px": {tol},
  "max_error_px": {max_err},
  "latency_ms": {{ "median": {median:.2}, "max": {max_ms:.2}, "cold_start": {cold:.2}, "three_frame_budget": {budget} }},
  "steady_state_applies": {steady_n},
  "within_three_frames": {within},
  "input_region_reapplied": {reapplied},
  "geometry_ok": {geo_ok},
  "latency_ok": {lat_ok},
  "verdict": "{verdict}"
}}
"#,
        ts = timestamp(),
        mode = if follow_mode { "follow" } else { "synthetic" },
        visible = visible,
        bx = base.x,
        by = base.y,
        bw = base.width,
        bh = base.height,
        n = applied.len(),
        hmin = height_min,
        hmax = height_max,
        huniq = unique_heights.len(),
        confirmed = confirmed,
        tol = TOLERANCE_PX,
        max_err = max_err,
        median = median_ms,
        max_ms = max_ms,
        cold = cold_start_ms,
        steady_n = steady_total,
        budget = THREE_FRAMES_MS,
        within = within_3_frames,
        reapplied = input_reapplied,
        geo_ok = geometry_ok,
        lat_ok = latency_ok,
        verdict = verdict,
    );

    println!("\n--- 汇总 ---");
    println!("  下发次数            : {}", applied.len());
    println!(
        "  高度范围            : {height_min} ~ {height_max}（{} 个不同值）",
        unique_heights.len()
    );
    println!(
        "  几何误差 ≤ {TOLERANCE_PX}px      : {confirmed}/{}",
        applied.len()
    );
    println!("  最大误差            : {max_err}px");
    println!("  冷启动（首次回读）  : {cold_start_ms:.2}ms");
    println!("  稳态延迟 中位/最大  : {median_ms:.2}ms / {max_ms:.2}ms");
    println!("  稳态 3 帧预算内     : {within_3_frames}/{steady_total}");
    println!("  输入区重设次数      : {input_reapplied}");
    println!("  verdict             : {verdict}");

    std::fs::write("docs/spike-overlay.json", &json)?;
    println!("\nwrote docs/spike-overlay.json");
    Ok(())
}
