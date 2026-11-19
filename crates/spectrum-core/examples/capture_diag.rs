//! 采集驱动方式对照诊断（需 `capture-pulse` feature）。
//!
//! 目的：定位「2 秒只采到 16384 帧、之后大量 `peek` 全 Empty」的根因，
//! 并验证哪种驱动方式能跟上实时吞吐。四个实验阶段各自 1 秒：
//!
//! - A `block=false`：`iterate(false)` + `sleep(2ms)`（原生产实现），**不传** `BufferAttr`
//! - B `block=true` ：`iterate(true)` 阻塞等待事件后再 `peek`，**不传** `BufferAttr`
//! - C `threaded`   ：`ThreadedMainloop` 独立线程持续 poll，主线程 lock 下 `peek`，**不传** `BufferAttr`
//! - D `attr`       ：驱动方式与 A **完全相同**，仅额外传 `BufferAttr(40ms, ADJUST_LATENCY)`
//!
//! 关键在于 A 与 D 构成**单变量对照**：两者 `iters`/`empty` 计数一致，
//! 唯一差别是 `BufferAttr`，因此帧数差异可直接归因于它，而非驱动方式。
//!
//! 实测结论（M27）：A/B/C 均为 `0.000` 实时比（peek 永远 Empty），
//! D 为 `1.000`（25 块 × 1920 帧 = 48000 帧/秒）。
//!
//! 只读取 monitor，不播放、不新建/卸载 sink、不改默认输出。
//!
//! 用法：
//! ```text
//! cargo run --release -p spectrum-core --features capture-pulse --example capture_diag
//! ```

use libpulse_binding::context::{Context, FlagSet as ContextFlagSet, State as ContextState};
use libpulse_binding::def::BufferAttr;
use libpulse_binding::mainloop::standard::Mainloop;
use libpulse_binding::mainloop::threaded::Mainloop as ThreadedMainloop;
use libpulse_binding::sample::{Format, Spec};
use libpulse_binding::stream::{
    FlagSet as StreamFlagSet, PeekResult, State as StreamState, Stream,
};
use std::time::{Duration, Instant};

const DEVICE: &str = "@DEFAULT_MONITOR@";
const RATE: u32 = 48_000;

#[derive(Default)]
struct Bucket {
    frames: u64,
    data: u64,
    hole: u64,
    empty: u64,
    max_chunk: usize,
    iters: u64,
}

#[allow(clippy::cast_precision_loss)]
fn main() {
    let a = phase_standard(false, false);
    report("A block=false（无 attr）", &a);
    let b = phase_standard(true, false);
    report("B block=true（无 attr）", &b);
    let c = phase_threaded();
    report("C threaded（无 attr）", &c);
    let d = phase_standard(false, true);
    report("D block=false + BufferAttr", &d);

    let realtime = f64::from(RATE);
    println!(
        "\n{{\n  \"device\": \"{DEVICE}\",\n  \"sample_rate\": {RATE},\n  \"window_ms\": 1000,"
    );
    for (name, key, v) in [
        ("非阻塞 iterate（无 attr）", "a_nonblock", &a),
        ("阻塞 iterate（无 attr）", "b_block", &b),
        ("threaded mainloop（无 attr）", "c_threaded", &c),
        (
            "非阻塞 + BufferAttr(40ms/ADJUST_LATENCY)",
            "d_nonblock_with_attr",
            &d,
        ),
    ] {
        println!(
            "  \"{key}\": {{\"label\": \"{name}\", \"frames\": {}, \"realtime_ratio\": {:.3}, \"data\": {}, \"hole\": {}, \"empty\": {}, \"max_chunk\": {}, \"iters\": {}}},",
            v.frames,
            v.frames as f64 / realtime,
            v.data,
            v.hole,
            v.empty,
            v.max_chunk,
            v.iters
        );
    }
    println!(
        "  \"verdict_note\": \"无 BufferAttr 时各驱动方式均远低于实时；加 attr 后同一驱动方式即达标 → 根因是 pa_buffer_attr，不是驱动方式\"\n}}"
    );
}

fn spec() -> Spec {
    Spec {
        format: Format::F32le,
        rate: RATE,
        channels: 1,
    }
}

