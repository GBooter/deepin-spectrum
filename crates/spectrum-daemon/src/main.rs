//! `deepin-spectrum` 主程序：daemon / doctor 两个可用子命令（F84）。
//!
//! ## 子命令
//!
//! - `doctor`：打印环境与编译期能力，不需要声卡与 X server，任何机器都能跑
//! - `daemon`：主循环。默认用合成纯音（`--synthetic`），
//!   `--live` 用真实 `PulseAudio` monitor（需 `pulse` feature）
//!
//! ## 主循环的时序（为什么这样排）
//!
//! 每一轮固定做四件事，**顺序不能换**：
//!
//! 1. 读一整个 hop 的 PCM（默认 1024 帧 ≈ 21.3ms @48kHz）
//! 2. 推进流水线（静音门限 → 分析 → 渲染）
//! 3. 只有 `should_present` 时才 `put_image`（静音期 0 帧，F34）
//! 4. 处理任务栏几何变化（非阻塞 `try_recv`，不阻塞渲染节奏）
//!
//! 第 4 步放在渲染**之后**是刻意的：几何变化最多晚一帧（约 21ms）生效，
//! 而若放在前面，一次 `configure_window` 往返会顶掉这一帧的音频，
//! 拖动任务栏时反而更容易看到"卡一下"。
//!
//! ## 安全边界
//!
//! 只读 monitor、只建自己的叠加窗；不注册 systemd、不改默认输出、
//! 不改任务栏任何属性（F7：绝不设 strut）。

use std::io::Write as _;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
#[cfg(feature = "wayland")]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(feature = "wayland")]
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use spectrum_core::config::VisualMode;
use spectrum_daemon::DAEMON_STAGE;
use spectrum_daemon::FrameOutcome;
use spectrum_daemon::instance::ControlState;
use spectrum_daemon::pipeline::{Pipeline, PipelineConfig};
use spectrum_daemon::reconnect::{ReconnectPolicy, Reconnector, RetryDecision};
use spectrum_platform::overlay::OverlayGeometry;
#[cfg(feature = "wayland")]
use spectrum_platform::wayland::WaylandOverlay;
use spectrum_render::RenderStyle;
use spectrum_settings::apply;
use spectrum_settings::store::{ConfigStore, Reload};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("doctor") => doctor(&args[1..]),
        Some("settings") => run_settings(&args[1..]),
        Some("icon") => match icon_cmd(&args[1..]) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("deepin-spectrum icon: {err}");
                ExitCode::FAILURE
            }
        },
        #[cfg(feature = "tray")]
        Some("tray") => match spectrum_daemon::tray::run() {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("deepin-spectrum tray: {err}");
                ExitCode::FAILURE
            }
        },
        #[cfg(not(feature = "tray"))]
        Some("tray") => {
            eprintln!("deepin-spectrum: 本次构建未启用 tray feature");
            ExitCode::FAILURE
        }
        Some("daemon") => match run_daemon(&args[1..]) {
            Ok(()) => ExitCode::SUCCESS,
            Err(DaemonError::AlreadyRunning(msg)) => {
                eprintln!("deepin-spectrum: {msg}");
                ExitCode::from(EXIT_ALREADY_RUNNING)
            }
            Err(DaemonError::Failed(msg)) => {
                eprintln!("deepin-spectrum: {msg}");
                ExitCode::FAILURE
            }
        },
        Some("--help" | "-h" | "help") | None => {
            print_help();
            ExitCode::SUCCESS
        }
        Some(other) => {
            eprintln!("deepin-spectrum: 未知子命令 {other}");
            print_help();
            ExitCode::FAILURE
        }
    }
}

/// `icon` 子命令：导出应用图标（原始 RGBA 字节）。
///
/// 用法：`deepin-spectrum icon [尺寸] > out.rgba`
/// 再用 `scripts/rgba_to_png.py` 转成 PNG 供打包使用。
fn icon_cmd(args: &[String]) -> Result<(), String> {
    let size = match args.first() {
        None => 256,
        Some(value) => value
            .parse::<i32>()
            .map_err(|_| format!("尺寸非法：{value}"))?
            .clamp(8, 1024),
    };
    let (width, height, rgba) = spectrum_daemon::icon::icon_rgba(size);
    eprintln!("图标 {width}x{height}（{} 字节 RGBA）", rgba.len());
    let mut stdout = std::io::stdout().lock();
    stdout
        .write_all(&rgba)
        .map_err(|e| format!("写标准输出失败：{e}"))?;
    stdout.flush().map_err(|e| format!("刷新失败：{e}"))
}

/// `settings` 子命令。
///
/// - 不带参数 → 尝试拉起图形界面（`deepin-spectrum-settings`）；
///   找不到该二进制时退回命令行打印（headless 机器上不至于"点了没反应"）。
/// - `--gui` → 强制打开图形界面。
/// - `--print/--check/--explain/...` → 命令行形态。
fn run_settings(args: &[String]) -> ExitCode {
    let wants_gui = args.is_empty() || args.first().map(String::as_str) == Some("--gui");
    if wants_gui {
        match spawn_settings_gui(args) {
            Some(Ok(())) => return ExitCode::SUCCESS,
            Some(Err(err)) => {
                eprintln!("deepin-spectrum settings: {err}");
                return ExitCode::FAILURE;
            }
            None => {
                if args.first().map(String::as_str) == Some("--gui") {
                    eprintln!(
                        "deepin-spectrum settings: 找不到设置界面程序 deepin-spectrum-settings"
                    );
                    return ExitCode::FAILURE;
                }
                // 没装图形界面：退回命令行打印
            }
        }
    }
    let cli_args: Vec<String> = args
        .iter()
        .filter(|a| a.as_str() != "--gui")
        .cloned()
        .collect();
    match spectrum_settings::cli::run(&cli_args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("deepin-spectrum settings: {err}");
            ExitCode::FAILURE
        }
    }
}

/// 拉起设置界面二进制。
///
/// 界面是**独立进程**（独立 crate、独立二进制），因此这里只负责找它并 exec。
/// 找不到时返回 `None`（调用方决定是报错还是退回命令行）。
fn spawn_settings_gui(args: &[String]) -> Option<Result<(), String>> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    let candidates = [
        dir.join("deepin-spectrum-settings"),
        PathBuf::from("/usr/bin/deepin-spectrum-settings"),
        PathBuf::from("/usr/local/bin/deepin-spectrum-settings"),
    ];
    let found = candidates.iter().find(|p| p.is_file())?;
    let mut cmd = std::process::Command::new(found);
    // 把 --config 透传给界面（其余参数界面不认）
    let mut index = 0;
    while index < args.len() {
        if args[index] == "--config"
            && let Some(value) = args.get(index + 1)
        {
            cmd.arg("--config").arg(value);
            index += 2;
        } else {
            index += 1;
        }
    }
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => return Some(Err(format!("拉起设置界面失败：{err}"))),
    };
    // GUI 是独立进程；给一个短暂的启动窗口，若它立刻以非零码退出
    // （没有显示服务器 / 缺少库），调用方应看到错误而不是“成功”。
    let deadline = Instant::now() + Duration::from_millis(300);
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Some(Ok(())),
            Ok(Some(status)) => {
                return Some(Err(format!("设置界面启动后立即退出（{status}）")));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(err) => return Some(Err(format!("等待设置界面启动失败：{err}"))),
        }
    }
    Some(Ok(()))
}

fn print_help() {
    println!(
        "deepin-spectrum —— Deepin 任务栏音频频谱\n\
         \n\
         用法：\n\
         \x20 deepin-spectrum doctor [--json]          打印环境与能力\n\
         \x20 deepin-spectrum daemon [选项]               运行主循环\n\
         \x20 deepin-spectrum tray                       托盘（停止/重启频谱）\n\
         \x20 deepin-spectrum settings [动作]             参数模型与热重载（无动作时打开图形界面）\n\n\
         \n\
         daemon 选项：\n\
         \x20 --synthetic <Hz>   用合成纯音代替声卡（默认 440）\n\
         \x20 --live             用真实 PulseAudio monitor（需 pulse feature）\n\
         \x20 --duration <秒>    运行 N 秒后退出（默认一直跑）\n\
         \x20 --hop <帧数>       每个 hop 的样本数（默认 1024）\n\
         \x20 --config <文件>    指定配置文件（默认 ~/.config/deepin-spectrum/config.toml）\n\n\
         \n\
         settings 动作：\n\
         \x20 --print            打印当前生效的配置\n\
         \x20 --path             打印配置文件路径\n\
         \x20 --write-default    写出带注释的默认模板\n\
         \x20 --check            校验配置文件\n\
         \x20 --explain          列出每个键的生效范围\n\
         \x20 --gui              打开图形界面\n\
         \x20 --config <文件>    指定配置文件（命令行动作与界面都适用）\n\
         \x20                    只给 --config 而没有动作时按 --print 处理，不打开界面\n"
    );
}

