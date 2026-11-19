//! D-Bus 任务栏信号订阅实证探针（阶段 2 · D-Bus 订阅门）。
//!
//! 目的：用**真实的 `spectrum-platform::dock_geom` 订阅链路**证明
//! 「任务栏高度自适应」所需的事件源确实可用，并把实测结果落盘为证据。
//!
//! 覆盖的实证点（每条都必须由本探针自己测出来，不得照抄文档）：
//!
//! | 编号 | 断言 | 本探针如何测 |
//! |---|---|---|
//! | E1 | 双 Proxy 必要性 | `daemon` 接口读 `FrontendWindowRect` 成功；`dock` 接口读同一属性**必须失败** |
//! | E2 | 物理矩形可读 | `get_property::<(i32,i32,i32,i32)>("FrontendWindowRect")` |
//! | E3 | 逻辑矩形可读（另一接口） | `dock` 接口的 `geometry` |
//! | E4 | 信号流可建立 | `receive_all_signals` 成功 |
//! | E5 | 订阅端到端收到首值 | [`DockSubscription::try_recv`] 拿到 `RectChanged` |
//! | E6 | 16ms 节流合并生效 | 喂入突发变化，统计 `Commit` 次数 |
//! | E7 | 非法矩形被拒绝 | 提交宽高为 0 的矩形 → `Rejected`，高度不变 |
//! | E8 | `doctor` 字段齐备 | 校验 `height_runtime` / `rect_source` / `resize_count` 等
//!
//! 用法：
//! ```bash
//! cargo run --release -p spectrum-platform --features dbus --example dock_signal_probe
//! ```
//!
//! 输出：`docs/spike-dbus-signals.json`（每次运行覆盖，含 `generated_at`）。

// 实证探针而非库代码：单函数线性罗列 E1~E8 各项断言，拆函数反而割裂可读性；
// 时间戳换算沿用 Howard Hinnant 的 civil_from_days 算法，其变量名是既有约定。
#![allow(clippy::too_many_lines, clippy::many_single_char_names)]

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use spectrum_core::dock_signal::{DockEvent, DockOutcome, DockSignalBridge, RectSource};
use spectrum_core::geometry::Rect;
use spectrum_platform::dock_geom::{DAEMON_INTERFACE, DOCK_INTERFACE, DockSubscription};
use zbus::proxy::Proxy;

/// D-Bus 物理矩形。
type RawRect = (i32, i32, i32, i32);

/// 生成 ISO 时间戳（自实现，不引 chrono）。
fn timestamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = i64::try_from(secs / 86_400).unwrap_or(0);
    let rem = secs % 86_400;
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// JSON 字符串转义。
fn esc(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 可选 watch 模式：`-- --watch <秒数>`，持续监听并记录每一次高度变化。
    // 不带参数时跑 E1~E8 一次性实证。
    let args: Vec<String> = std::env::args().collect();
    if let Some(pos) = args.iter().position(|a| a == "--watch") {
        let secs: u64 = args.get(pos + 1).and_then(|s| s.parse().ok()).unwrap_or(30);
        // zbus 6 的 API 是 async 的；用自带的 `block_on` 驱动（无需 tokio）。
        return watch_mode(secs);
    }
    zbus::block_on(run())
}

/// 一次重建提交的记录（watch 模式用）。
struct CommitRecord {
    at_ms: u128,
    from: Option<u32>,
    to: u32,
    rect: Rect,
}

