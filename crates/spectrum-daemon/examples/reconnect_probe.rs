//! 断流恢复实证探针（A 阶段 R1~R7 + B 阶段观察项）。
//!
//! ## 为什么不直接重启用户的音频服务
//!
//! 那样最真实，但会打断用户正在听的音乐，而且影响整个会话（其他应用一起断）。
//! 因此这里起一个**完全私有**的 `PulseAudio` 实例：
//!
//! ```text
//! pulseaudio -n --load="module-null-sink ..." \
//!            --load="module-native-protocol-unix socket=<私有目录>/native auth-anonymous=1"
//! ```
//!
//! 采集端用 [`PulseCapture::open_on`] 连到这个私有 socket，
//! 然后**直接 kill 掉服务端** —— 对 libpulse 来说就是"服务端消失"，
//! 与用户重启 `pipewire-pulse`、休眠唤醒、拔声卡是同一类事件
//! （context 变 `Terminated`、socket EOF）。
//!
//! 用户的会话服务全程未受影响；B 阶段才碰会话，且只增删自己建的虚拟声卡。
//!
//! ## 断言
//!
//! | 编号 | 断言 |
//! |---|---|
//! | R1 | 私有服务上采集链就绪 |
//! | R2 | **静音不算断流**（0 帧只是没数据） |
//! | R3 | 服务端被 kill 后 `read_hop` **返回错误**，而不是永远返回 0 帧 |
//! | R4 | 该错误被归类为 `StreamDead`（可判定的断流，而非普通 peek 失败） |
//! | R5 | 服务端不在时重连失败，退避按 200→400→800 增长 |
//! | R6 | 服务端回来后重连成功，`health()` 回到 `Ready` |
//! | R7 | 恢复后能继续读（采集链真的可用） |

// 探针把「起服务 → 采集 → kill → 重连 → 观察」串在一个 main 里，
// 拆开会把状态（pid/socket/统计）散到各处，反而更难读。
#![allow(clippy::too_many_lines)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use spectrum_core::capture::CaptureConfig;
use spectrum_core::pulse::{PulseCapture, PulseError, PulseHealth};
use spectrum_daemon::reconnect::{ReconnectPolicy, Reconnector, RetryDecision};