/// 打印能力清单。不碰声卡与 X server。
///
/// `--json` 输出机器可读版本，供打包校验与现场排障脚本使用。
#[allow(clippy::too_many_lines, clippy::format_push_string)]
fn doctor(args: &[String]) -> ExitCode {
    let json = args.iter().any(|a| a == "--json");
    if let Some(other) = args.iter().find(|a| a.as_str() != "--json") {
        eprintln!("deepin-spectrum doctor: 未知参数 {other}");
        return ExitCode::FAILURE;
    }

    // 配置与热重载：路径、是否存在、能否热生效
    let store = ConfigStore::open(spectrum_settings::store::default_path());
    let hot = spectrum_settings::schema::KEYS
        .iter()
        .filter(|k| {
            matches!(
                k.scope,
                spectrum_settings::schema::Scope::Live
                    | spectrum_settings::schema::Scope::LiveRebuild
            )
        })
        .count();
    let config_warnings: Vec<String> = store.warnings().iter().map(ToString::to_string).collect();

    let pipeline = Pipeline::new(PipelineConfig::default());
    let (self_check, fft_backend, bands, sample_rate) = match &pipeline {
        Ok(pipeline) => (
            "OK".to_owned(),
            pipeline.fft_backend_name().to_owned(),
            pipeline.band_count(),
            pipeline.config().sample_rate,
        ),
        Err(err) => (format!("FAIL（{err}）"), "n/a".to_owned(), 0, 0),
    };

    // 运行期任务栏矩形（只读一次属性，不建窗、不订阅）。doctor 显式执行，允许阻塞。
    #[cfg(feature = "dbus")]
    let dock = doctor_probe_dock();
    #[cfg(not(feature = "dbus"))]
    let dock: Option<DockProbe> = None;

    #[cfg(feature = "wayland")]
    let session = if is_wayland_session() {
        "wayland"
    } else {
        "x11"
    };
    #[cfg(not(feature = "wayland"))]
    let session = if cfg!(feature = "x11") {
        "x11"
    } else {
        "headless"
    };

    let ok = self_check == "OK";

    if json {
        let mut out = String::new();
        out.push_str("{\n");
        out.push_str(&format!(
            "  \"stage\": \"{}\",\n",
            json_escape(DAEMON_STAGE)
        ));
        out.push_str(&format!(
            "  \"core_stage\": \"{}\",\n",
            json_escape(spectrum_core::CORE_STAGE)
        ));
        out.push_str("  \"renderer\": \"software/shm\",\n");
        out.push_str(&format!("  \"session\": \"{session}\",\n"));
        out.push_str(&format!(
            "  \"features\": {{\"pulse\": {}, \"x11\": {}, \"wayland\": {}, \"dbus\": {}, \"tray\": {}}},\n",
            cfg!(feature = "pulse"),
            cfg!(feature = "x11"),
            cfg!(feature = "wayland"),
            cfg!(feature = "dbus"),
            cfg!(feature = "tray")
        ));
        out.push_str(&format!(
            "  \"fft_backend\": \"{}\",\n",
            json_escape(&fft_backend)
        ));
        out.push_str(&format!("  \"sample_rate\": {sample_rate},\n"));
        out.push_str(&format!("  \"bands\": {bands},\n"));
        out.push_str(&format!(
            "  \"config\": {{\"path\": \"{}\", \"source\": \"{}\", \"hot_keys\": {hot}, \"warnings\": [",
            json_escape(&store.path().display().to_string()),
            json_escape(&store.source().to_string())
        ));
        for (index, warning) in config_warnings.iter().enumerate() {
            if index > 0 {
                out.push_str(", ");
            }
            out.push_str(&format!("\"{}\"", json_escape(warning)));
        }
        out.push_str("]},\n");
        match &dock {
            Some(probe) => {
                let d = &probe.doctor;
                let position = d
                    .position
                    .map_or_else(|| "null".to_owned(), |p| p.to_string());
                let rect = probe.rect.map_or_else(
                    || "null".to_owned(),
                    |(x, y, w, h)| format!("[{x}, {y}, {w}, {h}]"),
                );
                let height = d
                    .height_runtime
                    .map_or_else(|| "null".to_owned(), |h| h.to_string());
                let icons = d
                    .icon_count
                    .map_or_else(|| "null".to_owned(), |c| c.to_string());
                let commit = d
                    .last_commit_ms
                    .map_or_else(|| "null".to_owned(), |m| m.to_string());
                out.push_str(&format!(
                    "  \"dock\": {{\"position\": {position}, \"rect\": {rect}, \"height_runtime\": {height}, \"rect_source\": \"{}\", \"resize_count\": {}, \"invalid_geometry_count\": {}, \"icon_count\": {icons}, \"last_resize_ms\": {commit}}},\n",
                    json_escape(d.rect_source),
                    d.resize_count,
                    d.invalid_geometry_count
                ));
            }
            None => out.push_str("  \"dock\": null,\n"),
        }
        out.push_str(&format!(
            "  \"self_check\": \"{}\"\n}}",
            json_escape(&self_check)
        ));
        println!("{out}");
        return if ok {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        };
    }

    println!("deepin-spectrum doctor");
    println!("  stage            : {DAEMON_STAGE}");
    println!("  core stage       : {}", spectrum_core::CORE_STAGE);
    println!("  render engine    : software (premultiplied ARGB → put_image)");
    #[cfg(feature = "pulse")]
    println!("  capture          : pulse（已编译）");
    #[cfg(not(feature = "pulse"))]
    println!("  capture          : 未编译（--features pulse）");
    #[cfg(feature = "x11")]
    println!("  presenter        : x11（depth-32 叠加窗 + 空输入区）");
    #[cfg(not(feature = "x11"))]
    println!("  presenter        : 未编译（--features x11）");
    #[cfg(feature = "wayland")]
    println!("  presenter(wl)    : wayland（zwlr_layer_shell_v1 + SHM）");
    #[cfg(not(feature = "wayland"))]
    println!("  presenter(wl)    : 未编译（--features wayland）");
    #[cfg(feature = "wayland")]
    println!(
        "  session          : {}",
        if is_wayland_session() {
            "wayland（Treeland）"
        } else {
            "x11"
        }
    );
    #[cfg(feature = "dbus")]
    println!(
        "  dock follow      : dbus（Wayland 轮询 geometry；X11 订阅 FrontendWindowRectChanged）"
    );
    #[cfg(not(feature = "dbus"))]
    println!("  dock follow      : 未编译（--features dbus），几何固定");
    #[cfg(feature = "tray")]
    println!("  tray             : tray（StatusNotifierItem + DBusMenu）");
    #[cfg(not(feature = "tray"))]
    println!("  tray             : 未编译（--features tray）");

    println!(
        "  config           : {}（{}）",
        store.path().display(),
        store.source()
    );
    println!("  hot-reload       : {hot} 个键热生效（改完保存 ≤250ms 生效）");
    println!("  config warnings  : {}", config_warnings.len());
    for warning in &config_warnings {
        println!("    - {warning}");
    }
    // 读取默认配置的堆叠模式（实际启动时以 --config 指定的文件为准）
    println!(
        "  x11 stacking     : {:?}（默认 override-redirect：显示桌面不再隐藏频谱，F124）",
        spectrum_core::config::X11Stacking::default()
    );
    println!("  settings stage   : {}", spectrum_settings::SETTINGS_STAGE);
    match &dock {
        Some(probe) => {
            let d = &probe.doctor;
            let position = d.position.map_or_else(|| "-".to_owned(), |p| p.to_string());
            match probe.rect {
                Some((x, y, width, height)) => println!(
                    "  dock runtime     : position={position} rect={width}x{height} @ ({x}, {y})"
                ),
                None => println!("  dock runtime     : position={position} rect=不可用"),
            }
            println!(
                "  dock counters    : height_runtime={:?} rect_source={} resize_count={} invalid_geometry={} icon_count={:?} last_resize_ms={:?}",
                d.height_runtime,
                d.rect_source,
                d.resize_count,
                d.invalid_geometry_count,
                d.icon_count,
                d.last_commit_ms
            );
        }
        None => println!("  dock runtime     : 不可用（D-Bus 未连接或无 dock 服务）"),
    }
    println!("  fft backend      : {fft_backend}");
    println!("  sample rate      : {sample_rate}");
    println!("  bands (3200x87)  : {bands}");
    println!("  self-check       : {self_check}");
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// doctor 的 D-Bus 探测结果：停靠位置 + 权威矩形 + 几何计数器。
struct DockProbe {
    /// 权威矩形（物理像素）；没有合法观测时为 `None`。
    rect: Option<(i32, i32, u32, u32)>,
    /// 桥接器导出的运行期计数（高度、重建次数、非法几何、图标数等）。
    doctor: spectrum_core::dock_signal::DockDoctor,
}

/// doctor 的 D-Bus 探测：订阅一次 dock 事件并喂给 `DockSignalBridge`。
///
/// 这样 `doctor` 不只拿到矩形，还能给出 `DockDoctor` 的计数
/// （`resize_count` / `invalid_geometry_count` / `icon_count` 等）。
/// 只读订阅，不建窗；最多等 300ms 让三路订阅线程完成首值握手。
#[cfg(feature = "dbus")]
fn doctor_probe_dock() -> Option<DockProbe> {
    use spectrum_core::dock_signal::{DockEvent, DockSignalBridge, RectSource};
    use spectrum_platform::dock_geom::DockSubscription;
    use std::time::{Duration, Instant};

    let mut bridge = DockSignalBridge::new(RectSource::Physical);
    // 位置只在**变化**时发信号；先主动读一次，否则 doctor 里 position 恒为 null。
    if let Some(position) = spectrum_platform::dock_geom::read_position() {
        bridge.submit(DockEvent::PositionChanged(position), 0);
    }
    match DockSubscription::start(RectSource::Physical) {
        Ok(mut sub) => {
            let started = Instant::now();
            let deadline = started + Duration::from_millis(300);
            while Instant::now() < deadline {
                match sub.try_recv() {
                    Ok(Some(event)) => {
                        let now_ms =
                            u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                        bridge.submit(event, now_ms);
                        if let spectrum_core::dock_signal::DockOutcome::NeedsRefresh =
                            bridge.tick(now_ms)
                        {
                            // 尺寸模式信号没有伴随矩形变更时，主动回读一次。
                            if let Some(event) =
                                spectrum_platform::dock_geom::read_rect(RectSource::Physical)
                            {
                                bridge.submit(event, now_ms);
                            }
                        }
                    }
                    Ok(None) => std::thread::sleep(Duration::from_millis(5)),
                    Err(_) => break,
                }
            }
        }
        Err(_) => {
            // 订阅不可用（无 D-Bus 会话）时退回一次性属性读取。
            if let Some(event) = spectrum_platform::dock_geom::read_rect(RectSource::Physical) {
                bridge.submit(event, 0);
            }
        }
    }
    let rect = bridge
        .latest_rect()
        .map(|rect| (rect.x, rect.y, rect.width, rect.height));
    let doctor = bridge.doctor();
    if rect.is_none() && doctor.position.is_none() {
        return None;
    }
    Some(DockProbe { rect, doctor })
}

/// 最小 JSON 字符串转义（doctor `--json` 用，避免为一个命令引入 serde）。
#[allow(clippy::format_push_string)]
fn json_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if u32::from(c) < 0x20 => out.push_str(&format!("\\u{:04x}", u32::from(c))),
            c => out.push(c),
        }
    }
    out
}

/// `--synthetic` 频率的合法范围（48kHz 采样下不越过奈奎斯特）。
const MIN_TONE_HZ: f32 = 1.0;
const MAX_TONE_HZ: f32 = 24_000.0;
/// `--duration` 的上限：7 天。超过它基本可断定是输入错误。
const MAX_DURATION_SECS: f64 = 7.0 * 24.0 * 3600.0;

/// 命令行参数。
///
/// `Clone`：断流重连时要按同一套参数重建采集链。
#[derive(Clone)]
struct DaemonArgs {
    live: bool,
    tone_hz: f32,
    duration: Option<Duration>,
    hop_frames: usize,
    /// 配置文件路径（`None` = 用默认路径）。
    config: Option<PathBuf>,
}

impl DaemonArgs {
    fn parse(args: &[String]) -> Result<Self, String> {
        let mut out = Self {
            live: false,
            tone_hz: 440.0,
            duration: None,
            hop_frames: 1024,
            config: None,
        };
        let mut explicit_tone = false;
        let mut index = 0;
        while index < args.len() {
            match args[index].as_str() {
                "--live" => {
                    out.live = true;
                    index += 1;
                }
                "--synthetic" => {
                    let value = args.get(index + 1).ok_or("--synthetic 需要频率参数")?;
                    let hz: f32 = value
                        .parse()
                        .map_err(|_| format!("--synthetic 频率非法：{value}"))?;
                    // `NaN`/`inf` 会让 ToneSource 生成全 NaN 缓冲；超出奈奎斯特的
                    // 频率只会混叠。都在这里挡掉，给出可读错误而不是后续 panic。
                    if !hz.is_finite() || !(MIN_TONE_HZ..=MAX_TONE_HZ).contains(&hz) {
                        return Err(format!(
                            "--synthetic 频率需在 {MIN_TONE_HZ}..={MAX_TONE_HZ} Hz 之间：{value}"
                        ));
                    }
                    out.tone_hz = hz;
                    explicit_tone = true;
                    index += 2;
                }
                "--duration" => {
                    let value = args.get(index + 1).ok_or("--duration 需要秒数")?;
                    let secs: f64 = value
                        .parse()
                        .map_err(|_| format!("--duration 非法：{value}"))?;
                    // `Duration::from_secs_f64(inf)` 会 panic（release 下直接 abort）。
                    if !secs.is_finite() || secs > MAX_DURATION_SECS {
                        return Err(format!(
                            "--duration 需为 0..={MAX_DURATION_SECS} 的有限秒数：{value}"
                        ));
                    }
                    out.duration = Some(Duration::from_secs_f64(secs.max(0.1)));
                    index += 2;
                }
                "--hop" => {
                    let value = args.get(index + 1).ok_or("--hop 需要帧数")?;
                    out.hop_frames = value
                        .parse::<usize>()
                        .map_err(|_| format!("--hop 非法：{value}"))?
                        .clamp(64, 16_384);
                    index += 2;
                }
                "--config" => {
                    let value = args.get(index + 1).ok_or("--config 需要文件路径")?;
                    out.config = Some(PathBuf::from(value));
                    index += 2;
                }
                other if other.starts_with("--") => {
                    return Err(format!("未知参数 {other}"));
                }
                other => return Err(format!("多余的位置参数 {other}")),
            }
        }
        if out.live && explicit_tone {
            return Err("--live 与 --synthetic 不能同时用".to_string());
        }
        Ok(out)
    }
}

/// 主循环的共享状态。
///
/// 把「起始时刻 / 截止 / hop 帧数 / 控制通道 / 配置仓库」打包成一个结构体，
/// 是为了**不**让 `run_loop_x11` 的参数列表继续增长（本项目对参数个数有 lint 约束），
/// 同时让"热重载"这件事在两个循环里只有一处实现。
#[allow(clippy::struct_excessive_bools)]
struct Session {
    /// 进程起点（用于算 `now_ms`）。
    started: Instant,
    /// 截止时刻（`None` = 一直跑）。
    deadline: Option<Instant>,
    /// 每个 hop 的样本数。
    hop_frames: usize,
    /// 托盘控制通道。
    control: Arc<ControlState>,
    /// 配置仓库（含热重载监视）。
    store: ConfigStore,
    /// 当前是否允许上屏（`general.enabled`）。
    enabled: bool,
    /// 全屏播放视频时是否隐藏（`behavior.hide_on_fullscreen`，热生效）。
    hide_on_fullscreen: bool,
    /// 锁屏/屏保时是否隐藏（`behavior.hide_on_lock`，热生效）。
    hide_on_lock: bool,
    /// 任务栏自动隐藏时是否隐藏（`behavior.hide_on_autohide`，Wayland 热生效）。
    hide_on_autohide: bool,
    /// 当前是否正处于"全屏隐藏"状态（用于捕捉边沿）。
    hidden_by_fullscreen: bool,
    /// 当前是否正处于"锁屏隐藏"状态。
    hidden_by_lock: bool,
    /// 锁屏状态监视器（没有 dbus feature 时不存在）。
    #[cfg(feature = "dbus")]
    lock: spectrum_platform::lock::LockMonitor,
    /// 是否显示在任务栏上方（`visual.mode = AboveDock`）。
    above_dock: bool,
    /// X11 堆叠模式（仅启动时读，改它要重启 daemon）。
    #[cfg_attr(not(feature = "x11"), allow(dead_code))]
    stacking: spectrum_core::config::X11Stacking,
}

impl Session {
    fn now_ms(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_millis()).unwrap_or(0)
    }

    fn expired(&self) -> bool {
        self.deadline.is_some_and(|d| Instant::now() >= d)
    }

    /// 当前是否允许上屏。
    ///
    /// ★ 全屏隐藏期间必须一直为 false：只在检测到全屏的那一刻清一帧是不够的，
    /// 下一帧 `pipeline.push` 仍会返回 `should_present`，频谱会立刻画回来。
    fn should_present(&self) -> bool {
        presentation_allowed(self.enabled, self.hidden_by_fullscreen, self.hidden_by_lock)
    }

    /// 根据全屏/锁屏观测更新隐藏状态。
    ///
    /// 返回 `Some(true)` 表示刚进入隐藏（需要清一帧），`Some(false)` 表示刚恢复，
    /// `None` 表示状态没变。两个原因同时存在时只在**都解除**后恢复。
    #[cfg_attr(not(any(feature = "x11", feature = "wayland")), allow(dead_code))]
    fn update_hide(&mut self, fullscreen_now: bool, locked_now: bool) -> Option<bool> {
        let was_hidden = self.hidden_by_fullscreen || self.hidden_by_lock;
        self.hidden_by_fullscreen = self.hide_on_fullscreen && fullscreen_now;
        self.hidden_by_lock = self.hide_on_lock && locked_now;
        let now_hidden = self.hidden_by_fullscreen || self.hidden_by_lock;
        (now_hidden != was_hidden).then_some(now_hidden)
    }
}