/// 持续监听模式：证明「高度变化瞬间信号真的送达」。
///
/// 与一次性探针的区别：它不只看启动时的首值，而是在 `secs` 秒内
/// 持续消费事件，把每一次 `Commit` 记录下来（含 from→to 高度与前后的时间差）。
/// 用户在监听期间拖动任务栏，就能拿到端到端的事件投递证据。
fn watch_mode(secs: u64) -> Result<(), Box<dyn std::error::Error>> {
    let started = Instant::now();
    let mut subscription = DockSubscription::start(RectSource::Physical)?;
    let mut bridge = DockSignalBridge::new(RectSource::Physical);

    // 首值：先吃进订阅线程推送的初始事件，得到基准高度。
    let initial_height = drain_initial(&mut subscription, &mut bridge);
    println!("listening {secs}s; initial height = {initial_height:?}");
    println!(">> 现在请拖动任务栏改变高度，本进程会把每次变化记录下来 <<");

    let mut events: Vec<(u128, String)> = Vec::new();
    let mut commits: Vec<CommitRecord> = Vec::new();
    let mut invalid = 0_u32;
    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut last_height = initial_height;

    while Instant::now() < deadline {
        match subscription.try_recv() {
            Ok(Some(event)) => {
                let at = started.elapsed().as_millis();
                events.push((at, describe(&event)));
                match bridge.submit(event, u64::try_from(at).unwrap_or(0)) {
                    DockOutcome::Commit(rect) => {
                        commits.push(CommitRecord {
                            at_ms: at,
                            from: last_height,
                            to: rect.height,
                            rect,
                        });
                        last_height = Some(rect.height);
                        println!(
                            "  [{:>6}ms] COMMIT  h {} -> {}",
                            at,
                            commits
                                .last()
                                .and_then(|c| c.from)
                                .map_or("-".to_string(), |h| h.to_string()),
                            rect.height
                        );
                    }
                    DockOutcome::Rejected => {
                        invalid += 1;
                        println!("  [{at:>6}ms] REJECTED（非法矩形，已保留上次有效值）");
                    }
                    _ => {}
                }
            }
            Ok(None) => {
                // 每帧 tick 以放行节流窗口内的合并提交。
                let at = u64::try_from(started.elapsed().as_millis()).unwrap_or(0);
                if let DockOutcome::Commit(rect) = bridge.tick(at) {
                    commits.push(CommitRecord {
                        at_ms: u128::from(at),
                        from: last_height,
                        to: rect.height,
                        rect,
                    });
                    last_height = Some(rect.height);
                    println!("  [{at:>6}ms] COMMIT(tick)  h -> {}", rect.height);
                }
                std::thread::sleep(Duration::from_millis(4));
            }
            Err(err) => {
                println!("订阅中断：{err}");
                break;
            }
        }
    }

    let doctor = bridge.doctor();
    let verdict = if commits.is_empty() {
        "NO_CHANGE_OBSERVED"
    } else {
        "PASS"
    };

    let json = format!(
        r#"{{
  "generated_at": "{ts}",
  "mode": "watch",
  "duration_secs": {secs},
  "initial_height": {init},
  "final_height": {fin},
  "observed_events": {nev},
  "commits": {ncommit},
  "invalid_geometry_count": {invalid},
  "resize_count": {resize},
  "event_log": [{evlog}],
  "commit_log": [{clog}],
  "verdict": "{verdict}"
}}
"#,
        ts = timestamp(),
        secs = secs,
        init = initial_height.map_or("null".to_string(), |h| h.to_string()),
        fin = doctor
            .height_runtime
            .map_or("null".to_string(), |h| h.to_string()),
        nev = events.len(),
        ncommit = commits.len(),
        invalid = invalid,
        resize = doctor.resize_count,
        evlog = events
            .iter()
            .map(|(t, d)| format!(r#"{{ "at_ms": {t}, "event": "{}" }}"#, esc(d)))
            .collect::<Vec<_>>()
            .join(", "),
        clog = commits
            .iter()
            .map(|c| format!(
                r#"{{ "at_ms": {}, "from": {}, "to": {}, "rect": {{ "x": {}, "y": {}, "w": {}, "h": {} }} }}"#,
                c.at_ms,
                c.from.map_or("null".to_string(), |h| h.to_string()),
                c.to,
                c.rect.x,
                c.rect.y,
                c.rect.width,
                c.rect.height
            ))
            .collect::<Vec<_>>()
            .join(", "),
        verdict = verdict,
    );
    std::fs::write("docs/spike-dbus-watch.json", &json)?;
    println!(
        "\nverdict = {verdict}  (commits={}, events={})",
        commits.len(),
        events.len()
    );
    println!("wrote docs/spike-dbus-watch.json");

    if commits.is_empty() {
        println!("提示：监听期间没有观察到高度变化。请在本进程运行时拖动任务栏。");
    }
    Ok(())
}

/// 吃掉订阅线程推送的初始事件，返回基准高度。
fn drain_initial(
    subscription: &mut DockSubscription,
    bridge: &mut DockSignalBridge,
) -> Option<u32> {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        match subscription.try_recv() {
            Ok(Some(event)) => {
                bridge.submit(event, 0);
            }
            Ok(None) => {
                if bridge.height().is_some() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => break,
        }
    }
    bridge.tick(16);
    bridge.height()
}

/// 把事件转成可读描述，供日志与 JSON 使用。
fn describe(event: &DockEvent) -> String {
    match event {
        DockEvent::RectChanged { height, .. } => format!("RectChanged(h={height})"),
        DockEvent::WindowSizeChanged(v) => format!("WindowSizeChanged({v})"),
        DockEvent::PositionChanged(v) => format!("PositionChanged({v})"),
        DockEvent::HideModeChanged(v) => format!("HideModeChanged({v})"),
        DockEvent::DisplayModeChanged(v) => format!("DisplayModeChanged({v})"),
        DockEvent::PluginsChanged(c) => format!("PluginsChanged({c})"),
    }
}