const RATE: u32 = 48_000;
const HOP: usize = 1024;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut notes: Vec<String> = Vec::new();

    // 私有运行时目录：带进程号，避免与其他运行冲突。
    //
    // ★ 必须用**短**路径：`Unix socket` 上限约 108 字节（`SUN_LEN`），
    // 而本项目路径本身就有几十个汉字。第一次跑用 `temp_dir()`（= 项目内 TMPDIR）
    // 直接超限，pulseaudio 静默起不来（stderr 又被丢弃），
    // 只看到"5s 内未创建 socket"这种毫无线索的报错。
    let dir = private_dir();
    std::fs::create_dir_all(&dir)?;
    let socket = dir.join("native");
    if socket.as_os_str().len() >= 100 {
        return Err(format!(
            "私有 socket 路径过长（{} 字节，上限约 100）：{}",
            socket.as_os_str().len(),
            socket.display()
        )
        .into());
    }
    let server = format!("unix:{}", socket.display());

    // ---------- A 阶段：私有服务 ----------
    let mut server_a = PrivateServer(spawn_private_pulseaudio(&dir, &socket, true)?);
    notes.push(format!("私有 pulseaudio pid={}", server_a.pid()));
    if !wait_socket(&socket, Duration::from_secs(5)) {
        let log = std::fs::read_to_string(dir.join("pa.log")).unwrap_or_default();
        return Err(format!("私有 pulseaudio 未在 5s 内创建 socket；日志：{log}").into());
    }
    println!("私有音频服务就绪：{server}");

    let mut capture = PulseCapture::open_on(Some(&server), None, RATE, CaptureConfig::default())?;
    let r1 = capture.health() == PulseHealth::Ready;
    println!("R1 私有服务上就绪: {} ({:?})", mark(r1), capture.health());

    // ---------- R2 静音不算断流 ----------
    // 虚拟 sink 上没有播放流，monitor 只产出静音；这正是"音乐暂停"的形态。
    let mut hop = vec![0.0_f32; HOP];
    let mut frames = 0_usize;
    let mut reads = 0_u32;
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_millis(300) {
        frames += capture.read_hop(&mut hop, Duration::from_millis(20))?;
        reads += 1;
    }
    let r2 = capture.health() == PulseHealth::Ready;
    println!(
        "R2 静音 300ms 后仍健康: {}（{reads} 次读、{frames} 帧，health={:?}）",
        mark(r2),
        capture.health()
    );

    // ---------- R3/R4 杀掉服务端 ----------
    server_a.kill_now();
    notes.push("已 kill 私有 pulseaudio（模拟服务端消失）".to_string());

    let mut dead_err: Option<PulseError> = None;
    let mut silent_ok = 0_u32;
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        match capture.read_hop(&mut hop, Duration::from_millis(20)) {
            Ok(_) => silent_ok += 1,
            Err(e) => {
                dead_err = Some(e);
                break;
            }
        }
    }
    let r3 = dead_err.is_some();
    println!(
        "R3 断流被 read_hop 报出: {}（此前还返回了 {silent_ok} 次 Ok）",
        mark(r3)
    );
    let r4 = matches!(dead_err, Some(PulseError::StreamDead(_)));
    let dead_detail = match &dead_err {
        Some(e) => e.to_string(),
        None => "未检出（仍在静默返回 Ok）".to_string(),
    };
    println!("R4 归类为 StreamDead: {} —— {dead_detail}", mark(r4));
    let health_after_kill = format!("{:?}", capture.health());
    println!("   kill 后 health = {health_after_kill}");
    drop(capture);

    // ---------- R5 服务端不在时的重连：失败 + 退避 ----------
    let mut rc = Reconnector::new(ReconnectPolicy::default());
    let mut delays = Vec::new();
    let mut failures = 0_u32;
    let base = Instant::now();
    for _ in 0..3 {
        match PulseCapture::open_on(Some(&server), None, RATE, CaptureConfig::default()) {
            Ok(_) => {
                notes.push("服务端已 kill 却仍能连上（不符合预期）".to_string());
                break;
            }
            Err(err) => {
                failures += 1;
                notes.push(format!("重连失败：{err}"));
                let now = u64::try_from(base.elapsed().as_millis()).unwrap_or(0);
                match rc.on_failure(now) {
                    RetryDecision::RetryIn(d) => {
                        delays.push(d);
                        std::thread::sleep(Duration::from_millis(d));
                    }
                    RetryDecision::GiveUp => break,
                }
            }
        }
    }
    let r5 = failures == 3 && delays == vec![200, 400, 800];
    println!(
        "R5 服务端缺失时重连失败 3 次、退避 {delays:?}: {}",
        mark(r5)
    );

    // ---------- R6 服务端回来 ----------
    // 清掉被 kill 的实例留下的僵尸 socket：否则 `socket.exists()` 立刻为真，
    // 会把"文件在"误当成"服务在"（M52）
    let _ = std::fs::remove_file(&socket);
    let server_b = PrivateServer(spawn_private_pulseaudio(&dir, &socket, true)?);
    notes.push(format!("私有 pulseaudio 重启 pid={}", server_b.pid()));

    // ★ 不能只看 socket 文件就开连：服务端刚 bind 完、还没装好访问控制时，
    // 连接会被拒（实测报 `Access denied`）。正确做法是**轮询到真正可用**，
    // 这也正是生产里重连循环在做的事。
    let mut recovered = None;
    let mut last_err = String::new();
    let ready_deadline = Instant::now() + Duration::from_secs(6);
    while Instant::now() < ready_deadline {
        match PulseCapture::open_on(Some(&server), None, RATE, CaptureConfig::default()) {
            Ok(capture) => {
                recovered = Some(capture);
                break;
            }
            Err(err) => {
                last_err = err.to_string();
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
    let mut recovered =
        recovered.ok_or_else(|| format!("服务端重启后 6s 内仍连不上：{last_err}"))?;
    let now = u64::try_from(base.elapsed().as_millis()).unwrap_or(0);
    let attempts = rc.attempt();
    rc.on_recovered(now);
    let r6 = recovered.health() == PulseHealth::Ready;
    println!(
        "R6 服务端恢复后重连成功: {}（此前失败 {attempts} 次，断流 {}ms）",
        mark(r6),
        rc.stats().total_outage_ms
    );

    // ---------- R7 恢复后真的能读 ----------
    let mut ok_reads = 0_u32;
    let mut read_err = String::new();
    let t1 = Instant::now();
    while t1.elapsed() < Duration::from_millis(300) {
        match recovered.read_hop(&mut hop, Duration::from_millis(20)) {
            Ok(_) => ok_reads += 1,
            Err(e) => {
                read_err = e.to_string();
                break;
            }
        }
    }
    let r7 = ok_reads > 0 && read_err.is_empty();
    println!("R7 恢复后可继续读: {}（{ok_reads} 次）", mark(r7));

    // ---------- B 阶段：会话侧行为观察（不计入 verdict）----------
    let observed = observe_device_removal(&mut notes);
    println!("B 会话侧观察（不计入判定）：{observed}");

    // 清理（不杀掉用户的会话服务，只清理自己起的私有实例）
    drop(recovered);
    drop(server_b); // 守卫里 kill + wait
    let _ = std::fs::remove_file(&socket);

    let verdict = if r1 && r2 && r3 && r4 && r5 && r6 && r7 {
        "PASS"
    } else {
        "FAIL"
    };
    println!("\nverdict = {verdict}");

    let server_esc = esc(&server);
    let health_esc = esc(&health_after_kill);
    let dead_esc = esc(&dead_detail);
    let observed_esc = esc(&observed);
    let notes_json = notes
        .iter()
        .map(|n| format!("\"{}\"", esc(n)))
        .collect::<Vec<_>>()
        .join(", ");

    let json = format!(
        r#"{{
  "verdict": "{verdict}",
  "scope": "私有 pulseaudio 实例（kill + 重启），未触碰用户会话服务",
  "server": "{server_esc}",
  "checks": {{
    "R1_private_ready": {r1},
    "R2_silence_is_not_outage": {r2},
    "R3_death_detected_by_read": {r3},
    "R4_classified_as_stream_dead": {r4},
    "R5_retry_fails_with_backoff": {r5},
    "R6_recovered_after_restart": {r6},
    "R7_readable_after_recovery": {r7}
  }},
  "detail": {{
    "health_after_kill": "{health_esc}",
    "read_hop_error": "{dead_esc}",
    "ok_before_error": {silent_ok},
    "backoff_delays_ms": {delays:?},
    "reconnect_attempts": {attempts},
    "outage_ms": {outage},
    "silence_frames_read": {frames}
  }},
  "session_observation": "{observed_esc}",
  "notes": [{notes_json}]
}}
"#,
        outage = rc.stats().total_outage_ms,
    );
    std::fs::write("docs/spike-reconnect.json", json)?;
    println!("证据已写入 docs/spike-reconnect.json");
    Ok(())
}