/// 上屏条件的纯函数形式，便于单测覆盖隐藏状态。
const fn presentation_allowed(
    enabled: bool,
    hidden_by_fullscreen: bool,
    hidden_by_lock: bool,
) -> bool {
    enabled && !hidden_by_fullscreen && !hidden_by_lock
}

/// 窗口操作连续失败的上限（约 6 秒 @47fps）。
///
/// 分辨率切换、显示器热插拔、X 服务短暂异常都可能让单帧操作失败；
/// 一次失败就 `?` 退出会让用户看到"频谱崩了"（随后依赖 systemd 重启）。
/// 预算内只记录并跳过本帧，超过预算才认为窗口系统真的不可用。
#[cfg(any(feature = "x11", feature = "wayland"))]
const WINDOW_FAILURE_LIMIT: u32 = 300;

/// 上屏/几何/窗口操作的失败预算。
#[cfg(any(feature = "x11", feature = "wayland"))]
struct FailureBudget {
    consecutive: u32,
    limit: u32,
}

#[cfg(any(feature = "x11", feature = "wayland"))]
impl FailureBudget {
    const fn new(limit: u32) -> Self {
        Self {
            consecutive: 0,
            limit,
        }
    }

    const fn ok(&mut self) {
        self.consecutive = 0;
    }

    /// 记录一次失败；返回 `true` 表示已超出预算。
    fn fail(&mut self, context: &str, err: &str) -> bool {
        self.consecutive = self.consecutive.saturating_add(1);
        if self.consecutive == 1 || self.consecutive.is_multiple_of(30) {
            eprintln!(
                "  警告：{context}失败（连续第 {} 次）：{err}",
                self.consecutive
            );
        }
        self.consecutive >= self.limit
    }
}

/// 把一次窗口操作纳入失败预算：预算内吞掉错误，超预算才上抛。
///
/// 成功**不**清零计数：只有真正上屏成功（`put_image`）才算窗口系统健康。
/// 否则「几何查询成功但上屏一直失败」会被无关的成功调用不断清零，
/// 预算永远到不了阈值。
#[cfg(any(feature = "x11", feature = "wayland"))]
fn tolerate(
    failures: &mut FailureBudget,
    context: &str,
    result: Result<(), String>,
) -> Result<(), String> {
    match result {
        Ok(()) => Ok(()),
        Err(err) => {
            if failures.fail(context, &err) {
                Err(err)
            } else {
                Ok(())
            }
        }
    }
}

/// 检查配置是否变化，并应用差分。
///
/// 返回 `true` 表示"画面需要重画"（配色变了 / 刚被重新启用）。
///
/// 这里刻意**不**重建窗口、不重启采集：热重载的全部意义就是"改了立刻生效，
/// 且用户看不到闪烁"。
fn poll_config(
    pipeline: &mut Pipeline,
    session: &mut Session,
    now_ms: u64,
) -> Result<bool, String> {
    let Some(reload) = session.store.poll(Instant::now()) else {
        return Ok(false);
    };
    match reload {
        Reload::Rejected { error } => {
            // 保留上一次的好配置；只报一次（store 内部已按错误文本去重）
            println!("配置错误（保留上一次生效的配置）：{error}");
            Ok(false)
        }
        Reload::Applied {
            revision,
            delta,
            warnings,
        } => {
            let started = Instant::now();
            let cfg = session.store.config();
            let plan = apply::plan(cfg);
            let mut repaint = false;

            if delta.needs_analyzer_rebuild() {
                pipeline
                    .reconfigure_analyzer(&plan.analyzer)
                    .map_err(|e| format!("重建分析器失败：{e}"))?;
                repaint = true;
            }
            if delta.keys.iter().any(|k| k.starts_with("visual.")) {
                pipeline.apply_style(plan.style);
                repaint = true;
            }
            if delta.keys.iter().any(|k| k.starts_with("behavior.silence")) {
                pipeline.set_silence(plan.silence_dbfs, plan.silence_timeout_ms);
            }
            if delta.keys.contains(&"behavior.hide_on_fullscreen") {
                session.hide_on_fullscreen = plan.hide_on_fullscreen;
                repaint = true;
            }
            if delta.keys.contains(&"behavior.hide_on_lock") {
                session.hide_on_lock = plan.hide_on_lock;
                repaint = true;
            }
            if delta.keys.contains(&"behavior.hide_on_autohide") {
                session.hide_on_autohide = plan.hide_on_autohide;
                repaint = true;
            }
            if delta.keys.contains(&"visual.mode") {
                session.above_dock = session.store.config().visual.mode == VisualMode::AboveDock;
                repaint = true;
            }
            if delta.keys.contains(&"general.enabled") {
                session.enabled = plan.enabled;
                repaint = true;
                if plan.enabled {
                    // 重新启用：重置平滑与静音门限，
                    // 否则会用停用前的旧状态插值，第一帧从半空高度"炸"起来
                    pipeline.clear(now_ms);
                }
            }

            println!(
                "配置热重载 rev={revision} 改动=[{}] 告警={warnings} 耗时={:.1}ms",
                delta.describe(),
                started.elapsed().as_secs_f64() * 1000.0
            );
            Ok(repaint)
        }
    }
}

/// 帧轨迹：定期打印**上屏前**的像素摘要。
///
/// 为什么需要它：热重载的证据不能只有"日志说改了"，还得证明**画面真的变了**。
/// 这里打印的校验和取自即将交给 `put_image` 的那块像素缓冲（与上屏数据逐字节相同），
/// 因此"校验和变了"= "屏幕上的画面变了"，且不依赖 X 窗口回读（depth-32 回读在
/// 部分服务端会 `BadMatch`，M41）。
///
/// 开启方式：环境变量 `SPECTRUM_FRAME_TRACE=<间隔帧数>`（默认关闭）。
#[cfg(feature = "x11")]
struct FrameTrace {
    /// 每隔多少帧打印一次。
    interval: u64,
    /// 已打印的帧数。
    printed: u64,
}

#[cfg(feature = "x11")]
impl FrameTrace {
    /// 从环境变量读取设置；未设置则不启用。
    fn from_env() -> Option<Self> {
        let raw = std::env::var("SPECTRUM_FRAME_TRACE").ok()?;
        let interval = raw.trim().parse::<u64>().unwrap_or(20).max(1);
        Some(Self {
            interval,
            printed: 0,
        })
    }

    /// 到达打印点就打印一行摘要。
    fn maybe(&mut self, pipeline: &Pipeline, surface: &spectrum_render::Surface, now_ms: u64) {
        self.printed += 1;
        if !self.printed.is_multiple_of(self.interval) {
            return;
        }
        let mut hash = 0xcbf2_9ce4_8422_2325_u64;
        let mut nonblank = 0_u64;
        for px in surface.pixels() {
            hash ^= u64::from(*px);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            if *px & 0xFF00_0000 != 0 {
                nonblank += 1;
            }
        }
        let style = pipeline.config().style;
        println!(
            "frame t={now_ms}ms bars={} px=0x{hash:016x} nonblank={nonblank} alpha={} colors={:06x}..{:06x} gap={}",
            pipeline.band_count(),
            style.alpha,
            style.color_bottom & 0x00FF_FFFF,
            style.color_top & 0x00FF_FFFF,
            style.gap_px
        );
    }
}

/// 首次把配置套到流水线上（启动时做一次）。
fn apply_initial_config(pipeline: &mut Pipeline, store: &ConfigStore) -> Result<(), String> {
    let plan = apply::plan(store.config());
    pipeline
        .reconfigure_analyzer(&plan.analyzer)
        .map_err(|e| format!("初始化分析器失败：{e}"))?;
    pipeline.apply_style(plan.style);
    pipeline.set_silence(plan.silence_dbfs, plan.silence_timeout_ms);
    Ok(())
}

/// 单实例冲突的独立退出码；systemd 单元据此不重启。
const EXIT_ALREADY_RUNNING: u8 = 3;

/// daemon 启动错误：区分"已有实例"与真正的失败。
#[derive(Debug)]
enum DaemonError {
    AlreadyRunning(String),
    Failed(String),
}

impl std::fmt::Display for DaemonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyRunning(msg) | Self::Failed(msg) => f.write_str(msg),
        }
    }
}

impl From<String> for DaemonError {
    fn from(message: String) -> Self {
        Self::Failed(message)
    }
}

/// 启动路径的编排（参数 → 单实例 → 配置 → 后端选择 → 主循环）集中在一处，
/// 拆开会把"配置告警/后端回退"的先后顺序打散，反而更难核对。
#[allow(clippy::too_many_lines)]
fn run_daemon(args: &[String]) -> Result<(), DaemonError> {
    let opts = DaemonArgs::parse(args)?;

    // 单实例：两个 daemon 会各建一块覆盖同一区域的窗口，画面互相覆盖、闪动。
    // 必须在建窗之前挡住，否则用户会先看到闪烁、再看到错误。
    let instance = match spectrum_daemon::instance::SingleInstance::acquire(
        spectrum_daemon::instance::default_lock_path(),
    ) {
        Ok(instance) => instance,
        Err(err) => {
            if err
                .downcast_ref::<spectrum_daemon::instance::AlreadyRunning>()
                .is_some()
            {
                return Err(DaemonError::AlreadyRunning(err.to_string()));
            }
            // 只读 / 无权限是 systemd 加固（ProtectSystem）挡住 $XDG_RUNTIME_DIR 的典型
            // 症状（真机 EROFS 曾导致登录自启反复重启却无频谱）；给出可操作的提示。
            let msg = err.to_string();
            let hint = err
                .downcast_ref::<std::io::Error>()
                .and_then(std::io::Error::raw_os_error)
                .is_some_and(|code| code == 30 || code == 13);
            return Err(DaemonError::Failed(if hint {
                format!(
                    "{msg}（无法在 $XDG_RUNTIME_DIR 建锁：若由 systemd 自启，请确认单元里 \
                     ProtectSystem 未把该目录挂只读，并保留 ReadWritePaths=%t）"
                )
            } else {
                msg
            }));
        }
    };
    // 托盘通过这条通道发 `stop`；主循环每帧查一次（见 ControlState）
    let control = instance.control();

    // 配置：先读进来，再建流水线 —— 这样首帧就是用户要的样式，
    // 而不是"先用默认样式跑一帧再改过来"（那会闪一下）。
    // 同时给几何兜底链提供 `dock.height`。
    let config_path = opts
        .config
        .clone()
        .unwrap_or_else(spectrum_settings::store::default_path);
    let store = ConfigStore::open(&config_path);
    println!("配置 {}（{}）", store.path().display(), store.source());
    for w in store.warnings() {
        println!("  配置告警：{w}");
    }
    // advanced.renderer：只有软件渲染（shm）实现了；gl 要明确告知而不是静默忽略。
    if store.config().advanced.renderer == "gl" {
        println!("  配置告警：advanced.renderer=gl 未实现，使用软件渲染（shm）");
    }
    // advanced.x11_stacking：managed-below 未实现，运行时降到 override-redirect。
    let stacking_pref = store.config().advanced.x11_stacking;
    if stacking_pref == spectrum_core::config::X11Stacking::ManagedBelow {
        println!(
            "  配置告警：advanced.x11_stacking=managed-below 未实现，按 override-redirect 运行"
        );
    }
    let stacking = if stacking_pref == spectrum_core::config::X11Stacking::ManagedBelow {
        spectrum_core::config::X11Stacking::OverrideRedirect
    } else {
        stacking_pref
    };
    let fallback_height = u16::try_from(store.config().dock.height).unwrap_or(45);

    #[cfg(feature = "wayland")]
    let wayland_session = {
        let detected = is_wayland_session();
        match store.config().advanced.backend.as_str() {
            // 显式要求 wayland：非 Wayland 会话下没有可用的合成器，回退自动判断。
            "wayland" => {
                if !detected {
                    println!(
                        "  配置告警：advanced.backend=wayland，但当前不是 Wayland 会话，按 auto 处理"
                    );
                }
                detected
            }
            // 显式要求 X11：Wayland 会话里也走 XWayland（用户可能在排查 Treeland 问题）。
            "x11" => {
                #[cfg(feature = "x11")]
                {
                    if detected {
                        println!("advanced.backend=x11：在 Wayland 会话中强制走 XWayland");
                    }
                    false
                }
                #[cfg(not(feature = "x11"))]
                {
                    println!(
                        "  配置告警：advanced.backend=x11，但本次构建未启用 x11，按 auto 处理"
                    );
                    detected
                }
            }
            _ => detected,
        }
    };
    #[cfg(all(not(feature = "wayland"), not(feature = "x11")))]
    if store.config().advanced.backend != "auto" {
        println!("  配置告警：advanced.backend 需要 x11/wayland feature，本次构建未启用");
    }
    #[cfg(feature = "wayland")]
    let (geometry, wayland_position) = if wayland_session {
        initial_wayland_geometry(fallback_height)
    } else {
        (initial_geometry(fallback_height), 2)
    };
    #[cfg(not(feature = "wayland"))]
    let geometry = initial_geometry(fallback_height);
    println!(
        "geometry {}x{} @ ({}, {})",
        geometry.width, geometry.height, geometry.x, geometry.y
    );

    let mut config = PipelineConfig::for_geometry(geometry.width, geometry.height);
    config.hop_frames = opts.hop_frames;
    config.style = RenderStyle::new();
    let mut pipeline = Pipeline::new(config).map_err(|e| e.to_string())?;
    apply_initial_config(&mut pipeline, &store)?;
    println!(
        "pipeline: {} 频段 / hop {} 帧 / sample_rate {}",
        pipeline.band_count(),
        opts.hop_frames,
        pipeline.config().sample_rate
    );

    let started = Instant::now();
    let deadline = opts.duration.map(|d| started + d);
    let mut source = ManagedSource::open(&opts, pipeline.config().sample_rate)?;
    let enabled = store.config().general.enabled;
    let hide_on_fullscreen = store.config().behavior.hide_on_fullscreen;
    let hide_on_lock = store.config().behavior.hide_on_lock;
    let hide_on_autohide = store.config().behavior.hide_on_autohide;
    let above_dock = store.config().visual.mode == VisualMode::AboveDock;
    #[cfg(feature = "dbus")]
    let lock = spectrum_platform::lock::LockMonitor::start();

    let mut session = Session {
        started,
        deadline,
        hop_frames: opts.hop_frames,
        control,
        store,
        enabled,
        hide_on_fullscreen,
        hide_on_lock,
        hide_on_autohide,
        hidden_by_fullscreen: false,
        hidden_by_lock: false,
        #[cfg(feature = "dbus")]
        lock,
        above_dock,
        stacking,
    };

    #[cfg(feature = "wayland")]
    if wayland_session {
        return run_loop_wayland(
            pipeline,
            geometry,
            wayland_position,
            &mut source,
            &mut session,
        )
        .map_err(DaemonError::from);
    }
    #[cfg(feature = "x11")]
    {
        run_loop_x11(pipeline, geometry, &mut source, &mut session).map_err(DaemonError::from)
    }
    #[cfg(not(feature = "x11"))]
    {
        run_loop_headless(pipeline, &mut source, &mut session).map_err(DaemonError::from)
    }
}