/// 是否已收到「矩形 + 图标数」两类首值。
fn has_rect_and_plugins(events: &[DockEvent]) -> bool {
    let rect = events
        .iter()
        .any(|e| matches!(e, DockEvent::RectChanged { .. }));
    let plugins = events
        .iter()
        .any(|e| matches!(e, DockEvent::PluginsChanged(_)));
    rect && plugins
}

/// 探针主体。
async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let started = Instant::now();
    let mut notes: Vec<String> = Vec::new();

    // ---------- E1/E2/E3：双 Proxy 语义 ----------
    let conn = zbus::Connection::session().await?;
    let daemon = Proxy::new(
        &conn,
        DAEMON_INTERFACE,
        spectrum_platform::dock_geom::DOCK_OBJECT_PATH,
        DAEMON_INTERFACE,
    )
    .await?;
    let dock = Proxy::new(
        &conn,
        DOCK_INTERFACE,
        spectrum_platform::dock_geom::DOCK_OBJECT_PATH,
        DOCK_INTERFACE,
    )
    .await?;

    let physical: RawRect = daemon.get_property("FrontendWindowRect").await?;
    let logical: RawRect = dock.get_property("geometry").await?;
    // 用错接口读物理矩形必须失败 —— 这是双 Proxy 必要性的直接证据。
    let wrong_interface_failed = dock
        .get_property::<RawRect>("FrontendWindowRect")
        .await
        .is_err();
    if !wrong_interface_failed {
        notes.push("E1 违反预期：用 dock 接口竟能读到 FrontendWindowRect".to_string());
    }
    println!("physical={physical:?} logical={logical:?} wrong_failed={wrong_interface_failed}");

    // ---------- E4/E5：真实订阅链路 ----------
    let mut subscription = DockSubscription::start(RectSource::Physical)?;
    let mut bridge = DockSignalBridge::new(RectSource::Physical);

    // 收集订阅线程推送的首批事件（矩形 + 图标数由不同线程分别给出）。
    // 等到「矩形 + 图标数」都到齐即可停止，不空等。
    let mut drain: Vec<DockEvent> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if let Some(event) = subscription.try_recv()? {
            drain.push(event);
        } else if has_rect_and_plugins(&drain) {
            break;
        } else {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    let first_event = drain.first().copied();
    let e5_ok = first_event.is_some();
    let drain_len = drain.len();
    println!(
        "subscribed events = {} first = {first_event:?}",
        drain.len()
    );

    // 把订阅到的事件喂进桥，观察提交。
    let mut commits = 0_u32;
    let mut pending = 0_u32;
    let mut rejected = 0_u32;
    let mut relayouts = 0_u32;
    for event in drain {
        match bridge.submit(event, 0) {
            DockOutcome::Pending => pending += 1,
            DockOutcome::Commit(_) => commits += 1,
            DockOutcome::Rejected => rejected += 1,
            DockOutcome::Relayout(_) => relayouts += 1,
            DockOutcome::NoChange | DockOutcome::NeedsRefresh => {}
        }
    }
    // 节流窗口后提交。
    if matches!(bridge.tick(16), DockOutcome::Commit(_)) {
        commits += 1;
    }

    // ---------- E8：真实订阅结果的 doctor（必须在合成测试之前取）----------
    // 合成测试会改写 bridge 的高度，若放在后面会让证据里的 height_runtime
    // 变成合成值（如 101）而非真实任务栏高度（78）——那是自欺欺人的证据。
    let real_doctor = bridge.doctor();
    let real_height = bridge.height();
    println!("real dock height = {real_height:?}");

    // ---------- E6：突发变化只提交一次（用独立 bridge，避免污染真实证据）----------
    let mut burst_bridge = DockSignalBridge::new(RectSource::Physical);
    burst_bridge.submit(
        DockEvent::RectChanged {
            x: 0,
            y: 1921,
            width: 3200,
            height: 78,
        },
        0,
    );
    burst_bridge.tick(16);
    let before_burst = burst_bridge.resize_count();
    for (i, h) in [79_u32, 82, 88, 95, 101].into_iter().enumerate() {
        let now = 1_000 + u64::try_from(i).unwrap_or(0) * 3;
        if burst_bridge.submit(
            DockEvent::RectChanged {
                x: 0,
                y: 2_000 - i32::try_from(h).unwrap_or(0),
                width: 3200,
                height: h,
            },
            now,
        ) == DockOutcome::Pending
        {
            pending += 1;
        }
    }
    if matches!(burst_bridge.tick(1_016), DockOutcome::Commit(_)) {
        commits += 1;
    }
    let burst_delta = burst_bridge.resize_count() - before_burst;

    // ---------- E7：非法矩形被拒绝（同样用独立 bridge）----------
    let height_before_invalid = burst_bridge.height();
    let invalid_before = burst_bridge.invalid_count();
    let invalid_rejected = burst_bridge.submit(
        DockEvent::RectChanged {
            x: 0,
            y: 0,
            width: 0,
            height: 78,
        },
        9_999,
    ) == DockOutcome::Rejected;
    let invalid_delta = burst_bridge.invalid_count() - invalid_before;
    let height_preserved = burst_bridge.height() == height_before_invalid;

    // ---------- E8：doctor 字段 ----------
    // `invalid_geometry_count` 属于合成测试（独立 bridge），真实订阅不应有非法值，
    // 因此这里只校验真实字段齐备，非法计数单独由 E7 断言。
    let doctor = &real_doctor;
    let doctor_ok = doctor.height_runtime.is_some()
        && !doctor.rect_source.is_empty()
        && doctor.icon_count.is_some()
        && doctor.invalid_geometry_count == 0;

    // ---------- 落盘 ----------
    let json = format!(
        r#"{{
  "generated_at": "{ts}",
  "bus": "session",
  "object_path": "{path}",
  "interfaces": {{ "physical": "{daemon}", "logical": "{dock}" }},
  "physical_rect": {{ "x": {px}, "y": {py}, "w": {pw}, "h": {ph} }},
  "logical_rect": {{ "x": {lx}, "y": {ly}, "w": {lw}, "h": {lh} }},
  "wrong_interface_read_failed": {wrong_failed},
  "subscription_started": true,
  "subscription_threads_alive": {alive},
  "first_event_received": {e5},
  "events_drained": {drained},
  "bridge_commits": {commits},
  "bridge_pending": {pending},
  "bridge_rejected": {rejected},
  "bridge_relayouts": {relayouts},
  "burst_resize_delta": {burst_delta},
  "invalid_rect_rejected": {invalid_rej},
  "invalid_count_delta": {invalid_delta},
  "height_preserved_after_invalid": {height_ok},
  "doctor": {{
    "height_runtime": {height},
    "rect_source": "{source}",
    "resize_count": {resize_count},
    "invalid_geometry_count": {invalid_count},
    "icon_count": {icon_count},
    "position": {position},
    "hide_mode": {hide_mode},
    "display_mode": {display_mode},
    "last_commit_ms": {last_commit}
  }},
  "doctor_fields_ok": {doctor_ok},
  "notes": [{notes}],
  "elapsed_ms": {elapsed}
}}
"#,
        ts = timestamp(),
        path = spectrum_platform::dock_geom::DOCK_OBJECT_PATH,
        daemon = DAEMON_INTERFACE,
        dock = DOCK_INTERFACE,
        px = physical.0,
        py = physical.1,
        pw = physical.2,
        ph = physical.3,
        lx = logical.0,
        ly = logical.1,
        lw = logical.2,
        lh = logical.3,
        wrong_failed = wrong_interface_failed,
        alive = subscription.is_alive(),
        e5 = e5_ok,
        drained = drain_len,
        commits = commits,
        pending = pending,
        rejected = rejected,
        relayouts = relayouts,
        burst_delta = burst_delta,
        invalid_rej = invalid_rejected,
        invalid_delta = invalid_delta,
        height_ok = height_preserved,
        height = doctor
            .height_runtime
            .map_or("null".to_string(), |h| h.to_string()),
        source = doctor.rect_source,
        resize_count = doctor.resize_count,
        invalid_count = doctor.invalid_geometry_count,
        icon_count = doctor
            .icon_count
            .map_or("null".to_string(), |c| c.to_string()),
        position = doctor
            .position
            .map_or("null".to_string(), |p| p.to_string()),
        hide_mode = doctor
            .hide_mode
            .map_or("null".to_string(), |h| h.to_string()),
        display_mode = doctor
            .display_mode
            .map_or("null".to_string(), |d| d.to_string()),
        last_commit = doctor
            .last_commit_ms
            .map_or("null".to_string(), |c| c.to_string()),
        doctor_ok = doctor_ok,
        notes = notes
            .iter()
            .map(|n| format!("\"{}\"", esc(n)))
            .collect::<Vec<_>>()
            .join(", "),
        elapsed = started.elapsed().as_millis(),
    );

    std::fs::write("docs/spike-dbus-signals.json", &json)?;
    println!("wrote docs/spike-dbus-signals.json");
    Ok(())
}