/// 选一个**足够短**的私有运行时目录。
///
/// 首选 `/var/tmp`（几乎所有发行版都有、且路径短），
/// 不可写时退回系统临时目录（此时要自己检查路径长度）。
fn private_dir() -> PathBuf {
    let pid = std::process::id();
    let short = Path::new("/var/tmp").join(format!("spa-{pid}"));
    if short.parent().is_some_and(Path::is_dir) {
        return short;
    }
    std::env::temp_dir().join(format!("spa-{pid}"))
}

/// 私有音频服务的**守卫**：离开作用域一定被 kill。
///
/// `Child` 在 `Drop` 时**不会**杀进程 —— 探针任何一处提前 `return Err`
/// 都会留下一个跑着的 `pulseaudio`（实测漏过一次，还顺带留下虚拟声卡）。
/// 对用户机器来说这是不可接受的副作用，所以用守卫兜住所有失败路径。
struct PrivateServer(Child);

impl PrivateServer {
    fn pid(&self) -> u32 {
        self.0.id()
    }

    /// 立刻杀掉（模拟"服务端消失"）。
    fn kill_now(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Drop for PrivateServer {
    fn drop(&mut self) {
        // 已经 kill 过的会返回错误，忽略即可
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// JSON 字符串转义。
///
/// ★ 必须做：`health_after_kill` 里带着 `Debug` 输出的双引号
/// （`Dead("context=Failed stream=Failed")`），直接插进 `format!` 会生成
/// **非法 JSON** —— 文件看着像 JSON，`json.load` 直接报错。
/// 第一次就是这么踩的：探针自己打印一切正常，是最后的独立校验才发现。
fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' | '\r' | '\t' => out.push(' '),
            c if (c as u32) < 0x20 => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

fn mark(ok: bool) -> &'static str {
    if ok { "PASS" } else { "FAIL" }
}

/// 起一个私有 `PulseAudio`：只加载 null-sink 与 unix socket 协议模块。
///
/// `-n` = 不读默认配置，因此必须显式加载协议模块，否则不会创建 socket
/// （第一次试就踩到了：socket 文件根本不存在）。
fn spawn_private_pulseaudio(
    dir: &Path,
    socket: &Path,
    log_to_file: bool,
) -> Result<Child, Box<dyn std::error::Error>> {
    let args = [
        "-n".to_string(),
        "--load=module-null-sink sink_name=priv rate=48000".to_string(),
        format!(
            "--load=module-native-protocol-unix socket={} auth-anonymous=1",
            socket.display()
        ),
        "--exit-idle-time=-1".to_string(),
    ];
    // 首次启动把 stderr 写文件：起不来时能直接看到原因，
    // 而不是只得到"5s 内未创建 socket"这种没有信息量的结论
    let stderr = if log_to_file {
        Stdio::from(std::fs::File::create(dir.join("pa.log"))?)
    } else {
        Stdio::null()
    };
    let child = Command::new("pulseaudio")
        .args(&args)
        // XDG_RUNTIME_DIR 指向私有目录：避免碰到会话运行时目录
        .env("XDG_RUNTIME_DIR", dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr)
        .spawn()?;
    Ok(child)
}

/// 等 socket 文件出现。
fn wait_socket(path: &Path, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if path.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    path.exists()
}

/// 会话侧观察：卸载一个虚拟声卡时，已存在的采集流会不会断。
///
/// 返回人类可读的观察结论（**不计入 verdict**：这是环境行为，不是我们的代码质量）。
fn observe_device_removal(notes: &mut Vec<String>) -> String {
    const NAME: &str = "spectrum-rt-observe";

    let loaded = Command::new("pactl")
        .args([
            "load-module",
            "module-null-sink",
            &format!("sink_name={NAME}"),
        ])
        .output();
    let module = match loaded {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim().to_string(),
        Ok(out) => {
            return format!(
                "无法创建虚拟声卡（{}）",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Err(e) => return format!("pactl 不可用（{e}）"),
    };

    let device = format!("{NAME}.monitor");
    let mut capture = match PulseCapture::open(Some(&device), RATE, CaptureConfig::default()) {
        Ok(c) => c,
        Err(e) => {
            let _ = Command::new("pactl")
                .args(["unload-module", &module])
                .output();
            return format!("无法采集该虚拟声卡（{e}）");
        }
    };

    let _ = Command::new("pactl")
        .args(["unload-module", &module])
        .output();

    let mut hop = vec![0.0_f32; HOP];
    let mut err_seen = None;
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        match capture.read_hop(&mut hop, Duration::from_millis(20)) {
            Ok(_) => {}
            Err(e) => {
                err_seen = Some(e.to_string());
                break;
            }
        }
    }
    let outcome = match err_seen {
        Some(e) => format!("设备移除后采集报错（断流被识别）：{e}"),
        None => match capture.health() {
            PulseHealth::Ready => {
                "设备移除后采集流**未中断**：PipeWire 把流自动迁到了别的源（不触发重连）"
                    .to_string()
            }
            other => format!("设备移除后 health={other:?} 但读取未报错"),
        },
    };
    notes.push(outcome.clone());
    drop(capture);
    outcome
}