/// 无 X11 的循环：只推进流水线并打印摘要（CI / 容器可用）。
///
/// 返回 `Result` 是为了与 `run_loop_x11` **保持同一签名**（两者按 feature 二选一），
/// 而不是因为这里真的会失败 —— 本函数内没有任何 `?`。
#[cfg(not(feature = "x11"))]
#[allow(clippy::unnecessary_wraps)]
fn run_loop_headless(
    mut pipeline: Pipeline,
    source: &mut ManagedSource,
    session: &mut Session,
) -> Result<(), String> {
    let mut hop = vec![0.0_f32; session.hop_frames];
    let mut presented = 0_u64;
    let mut cleared = 0_u64;
    // 跨帧复用 bands 缓冲（静音帧不再每帧分配）。
    let mut frame = FrameOutcome::default();
    let mut retry_logged = 0_u32;
    loop {
        if session.expired() {
            break;
        }
        if session.control.stop_requested() {
            println!("收到停止请求，退出");
            break;
        }
        let now_ms = session.now_ms();
        match source.read(&mut hop, now_ms) {
            ReadOutcome::Filled(n) => {
                if session.should_present() {
                    pipeline.push_into(&hop[..n], now_ms, &mut frame);
                    if frame.should_present {
                        presented += 1;
                        if presented % 40 == 1 {
                            print_summary(&frame);
                        }
                    }
                }
                // 已停用：只把音频读走，不分析、不上屏（省 FFT）
            }
            ReadOutcome::Idle => std::thread::sleep(Duration::from_millis(2)),
            ReadOutcome::Lost(why) => {
                println!("采集断流：{why} —— 自动重连中");
                pipeline.clear(now_ms);
                cleared += 1;
                retry_logged = 0;
            }
            ReadOutcome::RetryFailed(why) => {
                retry_logged += 1;
                if retry_logged <= 2 || retry_logged.is_multiple_of(20) {
                    println!("  重连失败（第 {retry_logged} 次）：{why}");
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            ReadOutcome::Recovered {
                attempts,
                outage_ms,
            } => {
                println!("采集已恢复（重试 {attempts} 次，累计断流 {outage_ms}ms）");
                pipeline.clear(now_ms);
                cleared += 1;
            }
            ReadOutcome::Waiting => std::thread::sleep(Duration::from_millis(20)),
        }

        // 配置热重载：内部自带 250ms 节流，这里每帧调用也只是被节流挡掉
        poll_config(&mut pipeline, session, now_ms)?;
    }
    let outages = source.reconnector().stats();
    println!(
        "presented={presented} frames={} skipped_silent={} cleared={cleared} reconnects={} outages={} outage_ms={}",
        pipeline.stats().frames,
        pipeline.stats().skipped_silent,
        source.reconnects(),
        outages.outages,
        outages.total_outage_ms
    );
    Ok(())
}

/// 带 X11 上屏与几何跟随的循环。
///
/// 行数超限是因为它同时承担五件事（读 PCM → 分析 → 上屏 → 几何跟随 → 配置/全屏），
/// 拆开反而要把 `session` 的状态在函数间传来传去。
#[cfg(feature = "x11")]
#[allow(clippy::too_many_lines)]
fn run_loop_x11(
    mut pipeline: Pipeline,
    geometry: OverlayGeometry,
    source: &mut ManagedSource,
    session: &mut Session,
) -> Result<(), String> {
    use spectrum_platform::x11::X11Overlay;

    // 堆叠模式（仅启动时读）：默认 override-redirect——绕过 WM，
    // 「显示桌面」/ 最小化不再隐藏频谱（F124，用户真机反馈）。
    let or_mode = session.stacking == spectrum_core::config::X11Stacking::OverrideRedirect;
    // 先启动几何跟随，拿初始停靠位置，避免顶部任务栏在启动第一帧就被移到屏幕外。
    let mut dock = DockWatcher::start();
    let initial = if session.above_dock {
        above_geometry(geometry, dock.position)
    } else {
        geometry
    };
    let mut overlay = if or_mode {
        X11Overlay::create_override_redirect(initial).map_err(|e| e.to_string())?
    } else {
        X11Overlay::create(initial).map_err(|e| e.to_string())?
    };
    if or_mode {
        println!("stacking    : override-redirect（绕过 WM，显示桌面不再隐藏频谱）");
    } else {
        println!("stacking    : managed（受管窗口；显示桌面会隐藏频谱）");
    }
    let actual = {
        use spectrum_platform::overlay::OverlayBackend;
        overlay.geometry().map_err(|e| e.to_string())?
    };
    pipeline.resize(actual.width, actual.height);
    println!(
        "overlay 0x{:x} actual {}x{}",
        overlay.window_id(),
        actual.width,
        actual.height
    );

    let mut trace = FrameTrace::from_env();
    // 全屏检测：独立 X 连接；连不上（如无 EWMH 环境）就等价于"永不全屏"
    let mut fullscreen = spectrum_platform::x11::fullscreen::FullscreenMonitor::connect()
        .map_err(|e| e.to_string())
        .ok();
    let mut last_fullscreen_check = Instant::now();
    let mut hop = vec![0.0_f32; session.hop_frames];
    let mut presented = 0_u64;
    let mut skipped = 0_u64;
    let mut cleared = 0_u64;
    let mut retry_logged = 0_u32;
    // 窗口系统瞬时错误（分辨率切换/热插拔）不应立刻退出 daemon。
    let mut failures = FailureBudget::new(WINDOW_FAILURE_LIMIT);
    // 跨帧复用 bands 缓冲（静音帧不再每帧分配）。
    let mut frame = FrameOutcome::default();

    loop {
        if session.expired() {
            break;
        }
        // 托盘"停止频谱"：优雅退出，后面的 `overlay.destroy()` 会销毁叠加窗。
        // 之所以走这条而不是托盘 kill，是因为 kill 不会走 destroy，
        // 窗口要等 X 连接断开才消失，用户会看到残影。
        if session.control.stop_requested() {
            println!("收到停止请求，退出");
            break;
        }
        let now_ms = session.now_ms();

        // 1) 取一整个 hop（断流不再中断循环，见 ManagedSource）
        match source.read(&mut hop, now_ms) {
            ReadOutcome::Filled(n) => {
                if session.should_present() {
                    // 2) 推进流水线（复用 bands 缓冲）
                    pipeline.push_into(&hop[..n], now_ms, &mut frame);
                    // 3) 需要时上屏
                    if frame.should_present {
                        let surface = pipeline.surface();
                        if let Some(trace) = trace.as_mut() {
                            trace.maybe(&pipeline, surface, now_ms);
                        }
                        match overlay.put_image(surface.pixels(), surface.width(), surface.height())
                        {
                            Ok(()) => {
                                failures.ok();
                                presented += 1;
                            }
                            Err(err) => tolerate(&mut failures, "上屏", Err(err.to_string()))?,
                        }
                    } else {
                        skipped += 1;
                    }
                } else {
                    // 已停用（general.enabled = false）：只把音频读走，
                    // 不分析、不上屏 —— 采集流不能停，否则 ringbuf 会堆爆。
                    skipped += 1;
                }
            }
            ReadOutcome::Idle => std::thread::sleep(Duration::from_millis(2)),
            ReadOutcome::Lost(why) => {
                println!("采集断流：{why} —— 画面清空，自动重连中");
                tolerate(
                    &mut failures,
                    "清屏",
                    clear_overlay(&mut pipeline, &mut overlay, now_ms),
                )?;
                cleared += 1;
                retry_logged = 0;
            }
            ReadOutcome::RetryFailed(why) => {
                retry_logged += 1;
                if retry_logged <= 2 || retry_logged.is_multiple_of(20) {
                    println!("  重连失败（第 {retry_logged} 次）：{why}");
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            ReadOutcome::Recovered {
                attempts,
                outage_ms,
            } => {
                println!("采集已恢复（重试 {attempts} 次，累计断流 {outage_ms}ms）");
                // 恢复后先清一帧：否则下一帧会从"断流前的旧高度"起跳
                tolerate(
                    &mut failures,
                    "清屏",
                    clear_overlay(&mut pipeline, &mut overlay, now_ms),
                )?;
                cleared += 1;
            }
            ReadOutcome::Waiting => std::thread::sleep(Duration::from_millis(20)),
        }

        // 4) 几何变化（非阻塞）。断流期间也照常跟随 ——
        //    用户完全可能一边等音频恢复一边拖任务栏高度。
        let geometry_changed =
            match dock.poll(&mut pipeline, &mut overlay, session.above_dock, now_ms) {
                Ok(changed) => changed,
                Err(err) => {
                    tolerate(&mut failures, "任务栏几何更新", Err(err))?;
                    false
                }
            };
        if geometry_changed {
            println!(
                "  几何变化 → {}x{}（像素缓冲已重建）",
                pipeline.config().width,
                pipeline.config().height
            );
        }

        // 5) 配置热重载（内部 250ms 节流）。
        //    放在几何之后：配置变更比几何变更少，不需要抢在渲染前面。
        //    配置本身有问题时保留旧配置继续跑，不退出 daemon。
        match poll_config(&mut pipeline, session, now_ms) {
            Ok(true) => {
                // 停用时立刻清一帧，否则最后一帧会冻在屏幕上（M44 同族）
                if !session.enabled {
                    tolerate(
                        &mut failures,
                        "清屏",
                        clear_overlay(&mut pipeline, &mut overlay, now_ms),
                    )?;
                    cleared += 1;
                }
            }
            Ok(false) => {}
            Err(err) => eprintln!("  警告：配置应用失败（保留旧配置）：{err}"),
        }

        // 6) 全屏/锁屏隐藏（500ms 轮询）+ OR 窗口定期置顶。
        //    全屏切换是秒级操作，两次 GetProperty（微秒级）可忽略；
        //    OR 窗口不被 WM 管理，别的 OR 窗口出现时会盖上来，靠定期 raise 顶回去。
        if Instant::now() >= last_fullscreen_check + Duration::from_millis(500) {
            last_fullscreen_check = Instant::now();
            let fullscreen_now = fullscreen
                .as_mut()
                .is_some_and(spectrum_platform::x11::fullscreen::FullscreenMonitor::is_fullscreen);
            #[cfg(feature = "dbus")]
            let locked_now = session.lock.is_locked();
            #[cfg(not(feature = "dbus"))]
            let locked_now = false;
            if let Some(hidden) = session.update_hide(fullscreen_now, locked_now) {
                if hidden {
                    tolerate(
                        &mut failures,
                        "清屏",
                        clear_overlay(&mut pipeline, &mut overlay, now_ms),
                    )?;
                    cleared += 1;
                    println!("全屏/锁屏 → 隐藏频谱");
                } else {
                    println!("全屏/锁屏结束 → 恢复频谱");
                    // 重置平滑：恢复后第一帧不要从旧高度"炸"起来
                    pipeline.clear(now_ms);
                }
            }
            // 锁屏期间不再置顶，避免 OR 窗口盖在锁屏层上。
            if overlay.is_override_redirect() && !session.hidden_by_lock {
                tolerate(
                    &mut failures,
                    "窗口置顶",
                    overlay.raise().map_err(|e| e.to_string()),
                )?;
            }
        }
    }

    let _ = overlay.destroy();
    let outages = source.reconnector().stats();
    println!(
        "presented={presented} skipped_silent={skipped} frames={} cleared={cleared} reconnects={} outages={} outage_ms={}",
        pipeline.stats().frames,
        source.reconnects(),
        outages.outages,
        outages.total_outage_ms
    );
    Ok(())
}

/// 清一帧屏并上屏（断流 / 恢复时调用）。
///
/// 断流后没有 PCM 可推进，最后一帧会**冻在屏幕上** ——
/// 用户看到"音乐断了但柱子还举着"，会以为程序卡死。
#[cfg(feature = "x11")]
fn clear_overlay(
    pipeline: &mut Pipeline,
    overlay: &mut spectrum_platform::x11::X11Overlay,
    now_ms: u64,
) -> Result<(), String> {
    let surface = pipeline.clear(now_ms);
    overlay
        .put_image(surface.pixels(), surface.width(), surface.height())
        .map_err(|e| e.to_string())
}

/// 当前会话是否为 Wayland（Treeland）。
///
/// 优先看 `XDG_SESSION_TYPE`；systemd 用户服务不继承该变量时，
/// 用 `$XDG_RUNTIME_DIR/treeland.socket` 作为兜底判据。
#[cfg(feature = "wayland")]
fn is_wayland_session() -> bool {
    match std::env::var("XDG_SESSION_TYPE").as_deref() {
        Ok("wayland") => true,
        Ok("x11") => false,
        _ => wayland_socket_path().is_some(),
    }
}

/// 解析 Wayland socket 绝对路径（`WAYLAND_DISPLAY` 可能是相对名）。
#[cfg(feature = "wayland")]
fn wayland_socket_path() -> Option<PathBuf> {
    // 空串按“未设置”处理：否则 `join("")` 会返回运行时目录本身，
    // 后续把它当 socket 路径连接会得到完全无关的错误。
    if let Some(value) = std::env::var_os("WAYLAND_DISPLAY").filter(|value| !value.is_empty()) {
        let path = PathBuf::from(value);
        if path.is_absolute() {
            return Some(path);
        }
        if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
            return Some(PathBuf::from(dir).join(path));
        }
    }
    let dir = std::env::var_os("XDG_RUNTIME_DIR")?;
    let path = PathBuf::from(dir).join("treeland.socket");
    path.is_file().then_some(path)
}

/// Treeland 的 dock 逻辑尺寸与停靠位置（dde-shell D-Bus）。
///
/// `geometry` 是逻辑像素；`Position` 0/1/2/3 = top/right/bottom/left。
/// 物理缓冲尺寸由 Wayland 线程回传的实际 scale 决定，不再硬编码倍率。
#[cfg(feature = "wayland")]
fn read_wayland_dock() -> Option<(u16, u16, i32)> {
    zbus::block_on(async {
        let conn = zbus::Connection::session().await.ok()?;
        let dock = zbus::Proxy::new(
            &conn,
            "org.deepin.dde.Dock1",
            "/org/deepin/dde/Dock1",
            "org.deepin.dde.Dock1",
        )
        .await
        .ok()?;
        let (_x, _y, w, h): (i32, i32, i32, i32) = dock.get_property("geometry").await.ok()?;
        let daemon = zbus::Proxy::new(
            &conn,
            "org.deepin.dde.Dock1",
            "/org/deepin/dde/Dock1",
            "org.deepin.dde.daemon.Dock1",
        )
        .await
        .ok()?;
        let position: i32 = daemon.get_property("Position").await.ok().unwrap_or(2);
        Some((u16::try_from(w).ok()?, u16::try_from(h).ok()?, position))
    })
}

/// Treeland 下 layer-shell 的初始几何（**逻辑尺寸**）。
///
/// 实际 scale 由 Wayland 线程在绑定 `wl_output` 后回传；主循环按
/// `逻辑尺寸 × 实际 scale` 重建像素缓冲，非 2x 屏幕与运行期改缩放都不会冻结。
#[cfg(feature = "wayland")]
fn initial_wayland_geometry(fallback_height: u16) -> (OverlayGeometry, i32) {
    let (w, h, position) = read_wayland_dock().unwrap_or_else(|| {
        let w = std::env::var("SPECTRUM_WIDTH")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1829);
        let h = std::env::var("SPECTRUM_HEIGHT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(fallback_height.max(1));
        (w, h, 2)
    });
    // 启动瞬间任务栏可能正处于自动隐藏动画（2px）；用配置高度垫一下，
    // 避免第一块像素缓冲退化。后续 poll 会按真实高度隐藏/恢复。
    let h = if h < 20 { fallback_height.max(20) } else { h };
    (OverlayGeometry::new(0, 0, w.max(1), h.max(1)), position)
}

/// 任务栏自动隐藏动画期间的中间高度（2/12px 等）低于这个值。
#[cfg(feature = "wayland")]
const MIN_DOCK_HEIGHT: u16 = 20;

/// 几何稳定窗口：同一观测持续这么久才算“动画结束”，才提交给主线程。
#[cfg(feature = "wayland")]
const DOCK_STABLE_MS: u64 = 250;

/// 订阅线程的采样间隔（探测“已稳定”的时间分辨率）。
#[cfg(feature = "wayland")]
const DOCK_WATCH_TICK_MS: u64 = 100;

/// 几何去抖：同一观测持续 [`DOCK_STABLE_MS`] 才产出一次。
///
/// 自动隐藏动画会连续产生 12/2/104/106/76… 等中间值；
/// 这里把它们挡在提交之前，主线程只看到最终稳定值。
#[cfg(feature = "wayland")]
#[derive(Debug, Default)]
struct DockDebounce {
    pending: Option<(u16, u16, i32)>,
    /// 已提交过的值：避免同一稳定值在之后每一轮都被重新提交。
    last_emitted: Option<(u16, u16, i32)>,
    changed_at_ms: u64,
}

#[cfg(feature = "wayland")]
impl DockDebounce {
    fn offer(&mut self, value: (u16, u16, i32), now_ms: u64) {
        if self.pending == Some(value) || self.last_emitted == Some(value) {
            return;
        }
        self.pending = Some(value);
        self.changed_at_ms = now_ms;
    }

    fn ready(&mut self, now_ms: u64) -> Option<(u16, u16, i32)> {
        if now_ms.saturating_sub(self.changed_at_ms) >= DOCK_STABLE_MS {
            let value = self.pending.take();
            if value.is_some() {
                self.last_emitted = value;
            }
            value
        } else {
            None
        }
    }
}

/// Wayland 下的任务栏几何跟随。
///
/// 事件来源：`DockSubscription::Logical`（`geometry` 属性 + `Position` 信号）。
/// 订阅线程做“稳定窗口”去抖，主线程用
/// [`spectrum_core::dock_signal::DockSignalBridge`]（内部是
/// `GeometryTracker`，16ms 合并 + 非法值过滤）统一处理，与 X11 路径一致。
#[cfg(feature = "wayland")]
struct WaylandDockWatcher {
    rx: Receiver<(u16, u16, i32)>,
    alive: Arc<AtomicBool>,
    /// 16ms 合并 + 非法值过滤（与 X11 同一套 `GeometryTracker`）。
    bridge: spectrum_core::dock_signal::DockSignalBridge,
    /// 最近一次**已应用**的 dock 逻辑尺寸。
    last: Option<(u16, u16)>,
    /// 当前停靠位置（0=top / 1=right / 2=bottom / 3=left）。
    position: i32,
    /// 任务栏处于自动隐藏状态（高度 < [`MIN_DOCK_HEIGHT`]）。
    hidden: bool,
}

#[cfg(feature = "wayland")]
impl WaylandDockWatcher {
    fn start() -> Self {
        use spectrum_core::dock_signal::{DockEvent, RectSource};
        use spectrum_platform::dock_geom::DockSubscription;

        let (tx, rx) = std::sync::mpsc::channel();
        let alive = Arc::new(AtomicBool::new(true));
        let alive_thread = Arc::clone(&alive);
        let _ = std::thread::Builder::new()
            .name("deepin-spectrum-wl-dock".to_owned())
            .spawn(move || {
                // 事件驱动：`geometry` 是属性，用 `DockSubscription::Logical`
                // 的 PropertiesChanged 订阅，不再每 500ms 新建一次 D-Bus 连接。
                let mut subscription = DockSubscription::start(RectSource::Logical).ok();
                // 首值：订阅只推变化，位置/初始矩形需要主动读一次。
                let mut snapshot = read_wayland_dock();
                let mut debounce = DockDebounce::default();
                let started = Instant::now();
                let mut last_fallback: Option<Instant> = None;
                let mut retry_at = Instant::now() + Duration::from_secs(2);
                let mut start_failures = 0_u32;

                while alive_thread.load(Ordering::SeqCst) {
                    if let Some(sub) = subscription.as_mut() {
                        loop {
                            match sub.try_recv() {
                                Ok(Some(DockEvent::RectChanged { width, height, .. })) => {
                                    let position = snapshot.map_or(2, |(_, _, p)| p);
                                    if let (Ok(w), Ok(h)) =
                                        (u16::try_from(width), u16::try_from(height))
                                    {
                                        snapshot = Some((w, h, position));
                                    }
                                }
                                Ok(Some(DockEvent::PositionChanged(position))) => {
                                    let (w, h) = snapshot.map_or((0, 0), |(w, h, _)| (w, h));
                                    snapshot = Some((w, h, position));
                                }
                                Ok(Some(_)) => {}
                                Ok(None) => break,
                                Err(_) => {
                                    subscription = None;
                                    retry_at = Instant::now() + Duration::from_secs(2);
                                    break;
                                }
                            }
                        }
                    }

                    // 订阅失效后节流重建；连续失败 5 次就永久退回轮询。
                    if subscription.is_none() && start_failures < 5 && Instant::now() >= retry_at {
                        if let Ok(sub) = DockSubscription::start(RectSource::Logical) {
                            subscription = Some(sub);
                            start_failures = 0;
                        } else {
                            start_failures += 1;
                            retry_at = Instant::now() + Duration::from_secs(2);
                        }
                    }

                    // 订阅不可用时的兜底：500ms 轮询（每次新连接，仅在降级期）。
                    if subscription.is_none()
                        && last_fallback.is_none_or(|at| at.elapsed() >= Duration::from_millis(500))
                    {
                        if let Some(rect) = read_wayland_dock() {
                            snapshot = Some(rect);
                        }
                        last_fallback = Some(Instant::now());
                    }

                    let now_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                    if let Some(rect) = snapshot {
                        debounce.offer(rect, now_ms);
                    }
                    // 稳定窗口结束才把最终值交给主线程（中间动画值直接丢弃）。
                    if let Some(rect) = debounce.ready(now_ms)
                        && tx.send(rect).is_err()
                    {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(DOCK_WATCH_TICK_MS));
                }
            });
        Self {
            rx,
            alive,
            bridge: spectrum_core::dock_signal::DockSignalBridge::new(RectSource::Logical),
            last: None,
            position: 2,
            hidden: false,
        }
    }

    /// 任务栏是否处于自动隐藏状态（主循环据此停止上屏）。
    fn dock_hidden(&self) -> bool {
        self.hidden
    }

    /// 非阻塞处理几何事件；返回是否发生尺寸/可见性变化。
    ///
    /// 订阅线程已做“稳定窗口”去抖；这里再走 `DockSignalBridge` 的 16ms
    /// 合并与非法值过滤，与 X11 路径共用同一套几何状态机。
    fn poll(
        &mut self,
        pipeline: &mut Pipeline,
        overlay: &mut WaylandOverlay,
        now_ms: u64,
        hide_on_autohide: bool,
    ) -> Result<bool, String> {
        use spectrum_core::dock_signal::{DockEvent, DockOutcome};
        use spectrum_platform::overlay::OverlayBackend;

        loop {
            match self.rx.try_recv() {
                Ok((w, h, position)) => {
                    self.position = position;
                    self.bridge.submit(
                        DockEvent::RectChanged {
                            x: 0,
                            y: 0,
                            width: u32::from(w),
                            height: u32::from(h),
                        },
                        now_ms,
                    );
                    self.bridge
                        .submit(DockEvent::PositionChanged(position), now_ms);
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.alive.store(false, Ordering::SeqCst);
                    break;
                }
            }
        }

        // 16ms 合并窗口结束后提交最终尺寸；与 X11 路径完全一致。
        let DockOutcome::Commit(rect) = self.bridge.tick(now_ms) else {
            return Ok(false);
        };
        let w = u16::try_from(rect.width).unwrap_or(0);
        let h = u16::try_from(rect.height).unwrap_or(0);
        if w == 0 || h == 0 {
            return Ok(false);
        }

        if h < MIN_DOCK_HEIGHT {
            return if hide_on_autohide && !self.hidden {
                self.hidden = true;
                pipeline.clear(now_ms);
                overlay.hide().map_err(|e| e.to_string())?;
                println!("  任务栏自动隐藏（{h}px）→ 暂停上屏");
                Ok(true)
            } else {
                Ok(false)
            };
        }

        let restore = self.hidden;
        self.hidden = false;
        self.last = Some((w, h));
        let changed = apply_wayland_geometry(pipeline, overlay, w, h)?;
        overlay
            .set_position(self.position)
            .map_err(|e| e.to_string())?;
        if restore {
            pipeline.clear(now_ms);
            overlay.show().map_err(|e| e.to_string())?;
            println!("  任务栏恢复（{w}x{h}）→ 继续上屏");
            return Ok(true);
        }
        if changed {
            pipeline.clear(now_ms);
        }
        Ok(changed)
    }

    /// 配置关闭「自动隐藏时隐藏」后，立即恢复上屏（不必等下一次 dock 事件）。
    fn force_show(
        &mut self,
        pipeline: &mut Pipeline,
        overlay: &mut WaylandOverlay,
        now_ms: u64,
    ) -> Result<(), String> {
        use spectrum_platform::overlay::OverlayBackend;

        if !self.hidden {
            return Ok(());
        }
        self.hidden = false;
        if let Some((w, h)) = self.last {
            apply_wayland_geometry(pipeline, overlay, w, h)?;
        }
        pipeline.clear(now_ms);
        overlay.show().map_err(|e| e.to_string())
    }

    /// 用最近一次 dock 逻辑尺寸重新应用（scale 变化后调用）。
    ///
    /// 还没收到 dock 事件时用启动时的逻辑尺寸兜底，避免把旧的“物理尺寸”
    /// 按新 scale 再除一次导致窗口缩小。
    fn reapply(
        &self,
        pipeline: &mut Pipeline,
        overlay: &mut WaylandOverlay,
        fallback: (u16, u16),
    ) -> Result<bool, String> {
        let (w, h) = self.last.unwrap_or(fallback);
        apply_wayland_geometry(pipeline, overlay, w, h)
    }
}

/// 逻辑尺寸 × 实际 scale → 物理几何；并把流水线同步到同一尺寸。
#[cfg(feature = "wayland")]
fn apply_wayland_geometry(
    pipeline: &mut Pipeline,
    overlay: &mut WaylandOverlay,
    logical_w: u16,
    logical_h: u16,
) -> Result<bool, String> {
    use spectrum_platform::overlay::OverlayBackend;

    let scale = overlay.scale().max(1);
    let width = u32::from(logical_w)
        .checked_mul(scale)
        .and_then(|v| u16::try_from(v).ok())
        .ok_or_else(|| format!("dock 逻辑宽度 {logical_w} × scale {scale} 溢出"))?;
    let height = u32::from(logical_h)
        .checked_mul(scale)
        .and_then(|v| u16::try_from(v).ok())
        .ok_or_else(|| format!("dock 逻辑高度 {logical_h} × scale {scale} 溢出"))?;
    let geometry = OverlayGeometry::new(0, 0, width, height);
    let current = overlay.geometry().map_err(|e| e.to_string())?;
    if geometry == current {
        return Ok(false);
    }
    overlay.set_geometry(geometry).map_err(|e| e.to_string())?;
    pipeline.resize(width, height);
    Ok(true)
}

#[cfg(feature = "wayland")]
impl Drop for WaylandDockWatcher {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::SeqCst);
    }
}

/// Treeland / Wayland 主循环：layer-shell 上屏 + dock 几何轮询。
#[cfg(feature = "wayland")]
#[allow(clippy::too_many_lines)]
fn run_loop_wayland(
    mut pipeline: Pipeline,
    geometry: OverlayGeometry,
    position: i32,
    source: &mut ManagedSource,
    session: &mut Session,
) -> Result<(), String> {
    let mut overlay = WaylandOverlay::create_with_socket(
        geometry,
        // 初始只是提示值；线程绑定 wl_output 后会回传真实 scale。
        1,
        position,
        session.above_dock,
        wayland_socket_path(),
    )
    .map_err(|e| e.to_string())?;
    println!(
        "overlay wayland {}x{} (scale {})，位置 {}，{}",
        geometry.width,
        geometry.height,
        overlay.scale(),
        position,
        if session.above_dock {
            "任务栏上方"
        } else {
            "任务栏内"
        }
    );

    let mut dock = WaylandDockWatcher::start();
    let fullscreen_native = overlay.fullscreen_flag();
    #[cfg(feature = "x11")]
    let mut fullscreen = spectrum_platform::x11::fullscreen::FullscreenMonitor::connect()
        .map_err(|e| e.to_string())
        .ok();
    let mut last_fullscreen_check = Instant::now();
    let initial_logical = (geometry.width, geometry.height);
    let mut last_scale = overlay.scale();
    let mut last_configure = overlay.configure_generation().wrapping_sub(1);
    let mut hop = vec![0.0_f32; session.hop_frames];
    let mut presented = 0_u64;
    let mut skipped = 0_u64;
    let mut cleared = 0_u64;
    let mut retry_logged = 0_u32;
    // 合成器瞬时错误（分辨率切换/热插拔）不应立刻退出 daemon。
    let mut failures = FailureBudget::new(WINDOW_FAILURE_LIMIT);
    // 跨帧复用 bands 缓冲（静音帧不再每帧分配）。
    let mut frame = FrameOutcome::default();

    loop {
        if session.expired() {
            break;
        }
        if session.control.stop_requested() {
            println!("收到停止请求，退出");
            break;
        }
        let now_ms = session.now_ms();

        match source.read(&mut hop, now_ms) {
            ReadOutcome::Filled(n) => {
                if session.should_present() && !dock.dock_hidden() {
                    pipeline.push_into(&hop[..n], now_ms, &mut frame);
                    if frame.should_present {
                        let surface = pipeline.surface();
                        match overlay.put_image(surface.pixels(), surface.width(), surface.height())
                        {
                            Ok(()) => {
                                failures.ok();
                                presented += 1;
                            }
                            Err(err) => tolerate(&mut failures, "上屏", Err(err.to_string()))?,
                        }
                    } else {
                        skipped += 1;
                    }
                } else {
                    skipped += 1;
                }
            }
            ReadOutcome::Idle => std::thread::sleep(Duration::from_millis(2)),
            ReadOutcome::Lost(why) => {
                println!("采集断流：{why} —— 画面清空，自动重连中");
                tolerate(
                    &mut failures,
                    "清屏",
                    clear_overlay_wayland(&mut pipeline, &mut overlay, now_ms),
                )?;
                cleared += 1;
                retry_logged = 0;
            }
            ReadOutcome::RetryFailed(why) => {
                retry_logged += 1;
                if retry_logged <= 2 || retry_logged.is_multiple_of(20) {
                    println!("  重连失败（第 {retry_logged} 次）：{why}");
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            ReadOutcome::Recovered {
                attempts,
                outage_ms,
            } => {
                println!("采集已恢复（重试 {attempts} 次，累计断流 {outage_ms}ms）");
                tolerate(
                    &mut failures,
                    "清屏",
                    clear_overlay_wayland(&mut pipeline, &mut overlay, now_ms),
                )?;
                cleared += 1;
            }
            ReadOutcome::Waiting => std::thread::sleep(Duration::from_millis(20)),
        }

        let geometry_changed = match dock.poll(
            &mut pipeline,
            &mut overlay,
            now_ms,
            session.hide_on_autohide,
        ) {
            Ok(changed) => changed,
            Err(err) => {
                tolerate(&mut failures, "任务栏几何更新", Err(err))?;
                false
            }
        };
        if geometry_changed {
            println!(
                "  几何变化 → {}x{}（像素缓冲已重建）",
                pipeline.config().width,
                pipeline.config().height
            );
        }

        // 实际 scale 变化：主线程必须按新倍率重建几何，否则旧物理尺寸会被
        // 线程按新 scale 再除一次，窗口会缩小。
        let scale = overlay.scale();
        if scale != last_scale {
            last_scale = scale;
            if !dock.dock_hidden() {
                match dock.reapply(&mut pipeline, &mut overlay, initial_logical) {
                    Ok(true) => {
                        println!(
                            "  Wayland scale → {scale}，像素缓冲重建为 {}x{}",
                            pipeline.config().width,
                            pipeline.config().height
                        );
                    }
                    Ok(false) => {}
                    Err(err) => tolerate(&mut failures, "Wayland 几何重建", Err(err))?,
                }
            }
        }

        // configure 可能夹紧 set_size；代际变化时按实际尺寸重建缓冲。
        let generation = overlay.configure_generation();
        if generation != last_configure {
            last_configure = generation;
            let (actual_w, actual_h) = overlay.actual_size();
            if actual_w != pipeline.config().width || actual_h != pipeline.config().height {
                pipeline.resize(actual_w, actual_h);
                println!("  configure 实际尺寸 → {actual_w}x{actual_h}（像素缓冲已重建）");
            }
        }

        match poll_config(&mut pipeline, session, now_ms) {
            Ok(true) if !session.enabled => {
                tolerate(
                    &mut failures,
                    "清屏",
                    clear_overlay_wayland(&mut pipeline, &mut overlay, now_ms),
                )?;
                cleared += 1;
            }
            Ok(_) => {}
            Err(err) => eprintln!("  警告：配置应用失败（保留旧配置）：{err}"),
        }
        // 配置关闭「自动隐藏时隐藏」后，立即恢复（不等下一次 dock 轮询）。
        if !session.hide_on_autohide && dock.dock_hidden() {
            match dock.force_show(&mut pipeline, &mut overlay, now_ms) {
                Ok(()) => {}
                Err(err) => tolerate(&mut failures, "恢复上屏", Err(err))?,
            }
        }
        // `visual.mode` 热切换：Wayland 只需改 exclusive zone（上面/里面）。
        tolerate(
            &mut failures,
            "显示位置切换",
            overlay
                .set_above_dock(session.above_dock)
                .map_err(|e| e.to_string()),
        )?;

        // 全屏/锁屏隐藏：原生 Wayland 检测 + X11 EWMH 兜底。
        if Instant::now() >= last_fullscreen_check + Duration::from_millis(500) {
            last_fullscreen_check = Instant::now();
            let fullscreen_now = {
                #[cfg(feature = "x11")]
                {
                    fullscreen_native.load(Ordering::Relaxed)
                        || fullscreen.as_mut().is_some_and(
                            spectrum_platform::x11::fullscreen::FullscreenMonitor::is_fullscreen,
                        )
                }
                #[cfg(not(feature = "x11"))]
                {
                    fullscreen_native.load(Ordering::Relaxed)
                }
            };
            #[cfg(feature = "dbus")]
            let locked_now = session.lock.is_locked();
            #[cfg(not(feature = "dbus"))]
            let locked_now = false;
            if let Some(hidden) = session.update_hide(fullscreen_now, locked_now) {
                if hidden {
                    tolerate(
                        &mut failures,
                        "清屏",
                        clear_overlay_wayland(&mut pipeline, &mut overlay, now_ms),
                    )?;
                    cleared += 1;
                    println!("全屏/锁屏 → 隐藏频谱");
                } else {
                    println!("全屏/锁屏结束 → 恢复频谱");
                    pipeline.clear(now_ms);
                }
            }
        }
    }

    let outages = source.reconnector().stats();
    println!(
        "presented={presented} skipped_silent={skipped} frames={} cleared={cleared} reconnects={} outages={} outage_ms={}",
        pipeline.stats().frames,
        source.reconnects(),
        outages.outages,
        outages.total_outage_ms
    );
    Ok(())
}

#[cfg(feature = "wayland")]
fn clear_overlay_wayland(
    pipeline: &mut Pipeline,
    overlay: &mut WaylandOverlay,
    now_ms: u64,
) -> Result<(), String> {
    let surface = pipeline.clear(now_ms);
    overlay
        .put_image(surface.pixels(), surface.width(), surface.height())
        .map_err(|e| e.to_string())
}

/// 一行摘要（仅无 X11 的构建使用）。
#[cfg(not(feature = "x11"))]
fn print_summary(outcome: &FrameOutcome) {
    let peak = outcome
        .peak_hz
        .map_or_else(|| "-".to_string(), |hz| format!("{hz:.0}Hz"));
    let max = outcome.bands.iter().copied().fold(0.0_f32, f32::max);
    println!(
        "  active={} peak={peak} max_level={max:.2} bands={}",
        outcome.active,
        outcome.bands.len()
    );
}

/// 一轮读取的结果。
///
/// 把「断流」和「没数据」分开是这套机制的核心：
/// 音乐暂停时 `read_hop` 同样返回 0 帧；若把 0 帧当断流，
/// 用户每暂停一次音乐就会触发一次重连。
enum ReadOutcome {
    /// 取到 `n` 帧 PCM。
    Filled(usize),
    /// 本轮没数据（静音 / 等超时），连接仍然健康。
    Idle,
    /// 刚发现断流（调用方应清一次屏）。
    Lost(String),
    /// 仍在断流中，重试又失败了。
    RetryFailed(String),
    /// 重连成功（调用方应清一次屏再继续）。
    Recovered { attempts: u32, outage_ms: u64 },
    /// 还在退避等待，本轮什么都没做。
    Waiting,
}

/// 带断流恢复的 PCM 来源。
///
/// ## 为什么需要它
///
/// 换输出设备、`PulseAudio` 重启、笔记本休眠唤醒，都会让采集流断掉。
/// 上一版直接 `source.read(...)?`，断流即整个 daemon 退出 ——
/// 用户看到的是"频谱突然消失"，且必须手动重启。
///
/// 现在的行为：断流 → 清屏一次 → 按退避重连 → 恢复后继续画。
/// 退避策略本身在 [`spectrum_daemon::reconnect`] 里，是纯逻辑、可单测。
struct ManagedSource {
    /// 重建采集链需要的原始参数。
    opts: DaemonArgs,
    sample_rate: u32,
    /// `None` = 断流中，等待重连。
    inner: Option<Source>,
    reconnector: Reconnector,
    reconnects: u64,
    last_error: Option<String>,
}

impl ManagedSource {
    /// 建立采集链；连不上时**有界重试**后再失败。
    ///
    /// 启动也重试的理由：随登录自启时，daemon 完全可能比 `pipewire-pulse`
    /// 早就绪。那属于"环境还没准备好"，不是配置错误，
    /// 直接退出会让用户看到一次莫名其妙的失败。
    /// 但也不能无限等 —— 有界重试后如实报错，指明是音频服务的问题。
    fn open(opts: &DaemonArgs, sample_rate: u32) -> Result<Self, String> {
        let startup_policy = ReconnectPolicy {
            first_delay_ms: 100,
            max_delay_ms: 1_000,
            factor: 2,
            max_attempts: Some(8),
        };
        let mut startup = Reconnector::new(startup_policy);
        let t0 = Instant::now();
        loop {
            match Source::open(opts, sample_rate) {
                Ok(source) => {
                    return Ok(Self {
                        opts: opts.clone(),
                        sample_rate,
                        inner: Some(source),
                        // 运行期策略与启动期不同：启动有界，运行期永不放弃
                        reconnector: Reconnector::new(ReconnectPolicy::default()),
                        reconnects: 0,
                        last_error: None,
                    });
                }
                Err(err) => {
                    let now = u64::try_from(t0.elapsed().as_millis()).unwrap_or(0);
                    match startup.on_failure(now) {
                        RetryDecision::RetryIn(delay) => {
                            eprintln!("等待音频服务就绪（{delay}ms 后重试）：{err}");
                            std::thread::sleep(Duration::from_millis(delay));
                        }
                        RetryDecision::GiveUp => {
                            return Err(format!(
                                "{err}（已尝试 {} 次、{} 秒；请确认音频服务是否在运行）",
                                startup.stats().attempts,
                                t0.elapsed().as_secs()
                            ));
                        }
                    }
                }
            }
        }
    }

    /// 读一个 hop；断流时自动进入重连流程，**不返回致命错误**。
    fn read(&mut self, out: &mut [f32], now_ms: u64) -> ReadOutcome {
        // 先把「这次读的结果」整个取出来，避免在下面对 self 的可变借用冲突
        let sampled = self.inner.as_mut().map(|source| source.read(out));
        match sampled {
            Some(Ok(0)) => ReadOutcome::Idle,
            Some(Ok(n)) => ReadOutcome::Filled(n),
            Some(Err(err)) => {
                self.last_error = Some(err.clone());
                // 先丢链（Drop 里会断开旧流），再排期重试 ——
                // 不丢的话死连接会一直留在那里，重试时又新建一条，越积越多
                self.inner = None;
                self.reconnector.on_failure(now_ms);
                ReadOutcome::Lost(err)
            }
            None => {
                if !self.reconnector.due(now_ms) {
                    return ReadOutcome::Waiting;
                }
                match Source::open(&self.opts, self.sample_rate) {
                    Ok(source) => {
                        let attempts = self.reconnector.attempt();
                        self.inner = Some(source);
                        self.reconnector.on_recovered(now_ms);
                        self.reconnects = self.reconnects.saturating_add(1);
                        ReadOutcome::Recovered {
                            attempts,
                            outage_ms: self.reconnector.stats().total_outage_ms,
                        }
                    }
                    Err(err) => {
                        self.last_error = Some(err.clone());
                        self.reconnector.on_failure(now_ms);
                        ReadOutcome::RetryFailed(err)
                    }
                }
            }
        }
    }

    /// 累计重连成功次数。
    const fn reconnects(&self) -> u64 {
        self.reconnects
    }

    /// 重连状态机（读取统计用）。
    const fn reconnector(&self) -> &Reconnector {
        &self.reconnector
    }

    /// 最近一次采集错误（断流期间的诊断信息）。
    #[allow(dead_code)]
    fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }
}

/// PCM 来源。
///
/// ## 为什么要有「节流时钟」
///
/// 真实采集天然受声卡时钟约束：`read_hop` 会阻塞到攒够一整个 hop。
/// 合成源没有这个约束，`fill` 会以 CPU 全速产帧 ——
/// 实测 4 秒产出 418 帧（≈104fps），而 48kHz/1024 帧的真实节奏是 46.9fps。
/// 后果是动画 2.2 倍速、CPU 白烧（F34）。
/// 因此合成源显式按 `hop / sample_rate` 的节奏睡眠。
struct Source {
    kind: SourceKind,
    sample_rate: u32,
    /// 下一次允许产帧的时刻（仅合成源使用）。
    next_at: Option<Instant>,
}

enum SourceKind {
    Tone(ToneSource),
    /// 装箱：`PulseCapture` 内含 mainloop/context/stream，比 `ToneSource` 大得多，
    /// 不装箱会让整个枚举按最大变体分配（clippy `large_enum_variant`）。
    #[cfg(feature = "pulse")]
    Live(Box<spectrum_core::pulse::PulseCapture>),
}

impl Source {
    fn open(opts: &DaemonArgs, sample_rate: u32) -> Result<Self, String> {
        if !opts.live {
            return Ok(Self {
                kind: SourceKind::Tone(ToneSource::new(opts.tone_hz, sample_rate)),
                sample_rate,
                next_at: None,
            });
        }
        #[cfg(feature = "pulse")]
        {
            use spectrum_core::capture::CaptureConfig;

            let capture = spectrum_core::pulse::PulseCapture::open(
                None,
                sample_rate,
                CaptureConfig::default(),
            )
            .map_err(|e| format!("打开 monitor 采集失败：{e}"))?;
            Ok(Self {
                kind: SourceKind::Live(Box::new(capture)),
                sample_rate,
                next_at: None,
            })
        }
        #[cfg(not(feature = "pulse"))]
        Err("本次构建未启用 pulse feature，无法 --live".to_string())
    }

    /// 读一整个 hop；返回实际填充的帧数（0 = 本次没有数据）。
    ///
    /// 返回 `Result` 是为了**跨 feature 组合保持同一签名**：
    /// 关闭 `pulse` 时只有合成源分支，确实不可能失败，
    /// 但按 feature 改签名会让调用点变成 cfg 泥潭，不如在这里 allow。
    #[allow(clippy::unnecessary_wraps)]
    fn read(&mut self, out: &mut [f32]) -> Result<usize, String> {
        match &mut self.kind {
            SourceKind::Tone(tone) => {
                tone.fill(out);
                self.pace(out.len());
                Ok(out.len())
            }
            #[cfg(feature = "pulse")]
            SourceKind::Live(capture) => capture
                .read_hop(out, Duration::from_millis(50))
                .map_err(|e| e.to_string()),
        }
    }

    /// 合成源按真实时钟节流到 `hop / sample_rate`。
    fn pace(&mut self, frames: usize) {
        let hop_ns = u128::from(u64::try_from(frames).unwrap_or(0)) * 1_000_000_000
            / u128::from(self.sample_rate.max(1));
        let hop = Duration::from_nanos(u64::try_from(hop_ns).unwrap_or(0));
        let now = Instant::now();
        let next = match self.next_at {
            Some(prev) => prev + hop,
            None => now + hop,
        };
        if next > now {
            std::thread::sleep(next - now);
            self.next_at = Some(next);
        } else if now - next > hop * 4 {
            // 落后太多（例如调试断点）就不要追帧了，直接重定基准，
            // 否则会连续快跑好几帧，看起来像"快进"
            self.next_at = Some(now + hop);
        } else {
            self.next_at = Some(next);
        }
    }
}

/// 合成纯音源。
///
/// 相位**跨 hop 连续**：如果每个 hop 都从 `sin(0)` 重新开始，
/// 频谱上会在 hop 边界出现宽频冲击（听感上是咔哒声）——
/// 那不是频谱算法的错，是信号本身不连续。
///
/// 用相位累加器而不是循环播放 1 秒缓冲：440.5Hz 这类非整数周期频率
/// 在缓冲回绕处会跳相位，频谱上表现为周期性的宽频毛刺。
struct ToneSource {
    phase: f32,
    step: f32,
}

impl ToneSource {
    #[allow(clippy::cast_precision_loss)]
    fn new(freq_hz: f32, sample_rate: u32) -> Self {
        let rate = sample_rate.max(1);
        Self {
            phase: 0.0,
            step: std::f32::consts::TAU * freq_hz / rate as f32,
        }
    }

    fn fill(&mut self, out: &mut [f32]) {
        for slot in out.iter_mut() {
            *slot = self.phase.sin();
            self.phase = (self.phase + self.step) % std::f32::consts::TAU;
        }
    }
}

/// 启动时的任务栏几何。
///
/// 降级链：D-Bus `FrontendWindowRect` → 环境变量 → `dock.height` +
/// X11 屏幕宽度 → 内置默认值。任何一步失败都只记录并继续，**不再让 daemon 退出**。
#[cfg(feature = "dbus")]
fn initial_geometry(fallback_height: u16) -> OverlayGeometry {
    use spectrum_core::dock_signal::{DockOutcome, DockSignalBridge, RectSource};
    use spectrum_platform::dock_geom::DockSubscription;

    let mut sub = match DockSubscription::start(RectSource::Physical) {
        Ok(sub) => sub,
        Err(err) => {
            eprintln!("  几何订阅失败（{err}），改用兜底几何");
            return fallback_geometry(fallback_height);
        }
    };
    let mut bridge = DockSignalBridge::new(RectSource::Physical);
    let deadline = Instant::now() + Duration::from_millis(1_500);
    while Instant::now() < deadline {
        match sub.try_recv() {
            Ok(Some(event)) => {
                // ★ 两步都要：`submit` 只是把观测塞进节流窗口（通常返回 Pending），
                // 必须再 `tick` 一个跨越窗口的时刻才会产出 `Commit`。
                // 只 submit 不 tick → 永远拿不到首值（本轮实测踩到过）。
                let now = u64::try_from(
                    deadline
                        .saturating_duration_since(Instant::now())
                        .as_millis(),
                )
                .unwrap_or(0);
                let submitted = bridge.submit(event, now);
                let outcome = match submitted {
                    DockOutcome::Commit(_) => submitted,
                    _ => bridge.tick(now.saturating_add(32)),
                };
                if let DockOutcome::Commit(rect) = outcome
                    && let Ok(geometry) = OverlayGeometry::try_from_rect(rect)
                {
                    return geometry;
                }
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(err) => {
                eprintln!("  几何订阅中断（{err}），改用兜底几何");
                return fallback_geometry(fallback_height);
            }
        }
    }
    eprintln!("  1500ms 内未取得任务栏几何，改用兜底几何");
    fallback_geometry(fallback_height)
}

/// 无 dbus 时的几何：环境变量 + `dock.height` 兜底。
#[cfg(not(feature = "dbus"))]
fn initial_geometry(fallback_height: u16) -> OverlayGeometry {
    fallback_geometry(fallback_height)
}

/// 环境变量优先；否则用 X11 屏幕宽度 + 配置的 `dock.height`。
fn fallback_geometry(fallback_height: u16) -> OverlayGeometry {
    #[cfg(feature = "x11")]
    let screen_width = spectrum_platform::x11::screen_size().map(|(width, _)| width);
    #[cfg(not(feature = "x11"))]
    let screen_width = None;

    let width = std::env::var("SPECTRUM_WIDTH")
        .ok()
        .and_then(|v| v.parse::<u16>().ok())
        .or(screen_width)
        .unwrap_or(3200);
    let height = std::env::var("SPECTRUM_HEIGHT")
        .ok()
        .and_then(|v| v.parse::<u16>().ok())
        .unwrap_or(fallback_height.max(1));
    OverlayGeometry::new(0, 0, width.max(1), height.max(1))
}

/// 任务栏几何跟随（订阅 + 节流桥）。
#[cfg(feature = "x11")]
#[cfg_attr(not(feature = "dbus"), allow(dead_code))]
struct DockWatcher {
    /// D-Bus 订阅句柄；没有 `dbus` feature 时为静态几何，不跟随。
    #[cfg(feature = "dbus")]
    subscription: Option<spectrum_platform::dock_geom::DockSubscription>,
    bridge: spectrum_core::dock_signal::DockSignalBridge,
    /// 最近一次收到的原始任务栏矩形（未按模式偏移）。
    last: Option<OverlayGeometry>,
    /// 上次应用时使用的模式（用于热切换重新应用）。
    last_above: Option<bool>,
    /// 任务栏停靠位置：0=top / 1=right / 2=bottom / 3=left。
    position: i32,
    /// 订阅失效后的下一次重建时间（毫秒，`None` = 无需重建）。
    #[cfg_attr(not(feature = "dbus"), allow(dead_code))]
    retry_at_ms: Option<u64>,
    /// 上次主动回读权威矩形的时间（节流 `NeedsRefresh` 的阻塞 D-Bus 往返）。
    #[cfg_attr(not(feature = "dbus"), allow(dead_code))]
    last_refresh_ms: Option<u64>,
    /// 连续重建失败的次数（达到上限后放弃自动重建，避免无限起线程）。
    #[cfg_attr(not(feature = "dbus"), allow(dead_code))]
    start_failures: u32,
}

/// 几何订阅失效后，隔多久重建一次（避免每个渲染帧都重连 D-Bus）。
#[cfg(all(feature = "x11", feature = "dbus"))]
const DOCK_RESUBSCRIBE_MS: u64 = 2_000;

/// 连续重建失败多少次后放弃（每次重建最多起 3 个线程，必须设上限）。
#[cfg(all(feature = "x11", feature = "dbus"))]
const DOCK_MAX_REBUILDS: u32 = 5;

/// `NeedsRefresh` 回读的最小间隔：回读是阻塞 D-Bus 调用，不能每帧都做。
#[cfg(all(feature = "x11", feature = "dbus"))]
const DOCK_REFRESH_MS: u64 = 2_000;

#[cfg(all(feature = "x11", feature = "dbus"))]
impl DockWatcher {
    fn start() -> Self {
        use spectrum_core::dock_signal::{DockSignalBridge, RectSource};
        use spectrum_platform::dock_geom::DockSubscription;

        let position = spectrum_platform::dock_geom::read_position().unwrap_or(2);
        match DockSubscription::start(RectSource::Physical) {
            Ok(sub) => Self {
                subscription: Some(sub),
                bridge: DockSignalBridge::new(RectSource::Physical),
                last: None,
                last_above: None,
                position,
                retry_at_ms: None,
                last_refresh_ms: None,
                start_failures: 0,
            },
            Err(err) => {
                eprintln!("  警告：几何订阅失败（{err}），本次不跟随高度变化");
                Self {
                    subscription: None,
                    bridge: DockSignalBridge::new(RectSource::Physical),
                    last: None,
                    last_above: None,
                    position,
                    // 启动失败也按同样的节流重试：任务栏服务可能比频谱晚就绪。
                    retry_at_ms: Some(0),
                    last_refresh_ms: None,
                    start_failures: 0,
                }
            }
        }
    }

    /// 检查订阅健康度；失效时按节流重建。
    ///
    /// 三路订阅线程中任意一路死亡时，另外两路仍持有 sender，
    /// `rx.try_recv()` 不会返回 `Disconnected` —— 只看 channel 会把
    /// "几何跟随静默降级"当成正常运行。这里同时看 `is_alive` 和
    /// `try_recv_failure`，并在重建失败后持续重试。
    fn ensure_subscription(&mut self, now_ms: u64) {
        use spectrum_platform::dock_geom::DockSubscription;

        let mut rebuild = false;
        if let Some(sub) = self.subscription.as_ref() {
            if let Some(err) = sub.try_recv_failure() {
                eprintln!("  警告：几何订阅通道失效（{err}），准备重建");
                rebuild = true;
            } else if !sub.is_alive() {
                eprintln!("  警告：几何订阅线程已退出，准备重建");
                rebuild = true;
            }
        }
        if rebuild {
            self.subscription = None;
            self.retry_at_ms = Some(now_ms.saturating_add(DOCK_RESUBSCRIBE_MS));
        }
        if self.subscription.is_none() && self.retry_at_ms.is_some_and(|at| now_ms >= at) {
            match DockSubscription::start(spectrum_core::dock_signal::RectSource::Physical) {
                Ok(sub) => {
                    self.subscription = Some(sub);
                    self.retry_at_ms = None;
                    self.start_failures = 0;
                    println!("几何订阅已恢复");
                }
                Err(err) => {
                    self.start_failures = self.start_failures.saturating_add(1);
                    if self.start_failures >= DOCK_MAX_REBUILDS {
                        // 每次重建会起 3 个订阅线程；长期失败必须停下来止损。
                        eprintln!(
                            "  警告：几何订阅重建连续失败 {} 次（{err}），停止自动重试，几何固定",
                            self.start_failures
                        );
                        self.retry_at_ms = None;
                    } else {
                        eprintln!(
                            "  警告：几何订阅重建失败（{err}），{DOCK_RESUBSCRIBE_MS}ms 后重试"
                        );
                        self.retry_at_ms = Some(now_ms.saturating_add(DOCK_RESUBSCRIBE_MS));
                    }
                }
            }
        }
    }

    /// 非阻塞处理几何事件；返回是否发生过尺寸/位置变化。
    fn poll(
        &mut self,
        pipeline: &mut Pipeline,
        overlay: &mut spectrum_platform::x11::X11Overlay,
        above_dock: bool,
        now_ms: u64,
    ) -> Result<bool, String> {
        use spectrum_core::dock_signal::DockOutcome;

        self.ensure_subscription(now_ms);
        let Some(sub) = self.subscription.as_mut() else {
            return Ok(false);
        };
        let mut changed = false;
        loop {
            match sub.try_recv() {
                Ok(Some(event)) => {
                    if let spectrum_core::dock_signal::DockEvent::PositionChanged(pos) = event {
                        self.position = pos;
                    }
                    match self.bridge.submit(event, now_ms) {
                        DockOutcome::Commit(rect) => {
                            if let Ok(geometry) = OverlayGeometry::try_from_rect(rect) {
                                self.last = Some(geometry);
                                self.last_above = Some(above_dock);
                                changed |= apply_if_changed(
                                    overlay,
                                    pipeline,
                                    geometry,
                                    above_dock,
                                    self.position,
                                )?;
                            }
                        }
                        DockOutcome::Relayout(_) => {
                            if let Some(last) = self.last {
                                self.last_above = Some(above_dock);
                                changed |= apply_if_changed(
                                    overlay,
                                    pipeline,
                                    last,
                                    above_dock,
                                    self.position,
                                )?;
                            }
                        }
                        _ => {}
                    }
                }
                Ok(None) => break,
                Err(err) => {
                    eprintln!("  警告：几何订阅中断（{err}），停止跟随");
                    self.subscription = None;
                    break;
                }
            }
        }
        // 节流窗口到点也要提交（拖动过程中的合并靠它）
        let mut outcome = self.bridge.tick(now_ms);
        // 尺寸模式变化（例如「任务栏尺寸」切成图标模式）不会带来矩形变更信号，
        // 必须主动回读权威矩形，否则会一直用旧几何。回读是阻塞调用，做节流。
        if matches!(outcome, DockOutcome::NeedsRefresh)
            && self
                .last_refresh_ms
                .is_none_or(|at| now_ms.saturating_sub(at) >= DOCK_REFRESH_MS)
        {
            self.last_refresh_ms = Some(now_ms);
            if let Some(event) = spectrum_platform::dock_geom::read_rect(
                spectrum_core::dock_signal::RectSource::Physical,
            ) {
                outcome = self.bridge.submit(event, now_ms);
            }
        }
        if let DockOutcome::Commit(rect) = outcome
            && let Ok(geometry) = OverlayGeometry::try_from_rect(rect)
        {
            self.last = Some(geometry);
            self.last_above = Some(above_dock);
            changed |= apply_if_changed(overlay, pipeline, geometry, above_dock, self.position)?;
        }
        // 配置热切换 `visual.mode`：没有新的 dock 事件也要重新应用一次。
        if let Some(last) = self.last
            && self.last_above != Some(above_dock)
        {
            changed |= apply_if_changed(overlay, pipeline, last, above_dock, self.position)?;
            self.last_above = Some(above_dock);
        }
        Ok(changed)
    }
}

#[cfg(all(feature = "x11", not(feature = "dbus")))]
impl DockWatcher {
    fn start() -> Self {
        Self {
            bridge: spectrum_core::dock_signal::DockSignalBridge::new(
                spectrum_core::dock_signal::RectSource::Physical,
            ),
            last: None,
            last_above: None,
            position: 2,
            retry_at_ms: None,
            last_refresh_ms: None,
            start_failures: 0,
        }
    }

    fn poll(
        &mut self,
        _pipeline: &mut Pipeline,
        _overlay: &mut spectrum_platform::x11::X11Overlay,
        _above_dock: bool,
        _now_ms: u64,
    ) -> Result<bool, String> {
        Ok(false)
    }
}

/// 任务栏上方模式的几何：按停靠方向把窗口整体移到任务栏外侧。
///
/// - 底部 (2)：`y -= height`
/// - 顶部 (0)：`y += height`
/// - 右侧 (1)：`x -= width`
/// - 左侧 (3)：`x += width`
///
/// 侧边任务栏的柱状方向（横条 vs 竖条）是独立设计项，这里只保证窗口位置正确。
#[cfg(feature = "x11")]
fn above_geometry(geometry: OverlayGeometry, position: i32) -> OverlayGeometry {
    let width = i16::try_from(geometry.width).unwrap_or(i16::MAX);
    let height = i16::try_from(geometry.height).unwrap_or(i16::MAX);
    let (x, y) = match position {
        0 => (geometry.x, geometry.y.saturating_add(height)),
        1 => (geometry.x.saturating_sub(width), geometry.y),
        3 => (geometry.x.saturating_add(width), geometry.y),
        _ => (geometry.x, geometry.y.saturating_sub(height)),
    };
    OverlayGeometry::new(x, y, geometry.width, geometry.height)
}

/// 只有几何真的变了才应用 + 报告。
///
/// 订阅建立后一定会先送来一次当前矩形（首值）；如果不比对就应用，
/// 每次启动都会打印一行"几何变化"，看着像出问题了，实际什么都没变。
#[cfg(all(feature = "x11", feature = "dbus"))]
fn apply_if_changed(
    overlay: &mut spectrum_platform::x11::X11Overlay,
    pipeline: &mut Pipeline,
    geometry: OverlayGeometry,
    above_dock: bool,
    position: i32,
) -> Result<bool, String> {
    use spectrum_platform::overlay::OverlayBackend;

    let target = if above_dock {
        above_geometry(geometry, position)
    } else {
        geometry
    };
    let current = overlay.geometry().map_err(|e| e.to_string())?;
    if target == current {
        return Ok(false);
    }
    overlay.set_geometry(target).map_err(|e| e.to_string())?;
    pipeline.resize(target.width, target.height);
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::{DaemonArgs, presentation_allowed};
    use std::time::Duration;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn duration_rejects_non_finite_and_oversized_values() {
        for bad in ["inf", "-inf", "NaN", "604801", "1e300"] {
            let mut argv = args(&["--duration"]);
            argv.push(bad.to_string());
            assert!(
                DaemonArgs::parse(&argv).is_err(),
                "--duration {bad} 应返回可读错误而不是 panic"
            );
        }
        assert!(
            DaemonArgs::parse(&args(&["--duration", "1e300"])).is_err(),
            "超大有限值同样必须拒绝"
        );
        // 合法值照常接受；负数按最小 0.1s 处理（原有行为）。
        let ok = DaemonArgs::parse(&args(&["--duration", "5"])).unwrap();
        assert_eq!(ok.duration, Some(Duration::from_secs(5)));
        let negative = DaemonArgs::parse(&args(&["--duration", "-3"])).unwrap();
        assert_eq!(negative.duration, Some(Duration::from_secs_f64(0.1)));
    }

    #[test]
    fn synthetic_rejects_non_finite_and_out_of_range() {
        for bad in ["inf", "-inf", "NaN", "0", "-20", "100000"] {
            let mut argv = args(&["--synthetic"]);
            argv.push(bad.to_string());
            assert!(
                DaemonArgs::parse(&argv).is_err(),
                "--synthetic {bad} 应被拒绝"
            );
        }
        let ok = DaemonArgs::parse(&args(&["--synthetic", "440"])).unwrap();
        assert!((ok.tone_hz - 440.0).abs() < f32::EPSILON);
    }

    #[test]
    fn hidden_by_fullscreen_blocks_every_frame() {
        assert!(presentation_allowed(true, false, false));
        assert!(
            !presentation_allowed(true, true, false),
            "全屏隐藏期间必须持续阻止上屏，否则下一帧会立刻画回来"
        );
        assert!(!presentation_allowed(true, false, true), "锁屏隐藏同理");
        assert!(!presentation_allowed(false, false, false));
        assert!(!presentation_allowed(false, true, true));
    }

    #[cfg(any(feature = "x11", feature = "wayland"))]
    #[test]
    fn failure_budget_trips_only_after_limit() {
        use super::{FailureBudget, tolerate};

        let mut budget = FailureBudget::new(3);
        assert!(!budget.fail("上屏", "e1"));
        // 无关操作成功不清零：只有上屏成功才算窗口系统健康。
        tolerate(&mut budget, "几何", Ok(())).unwrap();
        assert!(!budget.fail("上屏", "e2"), "成功调用不应清零计数");
        assert!(budget.fail("上屏", "e3"), "连续失败达到上限才退出");
        // 一次真正的上屏成功重新开始计数。
        budget.ok();
        assert!(!budget.fail("上屏", "e4"));
    }

    #[test]
    #[allow(clippy::cast_precision_loss)]
    fn tone_source_phase_is_continuous_across_hops() {
        use super::ToneSource;

        let rate = 48_000_u32;
        let hz = 440.5_f32;
        let mut tone = ToneSource::new(hz, rate);
        let mut first = vec![0.0_f32; 1024];
        let mut second = vec![0.0_f32; 1024];
        tone.fill(&mut first);
        tone.fill(&mut second);

        assert!(first[0].abs() < 1e-6, "首个样本应为 sin(0)");
        let step = std::f32::consts::TAU * hz / rate as f32;
        let expected = (step * 1024.0).sin();
        assert!(
            (second[0] - expected).abs() < 1e-3,
            "hop 边界相位应连续：got {}, expected {expected}",
            second[0]
        );
    }

    #[cfg(feature = "wayland")]
    #[test]
    fn dock_debounce_only_emits_stable_value() {
        use super::DockDebounce;

        let mut debounce = DockDebounce::default();
        let mut emitted = Vec::new();
        // 模拟审计日志里的自动隐藏动画：每 100ms 一个中间值。
        for (index, height) in [12_u16, 2, 104, 106, 2, 76, 106].iter().enumerate() {
            let now = u64::try_from(index).unwrap_or(0) * 100;
            debounce.offer((1829, *height, 2), now);
            if let Some(value) = debounce.ready(now) {
                emitted.push(value);
            }
        }
        assert!(emitted.is_empty(), "动画期间不应提交：{emitted:?}");

        // 最后一个 106 出现在 600ms；稳定 250ms 后才提交一次。
        assert_eq!(debounce.ready(849), None, "未到稳定窗口");
        assert_eq!(debounce.ready(850), Some((1829, 106, 2)));
        // 线程会持续 offer 同一个快照；已提交的值不能被重复提交。
        debounce.offer((1829, 106, 2), 900);
        assert_eq!(debounce.ready(1400), None, "同一个值只提交一次");
    }

    #[cfg(feature = "x11")]
    #[test]
    fn above_geometry_offsets_all_dock_positions() {
        use super::above_geometry;
        use spectrum_platform::overlay::OverlayGeometry;

        let bottom = OverlayGeometry::new(0, 1000, 1920, 40);
        assert_eq!(above_geometry(bottom, 2).y, 960, "底部任务栏应向上让位");
        assert_eq!(above_geometry(bottom, 0).y, 1040, "顶部任务栏应向下让位");

        let side = OverlayGeometry::new(0, 0, 40, 1080);
        assert_eq!(above_geometry(side, 1).x, -40, "右侧任务栏应向左边让位");
        assert_eq!(above_geometry(side, 3).x, 40, "左侧任务栏应向右让位");
    }
}