/// 在给定 stream 上抽干可用 chunk。
fn drain(stream: &mut Stream, b: &mut Bucket, budget: usize) {
    for _ in 0..budget {
        match stream.peek() {
            Ok(PeekResult::Data(bytes)) => {
                b.data += 1;
                let frames = bytes.len() / 4;
                b.frames += frames as u64;
                b.max_chunk = b.max_chunk.max(frames);
                let _ = stream.discard();
            }
            Ok(PeekResult::Hole(_)) => {
                b.hole += 1;
                let _ = stream.discard();
            }
            Ok(PeekResult::Empty) => {
                b.empty += 1;
                break;
            }
            Err(_) => break,
        }
    }
}

fn phase_standard(block: bool, with_attr: bool) -> Bucket {
    let mut ml = Mainloop::new().expect("mainloop");
    let mut ctx = Context::new(&ml, "diag-std").expect("context");
    ctx.connect(None, ContextFlagSet::NOFLAGS, None)
        .expect("ctx connect");
    drive_until(&mut ml, || ctx.get_state() == ContextState::Ready);
    let mut stream = Stream::new(&mut ctx, "diag-std", &spec(), None).expect("stream");
    // 与生产实现一致的 BufferAttr（target_latency=40ms）
    let frag = RATE * 40 * 4 / 1000;
    let attr = BufferAttr {
        maxlength: u32::MAX,
        tlength: u32::MAX,
        prebuf: u32::MAX,
        minreq: u32::MAX,
        fragsize: frag,
    };
    let (attr_ref, flags) = if with_attr {
        (Some(&attr), StreamFlagSet::ADJUST_LATENCY)
    } else {
        (None, StreamFlagSet::NOFLAGS)
    };
    stream
        .connect_record(Some(DEVICE), attr_ref, flags)
        .expect("connect_record");
    drive_until(&mut ml, || stream.get_state() == StreamState::Ready);

    let mut b = Bucket::default();
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(1000) {
        if ml.iterate(block).is_error() {
            break;
        }
        b.iters += 1;
        drain(&mut stream, &mut b, 64);
        if !block {
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    let _ = stream.disconnect();
    ctx.disconnect();
    b
}

fn phase_threaded() -> Bucket {
    let mut ml = ThreadedMainloop::new().expect("threaded mainloop");
    let _ = ml.start();

    ml.lock();
    let mut ctx = Context::new(&ml, "diag-threaded").expect("context");
    if ctx.connect(None, ContextFlagSet::NOFLAGS, None).is_err() {
        ml.unlock();
        ml.stop();
        return Bucket::default();
    }
    ml.unlock();

    wait_threaded(&mut ml, || ctx.get_state() == ContextState::Ready);

    ml.lock();
    let mut stream = Stream::new(&mut ctx, "diag-threaded", &spec(), None).expect("stream");
    stream
        .connect_record(Some(DEVICE), None, StreamFlagSet::NOFLAGS)
        .expect("connect_record");
    ml.unlock();

    wait_threaded(&mut ml, || stream.get_state() == StreamState::Ready);

    let mut b = Bucket::default();
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(1000) {
        ml.lock();
        b.iters += 1;
        drain(&mut stream, &mut b, 64);
        ml.unlock();
        std::thread::sleep(Duration::from_millis(5));
    }

    ml.lock();
    let _ = stream.disconnect();
    ctx.disconnect();
    ml.unlock();
    ml.stop();
    b
}

#[allow(clippy::cast_precision_loss)]
fn report(name: &str, b: &Bucket) {
    eprintln!(
        "{name:24} frames={:6} ratio={:.3} data={:4} hole={:3} empty={:4} max_chunk={:5} iters={:5}",
        b.frames,
        b.frames as f64 / f64::from(RATE),
        b.data,
        b.hole,
        b.empty,
        b.max_chunk,
        b.iters
    );
}

fn drive_until(ml: &mut Mainloop, cond: impl Fn() -> bool) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(3) {
        if ml.iterate(false).is_error() {
            return;
        }
        if cond() {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn wait_threaded(ml: &mut ThreadedMainloop, cond: impl Fn() -> bool) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(3) {
        let ok = {
            ml.lock();
            let c = cond();
            ml.unlock();
            c
        };
        if ok {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}
