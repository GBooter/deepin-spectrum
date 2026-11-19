//! Treeland / Wayland 叠加层：`zwlr_layer_shell_v1` 的 overlay 层 + SHM 软件渲染。
//!
//! ## 为什么不是 `XWayland`
//!
//! Treeland 下 dde-shell 的 Dock 是 Wayland 原生 surface；XWayland 的
//! override-redirect 窗口无法稳定盖在它上面（实测 Treeland 下频谱不可见）。
//! 因此 Wayland 会话必须走 `zwlr_layer_shell_v1`（Treeland 已导出 global 34/v5）。
//!
//! ## 坐标与缩放
//!
//! layer-shell 的 `set_size` 使用**逻辑像素**；`wl_surface.set_buffer_scale(n)`
//! 再告诉合成器"这块缓冲是 n 倍逻辑尺寸"。本实现用 `scale`（Treeland 当前
//! `wl_output.scale=2`）渲染缓冲：逻辑 1829×52 → 缓冲 3658×104。
//! 合成器按逻辑尺寸显示，物理输出上仍是清晰的高分辨率像素。
//!
//! ## 线程模型
//!
//! 主线程（音频/FFT）只通过 channel 发送整帧像素；Wayland 连接、layer surface、
//! SHM 缓冲与 calloop 事件循环都在独立线程里。这样音频节奏不会被 Wayland
//! 往返阻塞，Wayland 事件也不会拖慢 FFT。

use std::collections::HashMap;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shell::wlr_layer::{
    Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
    LayerSurfaceConfigure,
};
use smithay_client_toolkit::shm::slot::SlotPool;
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::{
    delegate_compositor, delegate_layer, delegate_output, delegate_registry, delegate_shm,
    registry_handlers,
};

// SCTK 重新导出的版本与本 crate 使用的 wayland-client 完全一致，避免版本漂移。
use smithay_client_toolkit::reexports::calloop;
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::reexports::client::globals::registry_queue_init;
use smithay_client_toolkit::reexports::client::protocol::{wl_output, wl_shm, wl_surface};
use smithay_client_toolkit::reexports::client::{
    Connection, Dispatch, QueueHandle, event_created_child,
};
// 原生全屏检测：Treeland 导出 zwlr_foreign_toplevel_manager_v1 v3。
use wayland_protocols_wlr::foreign_toplevel::v1::client::{
    zwlr_foreign_toplevel_handle_v1::{Event as ToplevelEvent, ZwlrForeignToplevelHandleV1},
    zwlr_foreign_toplevel_manager_v1::{Event as ManagerEvent, ZwlrForeignToplevelManagerV1},
};

use crate::overlay::{OverlayBackend, OverlayError, OverlayGeometry};

/// 层名称（合成器日志/调试可读）。
const NAMESPACE: &str = "deepin-spectrum";

/// 主线程 → Wayland 线程的命令。
#[derive(Clone, Copy)]
enum Command {
    /// 有新帧可用（帧数据在共享槽里，命令本身不携带像素）。
    Wake,
    /// 新的物理几何 + 停靠位置 + 是否显示在任务栏上方。
    Geometry {
        physical: OverlayGeometry,
        position: i32,
        above_dock: bool,
    },
    Show,
    Hide,
    Quit,
}

/// 一帧像素。
struct Frame {
    pixels: Vec<u32>,
    width: u32,
    height: u32,
}

/// 单槽最新帧邮箱。
///
/// `put_image` 每帧都要发数据，但 Wayland 线程可能短时卡顿；用无界 channel
/// 直接传 `Vec` 会在卡顿时堆出几十 MB。这里只保留**最新一帧**：
/// - 生产端复制进固定槽（容量复用，不再每帧分配）；
/// - 消费端用一个 `Vec` 与槽交换（ping-pong），零额外分配；
/// - `Wake` 命令用原子标志合并，队列里最多一个。
struct FrameMailbox {
    slot: Mutex<FrameSlot>,
    wake: AtomicBool,
}

struct FrameSlot {
    pixels: Vec<u32>,
    width: u32,
    height: u32,
    /// 是否有未被消费的新帧。
    pending: bool,
}

impl Default for FrameMailbox {
    fn default() -> Self {
        Self {
            slot: Mutex::new(FrameSlot {
                pixels: Vec::new(),
                width: 0,
                height: 0,
                pending: false,
            }),
            wake: AtomicBool::new(false),
        }
    }
}

impl FrameMailbox {
    /// 写入最新帧；返回是否需要向 Wayland 线程发送 `Wake`。
    fn publish(&self, pixels: &[u32], width: u32, height: u32) -> bool {
        {
            let mut slot = self
                .slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            slot.pixels.clear();
            slot.pixels.extend_from_slice(pixels);
            slot.width = width;
            slot.height = height;
            slot.pending = true;
        }
        !self.wake.swap(true, Ordering::SeqCst)
    }

    /// 消费端在收到 Wake 时先清标志，再取最新帧。
    ///
    /// 顺序不能反：若先取帧再清标志，取帧期间生产端看到 `wake=true`
    /// 不会发新 Wake，它的新帧就再也没人消费。
    fn claim_wake(&self) {
        self.wake.store(false, Ordering::SeqCst);
    }

    /// 把最新帧交换到 `current`；没有新帧时返回 false。
    fn take_latest(&self, current: &mut Option<Frame>) -> bool {
        let mut slot = self
            .slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !slot.pending {
            return false;
        }
        slot.pending = false;
        match current {
            Some(frame) => {
                std::mem::swap(&mut frame.pixels, &mut slot.pixels);
                frame.width = slot.width;
                frame.height = slot.height;
            }
            None => {
                *current = Some(Frame {
                    pixels: std::mem::take(&mut slot.pixels),
                    width: slot.width,
                    height: slot.height,
                });
            }
        }
        true
    }
}

/// Wayland 叠加层（主线程句柄）。
pub struct WaylandOverlay {
    tx: calloop::channel::Sender<Command>,
    geometry: OverlayGeometry,
    position: i32,
    /// 线程内实际生效的 `wl_output.scale`（启动时只是提示值）。
    scale: Arc<AtomicU32>,
    /// 最近一次 configure 后的物理缓冲尺寸（`w << 32 | h`）。
    actual_size: Arc<AtomicU64>,
    /// configure 代数：每次 configure 递增，供主线程区分“新 configure”与旧值。
    configure_generation: Arc<AtomicU64>,
    /// 是否检测到原生 Wayland 全屏窗口。
    fullscreen: Arc<AtomicBool>,
    /// 最新帧邮箱（有界，不随 channel 队列增长）。
    frames: Arc<FrameMailbox>,
    above_dock: bool,
    join: Option<JoinHandle<()>>,
    alive: Arc<AtomicBool>,
}

impl std::fmt::Debug for WaylandOverlay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WaylandOverlay")
            .field("geometry", &self.geometry)
            .field("position", &self.position)
            .field("scale", &self.scale())
            .field("alive", &self.alive.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl WaylandOverlay {
    /// 建立 layer-shell 叠加层。
    ///
    /// `geometry` 是**物理缓冲尺寸**（逻辑 dock 尺寸 × `scale`）；
    /// `position` 与 dde-shell 的 `Position` 属性一致（0=top / 1=right / 2=bottom / 3=left）。
    ///
    /// # Errors
    ///
    /// 几何非法、Wayland 连接/全局缺失、layer surface 创建失败，或线程启动超时。
    pub fn create(
        geometry: OverlayGeometry,
        scale: u32,
        position: i32,
    ) -> Result<Self, OverlayError> {
        Self::create_with_socket(geometry, scale, position, false, None)
    }

    /// 与 [`Self::create`] 相同，但可显式指定 Wayland socket 路径。
    ///
    /// systemd 用户服务不一定会继承 `WAYLAND_DISPLAY`；调用方可用
    /// `$XDG_RUNTIME_DIR/treeland.socket` 直接连接，避免依赖环境变量。
    ///
    /// # Errors
    ///
    /// 同 [`Self::create`]。
    pub fn create_with_socket(
        geometry: OverlayGeometry,
        scale: u32,
        position: i32,
        above_dock: bool,
        socket: Option<PathBuf>,
    ) -> Result<Self, OverlayError> {
        if !geometry.is_valid() {
            return Err(OverlayError::InvalidGeometry {
                width: geometry.width,
                height: geometry.height,
            });
        }
        let scale_hint = scale.max(1);
        let scale_shared = Arc::new(AtomicU32::new(scale_hint));
        let actual_size_shared = Arc::new(AtomicU64::new(pack_size(
            u32::from(geometry.width),
            u32::from(geometry.height),
        )));
        let configure_generation_shared = Arc::new(AtomicU64::new(0));
        let fullscreen_shared = Arc::new(AtomicBool::new(false));
        let frames_shared = Arc::new(FrameMailbox::default());
        let (ready_tx, ready_rx) = sync_channel(0);
        let (tx, rx) = calloop::channel::channel();
        let alive = Arc::new(AtomicBool::new(true));
        let alive_thread = Arc::clone(&alive);
        let thread_scale = Arc::clone(&scale_shared);
        let thread_actual = Arc::clone(&actual_size_shared);
        let thread_generation = Arc::clone(&configure_generation_shared);
        let thread_fullscreen = Arc::clone(&fullscreen_shared);
        let thread_frames = Arc::clone(&frames_shared);

        let join = std::thread::Builder::new()
            .name("deepin-spectrum-wayland".to_owned())
            .spawn(move || {
                let result = run(
                    geometry,
                    scale_hint,
                    thread_scale,
                    thread_actual,
                    thread_generation,
                    thread_fullscreen,
                    thread_frames,
                    position,
                    above_dock,
                    socket,
                    rx,
                    &ready_tx,
                );
                if let Err(err) = result {
                    eprintln!("[wayland] 叠加层线程退出：{err}");
                }
                alive_thread.store(false, Ordering::SeqCst);
            })
            .map_err(|e| OverlayError::Wayland {
                operation: "spawn",
                detail: e.to_string(),
            })?;

        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(Self {
                tx,
                geometry,
                position,
                scale: scale_shared,
                actual_size: actual_size_shared,
                configure_generation: configure_generation_shared,
                fullscreen: fullscreen_shared,
                frames: frames_shared,
                above_dock,
                join: Some(join),
                alive,
            }),
            Ok(Err(err)) => {
                let _ = join.join();
                Err(OverlayError::Wayland {
                    operation: "create",
                    detail: err,
                })
            }
            Err(e) => {
                let _ = tx.send(Command::Quit);
                let _ = join.join();
                Err(OverlayError::Wayland {
                    operation: "create",
                    detail: format!("等待 Wayland 初始化超时：{e}"),
                })
            }
        }
    }

    /// 送一整帧像素（预乘 ARGB，`width * height` 个 `u32`）。
    ///
    /// # Errors
    ///
    /// 缓冲长度不符，或 Wayland 线程已退出。
    pub fn put_image(
        &mut self,
        pixels: &[u32],
        width: u16,
        height: u16,
    ) -> Result<(), OverlayError> {
        let expected = usize::from(width) * usize::from(height);
        if pixels.len() != expected {
            return Err(OverlayError::PixelBufferMismatch {
                width,
                height,
                pixels: pixels.len(),
                expected,
            });
        }
        // 复制进共享槽（容量复用）；Wake 命令不带像素，且会被合并成最多一个。
        if self
            .frames
            .publish(pixels, u32::from(width), u32::from(height))
            && let Err(err) = self.send(Command::Wake)
        {
            // 发送失败时把 wake 复位，否则下一帧会误以为已有 Wake 在途。
            self.frames.claim_wake();
            return Err(err);
        }
        Ok(())
    }

    /// 更新停靠位置（0=top / 1=right / 2=bottom / 3=left）。
    ///
    /// # Errors
    ///
    /// Wayland 线程已退出。
    pub fn set_position(&mut self, position: i32) -> Result<(), OverlayError> {
        if position == self.position {
            return Ok(());
        }
        self.position = position;
        self.send(Command::Geometry {
            physical: self.geometry,
            position,
            above_dock: self.above_dock,
        })
    }

    /// 切换「任务栏内」/「任务栏上方」模式。
    ///
    /// # Errors
    ///
    /// Wayland 线程已退出。
    pub fn set_above_dock(&mut self, above_dock: bool) -> Result<(), OverlayError> {
        if above_dock == self.above_dock {
            return Ok(());
        }
        self.above_dock = above_dock;
        self.send(Command::Geometry {
            physical: self.geometry,
            position: self.position,
            above_dock,
        })
    }

    /// 当前实际渲染倍率（由 Wayland 线程回传）。
    #[must_use]
    pub fn scale(&self) -> u32 {
        self.scale.load(Ordering::Relaxed).max(1)
    }

    /// 最近一次 configure 后实际生效的物理缓冲尺寸。
    ///
    /// 合成器可能对 `set_size` 做夹紧或返回不同尺寸；主线程必须按这个值
    /// 重建像素缓冲，否则 `draw()` 永远等不到匹配尺寸。
    #[must_use]
    pub fn actual_size(&self) -> (u16, u16) {
        unpack_size(self.actual_size.load(Ordering::Relaxed))
    }

    /// 最近一次 configure 的代数（每次 configure 递增）。
    #[must_use]
    pub fn configure_generation(&self) -> u64 {
        self.configure_generation.load(Ordering::Relaxed)
    }

    /// 原生 Wayland 全屏检测标志（供主循环读取）。
    #[must_use]
    pub fn fullscreen_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.fullscreen)
    }

    /// 当前是否检测到原生 Wayland 全屏窗口。
    #[must_use]
    pub fn is_fullscreen(&self) -> bool {
        self.fullscreen.load(Ordering::Relaxed)
    }

    /// 线程是否还活着。
    #[must_use]
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    fn send(&self, cmd: Command) -> Result<(), OverlayError> {
        self.tx.send(cmd).map_err(|e| OverlayError::Wayland {
            operation: "send",
            detail: e.to_string(),
        })
    }
}

impl OverlayBackend for WaylandOverlay {
    fn geometry(&self) -> Result<OverlayGeometry, OverlayError> {
        Ok(self.geometry)
    }

    fn set_geometry(&mut self, geometry: OverlayGeometry) -> Result<(), OverlayError> {
        if !geometry.is_valid() {
            return Err(OverlayError::InvalidGeometry {
                width: geometry.width,
                height: geometry.height,
            });
        }
        if geometry == self.geometry {
            return Ok(());
        }
        self.geometry = geometry;
        self.send(Command::Geometry {
            physical: geometry,
            position: self.position,
            above_dock: self.above_dock,
        })
    }

    fn show(&mut self) -> Result<(), OverlayError> {
        self.send(Command::Show)
    }

    fn hide(&mut self) -> Result<(), OverlayError> {
        self.send(Command::Hide)
    }

    fn flush(&mut self) -> Result<(), OverlayError> {
        Ok(())
    }
}

impl Drop for WaylandOverlay {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Quit);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// Wayland 线程主体。
#[allow(clippy::too_many_arguments)]
fn run(
    initial: OverlayGeometry,
    scale: u32,
    scale_shared: Arc<AtomicU32>,
    actual_size_shared: Arc<AtomicU64>,
    configure_generation_shared: Arc<AtomicU64>,
    fullscreen_shared: Arc<AtomicBool>,
    frames_shared: Arc<FrameMailbox>,
    position: i32,
    above_dock: bool,
    socket: Option<PathBuf>,
    rx: calloop::channel::Channel<Command>,
    ready: &SyncSender<Result<(), String>>,
) -> Result<(), String> {
    let mut event_loop: calloop::EventLoop<WaylandState> =
        calloop::EventLoop::try_new().map_err(|e| format!("创建 calloop 失败：{e}"))?;
    let handle = event_loop.handle();

    let conn = match socket {
        Some(path) => {
            let stream = UnixStream::connect(&path)
                .map_err(|e| format!("连接 Wayland socket {} 失败：{e}", path.display()))?;
            Connection::from_socket(stream).map_err(|e| format!("连接 Wayland 失败：{e}"))?
        }
        None => Connection::connect_to_env().map_err(|e| format!("连接 Wayland 失败：{e}"))?,
    };
    let (globals, event_queue) = registry_queue_init::<WaylandState>(&conn)
        .map_err(|e| format!("读取 Wayland globals 失败：{e}"))?;
    let qh = event_queue.handle();

    let compositor =
        CompositorState::bind(&globals, &qh).map_err(|e| format!("wl_compositor 不可用：{e}"))?;
    let layer_shell =
        LayerShell::bind(&globals, &qh).map_err(|e| format!("zwlr_layer_shell_v1 不可用：{e}"))?;
    let shm = Shm::bind(&globals, &qh).map_err(|e| format!("wl_shm 不可用：{e}"))?;
    // Treeland 导出 v3；绑定失败不致命（退化为只用 X11 EWMH 检测）。
    let toplevel_manager = globals
        .bind::<ZwlrForeignToplevelManagerV1, _, _>(&qh, 1..=3, ())
        .ok();
    if toplevel_manager.is_none() {
        trace("zwlr_foreign_toplevel_manager_v1 不可用，原生全屏检测关闭");
    }

    let surface = compositor.create_surface(&qh);
    let layer =
        layer_shell.create_layer_surface(&qh, surface, Layer::Overlay, Some(NAMESPACE), None);

    let (logical_w, logical_h) = logical_size(initial, scale);
    layer.set_anchor(anchor_for(position));
    layer.set_size(logical_w, logical_h);
    // 统一用 -1：不参与 exclusive zone 布局，位置完全由 margin 控制。
    // 这样「任务栏内 / 任务栏上方」只改 margin，不依赖 Treeland 是否
    // 在运行期重新计算 exclusive zone（实测后者可能被忽略）。
    layer.set_exclusive_zone(-1);
    layer.set_keyboard_interactivity(KeyboardInteractivity::None);
    let margin = margin_for(position, logical_w, logical_h, above_dock);
    layer.set_margin(margin.0, margin.1, margin.2, margin.3);

    // 输入穿透：空 input region。必须在首次 commit 前设置。
    {
        let region = smithay_client_toolkit::compositor::Region::new(&compositor)
            .map_err(|e| format!("创建 wl_region 失败：{e}"))?;
        layer.set_input_region(Some(region.wl_region()));
    }
    layer
        .set_buffer_scale(scale)
        .map_err(|e| format!("set_buffer_scale 失败：{e:?}"))?;

    // 首次无 buffer commit：合成器随后发 configure。
    layer.commit();

    let pool = SlotPool::new(
        usize::try_from(u32::from(initial.width) * u32::from(initial.height) * 4)
            .map_err(|_| "初始缓冲尺寸溢出".to_string())?,
        &shm,
    )
    .map_err(|e| format!("创建 SHM pool 失败：{e}"))?;

    let signal = event_loop.get_signal();
    let mut state = WaylandState {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        shm,
        layer,
        pool,
        scale,
        scale_shared,
        actual_size: actual_size_shared,
        configure_generation: configure_generation_shared,
        fullscreen: fullscreen_shared,
        frames: frames_shared,
        toplevel_manager,
        toplevels: HashMap::new(),
        position,
        above_dock,
        logical: (logical_w, logical_h),
        configured: false,
        hidden: false,
        latest: None,
        signal,
    };
    state.publish_actual_size();

    WaylandSource::new(conn, event_queue)
        .insert(handle.clone())
        .map_err(|e| format!("插入 Wayland source 失败：{e}"))?;

    handle
        .insert_source(rx, |event, (), state: &mut WaylandState| match event {
            calloop::channel::Event::Msg(cmd) => state.handle(cmd),
            calloop::channel::Event::Closed => state.signal.stop(),
        })
        .map_err(|e| format!("插入命令 channel 失败：{e}"))?;

    ready
        .send(Ok(()))
        .map_err(|_| "主线程已放弃等待".to_string())?;

    event_loop
        .run(None, &mut state, |_| {})
        .map_err(|e| format!("calloop 运行失败：{e}"))?;
    Ok(())
}

struct WaylandState {
    registry_state: RegistryState,
    output_state: OutputState,
    shm: Shm,
    layer: LayerSurface,
    pool: SlotPool,
    scale: u32,
    /// 与主线程共享的实际 scale。
    scale_shared: Arc<AtomicU32>,
    /// configure 后的实际物理缓冲尺寸（`w << 32 | h`）。
    actual_size: Arc<AtomicU64>,
    /// configure 代数，供主线程判断是否需要重建缓冲。
    configure_generation: Arc<AtomicU64>,
    /// 原生全屏检测结果（与主线程共享）。
    fullscreen: Arc<AtomicBool>,
    /// 最新帧邮箱（与主线程共享）。
    frames: Arc<FrameMailbox>,
    /// 保持 manager 代理存活；`None` = 合成器不支持该协议。
    #[allow(dead_code)]
    toplevel_manager: Option<ZwlrForeignToplevelManagerV1>,
    /// 当前 toplevel 状态：`(fullscreen, activated, minimized)`。
    toplevels: HashMap<ZwlrForeignToplevelHandleV1, (bool, bool, bool)>,
    position: i32,
    above_dock: bool,
    /// 逻辑尺寸（layer surface 的 `set_size` 口径）。
    logical: (u32, u32),
    configured: bool,
    /// 被主循环显式隐藏（任务栏自动隐藏时高度会缩到 2px）。
    hidden: bool,
    latest: Option<Frame>,
    signal: calloop::LoopSignal,
}

impl WaylandState {
    /// 把当前逻辑尺寸 × scale 发布给主线程。
    ///
    /// 只在 `configure`（含首次）时调用：`set_size` 可能被合成器夹紧，
    /// 主线程必须以 configure 的实际结果为准重建像素缓冲。
    fn publish_actual_size(&self) {
        let scale = self.scale.max(1);
        let width = self.logical.0.saturating_mul(scale);
        let height = self.logical.1.saturating_mul(scale);
        self.actual_size
            .store(pack_size(width, height), Ordering::Relaxed);
    }

    /// 重新计算「是否有全屏窗口」并写入共享标志。
    ///
    /// 不要求 `activated`：视频全屏时用户可能把焦点切到另一块屏幕的窗口，
    /// 但全屏画面仍然可见，频谱不应盖回去。最小化的全屏窗口不算。
    fn publish_fullscreen(&self) {
        let any = self
            .toplevels
            .values()
            .any(|(fullscreen, _activated, minimized)| *fullscreen && !*minimized);
        self.fullscreen.store(any, Ordering::Relaxed);
    }
}

impl WaylandState {
    fn handle(&mut self, cmd: Command) {
        match cmd {
            Command::Wake => {
                self.frames.claim_wake();
                if self.frames.take_latest(&mut self.latest) && self.configured {
                    self.draw();
                }
            }
            Command::Geometry {
                physical,
                position,
                above_dock,
            } => {
                let (w, h) = logical_size(physical, self.scale);
                let size_changed = self.logical != (w, h);
                self.position = position;
                self.above_dock = above_dock;
                self.logical = (w, h);
                self.layer.set_anchor(anchor_for(position));
                self.layer.set_size(w, h);
                self.layer.set_exclusive_zone(-1);
                let margin = margin_for(position, w, h, above_dock);
                self.layer
                    .set_margin(margin.0, margin.1, margin.2, margin.3);
                self.layer.commit();
                if size_changed {
                    // 尺寸变了：等合成器下发新的 configure，避免把旧尺寸拉伸。
                    self.configured = false;
                } else {
                    // 只切模式/位置：尺寸没变，合成器不一定会再发 configure，
                    // 必须继续用当前帧上屏，否则频谱会冻住/消失。
                    self.draw();
                }
                trace(&format!(
                    "geometry {w}x{h} logical, position={position}, above={above_dock}, margin={margin:?}, size_changed={size_changed}"
                ));
            }
            Command::Show => {
                self.hidden = false;
                self.frames.take_latest(&mut self.latest);
                if let Some(frame) = &self.latest {
                    let (w, h) = (frame.width, frame.height);
                    self.draw_frame(w, h);
                } else {
                    self.layer.commit();
                }
            }
            Command::Hide => {
                self.hidden = true;
                self.layer.wl_surface().attach(None, 0, 0);
                self.layer.commit();
                self.configured = false;
            }
            Command::Quit => {
                self.signal.stop();
            }
        }
    }

    fn draw(&mut self) {
        if self.hidden {
            return;
        }
        let Some(frame) = &self.latest else {
            return;
        };
        let (w, h) = (frame.width, frame.height);
        let (lw, lh) = self.logical;
        let expected = (lw.saturating_mul(self.scale), lh.saturating_mul(self.scale));
        if (w, h) != expected {
            // 管线还没按新几何重渲染：等下一帧，避免把旧尺寸拉伸。
            return;
        }
        self.draw_frame(w, h);
    }

    fn draw_frame(&mut self, width: u32, height: u32) {
        let Some(frame) = &self.latest else {
            return;
        };
        if frame.width != width || frame.height != height {
            return;
        }
        let stride = i32::try_from(width.saturating_mul(4)).unwrap_or(i32::MAX);
        let Ok(w) = i32::try_from(width) else {
            return;
        };
        let Ok(h) = i32::try_from(height) else {
            return;
        };
        let Ok((buffer, canvas)) = self
            .pool
            .create_buffer(w, h, stride, wl_shm::Format::Argb8888)
        else {
            eprintln!("[wayland] create_buffer 失败");
            return;
        };
        for (chunk, px) in canvas.chunks_exact_mut(4).zip(&frame.pixels) {
            chunk.copy_from_slice(&px.to_le_bytes());
        }
        self.layer.wl_surface().damage_buffer(0, 0, w, h);
        if buffer.attach_to(self.layer.wl_surface()).is_err() {
            eprintln!("[wayland] attach buffer 失败");
            return;
        }
        self.layer.commit();
        trace(&format!("draw {width}x{height}"));
    }
}

impl CompositorHandler for WaylandState {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        new_factor: i32,
    ) {
        if let Ok(factor) = u32::try_from(new_factor)
            && factor > 0
            && factor != self.scale
        {
            self.scale = factor;
            // 主线程按这个值重建像素缓冲；`configured=false` 等合成器下发新 configure。
            self.scale_shared.store(factor, Ordering::Relaxed);
            let _ = self.layer.set_buffer_scale(factor);
            let (w, h) = self.logical;
            self.layer.set_size(w, h);
            self.layer.commit();
            self.configured = false;
            trace(&format!("scale -> {factor}"));
        }
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_transform: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for WaylandState {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }
}

impl ShmHandler for WaylandState {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl LayerShellHandler for WaylandState {
    fn closed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _layer: &LayerSurface) {
        self.signal.stop();
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        let (w, h) = if configure.new_size.0 > 0 && configure.new_size.1 > 0 {
            configure.new_size
        } else {
            self.logical
        };
        self.logical = (w, h);
        self.configured = true;
        // 合成器可能夹紧尺寸；把实际值回传主线程重建缓冲。
        self.configure_generation.fetch_add(1, Ordering::Relaxed);
        self.publish_actual_size();
        trace(&format!("configure {w}x{h} logical"));
        self.draw();
    }
}

impl Dispatch<ZwlrForeignToplevelManagerV1, ()> for WaylandState {
    // `toplevel` 事件（opcode 0）会创建 `zwlr_foreign_toplevel_handle_v1` 子对象。
    // wayland-client 要求为这类"事件创建对象"显式声明子对象类型与用户数据；
    // 缺省实现会直接 panic —— Treeland 绑定 manager 后立刻下发该事件，
    // 叠加层线程随即崩溃，主循环随后因 put_image 失败退出（表现为"频谱无法启动"）。
    event_created_child!(WaylandState, ZwlrForeignToplevelManagerV1, [
        0 => (ZwlrForeignToplevelHandleV1, ()),
    ]);

    fn event(
        state: &mut Self,
        _proxy: &ZwlrForeignToplevelManagerV1,
        event: ManagerEvent,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            ManagerEvent::Toplevel { toplevel } => {
                state.toplevels.insert(toplevel, (false, false, false));
                state.publish_fullscreen();
            }
            ManagerEvent::Finished => {
                state.toplevels.clear();
                state.publish_fullscreen();
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrForeignToplevelHandleV1, ()> for WaylandState {
    fn event(
        state: &mut Self,
        proxy: &ZwlrForeignToplevelHandleV1,
        event: ToplevelEvent,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            ToplevelEvent::State { state: states } => {
                let fullscreen = array_has_state(&states, STATE_FULLSCREEN);
                let activated = array_has_state(&states, STATE_ACTIVATED);
                let minimized = array_has_state(&states, STATE_MINIMIZED);
                state
                    .toplevels
                    .insert(proxy.clone(), (fullscreen, activated, minimized));
                state.publish_fullscreen();
                trace(&format!(
                    "toplevel state fullscreen={fullscreen} activated={activated} minimized={minimized}"
                ));
            }
            ToplevelEvent::Closed => {
                state.toplevels.remove(proxy);
                state.publish_fullscreen();
            }
            _ => {}
        }
    }
}

impl ProvidesRegistryState for WaylandState {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }

    registry_handlers![OutputState];
}

delegate_compositor!(WaylandState);
delegate_output!(WaylandState);
delegate_shm!(WaylandState);
delegate_layer!(WaylandState);
delegate_registry!(WaylandState);

/// `zwlr_foreign_toplevel_handle_v1.state` 的枚举值（协议 v1 XML）。
const STATE_MINIMIZED: u32 = 1;
const STATE_ACTIVATED: u32 = 2;
const STATE_FULLSCREEN: u32 = 3;

/// 协议把 enum 数组编码为 `u32` 数组（宿主字节序）。
fn array_has_state(states: &[u8], value: u32) -> bool {
    states.chunks_exact(4).any(|chunk| {
        let mut bytes = [0_u8; 4];
        bytes.copy_from_slice(chunk);
        u32::from_ne_bytes(bytes) == value
    })
}

/// `(w, h)` → `w << 32 | h`，供 `AtomicU64` 无锁回传。
fn pack_size(width: u32, height: u32) -> u64 {
    (u64::from(width) << 32) | u64::from(height)
}

/// `pack_size` 的逆运算；超出 `u16` 时饱和，避免窄化 panic。
fn unpack_size(packed: u64) -> (u16, u16) {
    let width = u32::try_from(packed >> 32).unwrap_or(u32::MAX);
    let height = u32::try_from(packed & 0xFFFF_FFFF).unwrap_or(u32::MAX);
    (
        u16::try_from(width).unwrap_or(u16::MAX),
        u16::try_from(height).unwrap_or(u16::MAX),
    )
}

/// 物理缓冲尺寸 → 逻辑尺寸（向上取整，避免少 1px 露缝）。
fn logical_size(physical: OverlayGeometry, scale: u32) -> (u32, u32) {
    let s = scale.max(1);
    let w = u32::from(physical.width).div_ceil(s);
    let h = u32::from(physical.height).div_ceil(s);
    (w.max(1), h.max(1))
}

/// dde-shell 的 Position 枚举 → layer-shell 锚点。
fn anchor_for(position: i32) -> Anchor {
    match position {
        0 => Anchor::TOP | Anchor::LEFT | Anchor::RIGHT,
        1 => Anchor::RIGHT | Anchor::TOP | Anchor::BOTTOM,
        3 => Anchor::LEFT | Anchor::TOP | Anchor::BOTTOM,
        _ => Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT,
    }
}

/// `AboveDock` 的边距：把表面整体推离任务栏一个"厚度"。
///
/// 底部任务栏：bottom margin = 任务栏高度；顶部：top margin = 高度；
/// 左/右任务栏：left/right margin = 任务栏宽度。`FullDock` 全部为 0。
fn margin_for(
    position: i32,
    logical_w: u32,
    logical_h: u32,
    above_dock: bool,
) -> (i32, i32, i32, i32) {
    if !above_dock {
        return (0, 0, 0, 0);
    }
    let w = i32::try_from(logical_w).unwrap_or(0);
    let h = i32::try_from(logical_h).unwrap_or(0);
    match position {
        0 => (h, 0, 0, 0),
        1 => (0, w, 0, 0),
        3 => (0, 0, 0, w),
        _ => (0, 0, h, 0),
    }
}

fn trace(msg: &str) {
    if std::env::var_os("SPECTRUM_WAYLAND_TRACE").is_some() {
        eprintln!("[wayland] {msg}");
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Anchor, FrameMailbox, STATE_ACTIVATED, STATE_FULLSCREEN, STATE_MINIMIZED, anchor_for,
        array_has_state, logical_size, margin_for, pack_size, unpack_size,
    };
    use crate::overlay::OverlayGeometry;

    #[test]
    fn logical_size_rounds_up() {
        let geo = OverlayGeometry::new(0, 0, 3658, 104);
        assert_eq!(logical_size(geo, 2), (1829, 52));
        // 非整除时向上取整，避免少 1px
        assert_eq!(
            logical_size(OverlayGeometry::new(0, 0, 3200, 91), 2),
            (1600, 46)
        );
        assert_eq!(
            logical_size(OverlayGeometry::new(0, 0, 10, 10), 0),
            (10, 10)
        );
    }

    #[test]
    fn actual_size_round_trips_for_scale_values() {
        for scale in [1_u32, 2, 3] {
            let (w, h) = (1829_u32, 52_u32);
            let packed = pack_size(w.saturating_mul(scale), h.saturating_mul(scale));
            assert_eq!(
                unpack_size(packed),
                (
                    u16::try_from(w * scale).unwrap(),
                    u16::try_from(h * scale).unwrap()
                ),
                "scale={scale} 时物理尺寸换算不一致"
            );
        }
        // 超界值饱和而不是 panic
        assert_eq!(
            unpack_size(pack_size(u32::MAX, u32::MAX)),
            (u16::MAX, u16::MAX)
        );
    }

    #[test]
    fn toplevel_state_array_decodes_enum_values() {
        let mut states = Vec::new();
        for value in [STATE_ACTIVATED, STATE_FULLSCREEN] {
            states.extend_from_slice(&value.to_ne_bytes());
        }
        assert!(array_has_state(&states, STATE_FULLSCREEN));
        assert!(array_has_state(&states, STATE_ACTIVATED));
        assert!(!array_has_state(&states, STATE_MINIMIZED));
        assert!(
            !array_has_state(&states[..5], STATE_FULLSCREEN),
            "不完整数组不应误判"
        );
    }

    #[test]
    fn frame_mailbox_keeps_latest_and_coalesces_wake() {
        let mailbox = FrameMailbox::default();
        let mut current = None;
        assert!(!mailbox.take_latest(&mut current), "初始没有帧");

        assert!(mailbox.publish(&[1, 2], 1, 2), "第一帧需要 Wake");
        assert!(
            !mailbox.publish(&[3, 4], 1, 2),
            "第二帧在同一个 Wake 周期内，不应再入队"
        );

        mailbox.claim_wake();
        assert!(mailbox.take_latest(&mut current));
        assert_eq!(
            current.as_ref().map(|f| f.pixels.as_slice()),
            Some([3, 4].as_slice()),
            "消费端必须拿到最新帧"
        );
        assert!(!mailbox.take_latest(&mut current), "没有新帧时不应重复消费");
    }

    #[test]
    fn bottom_position_anchors_bottom() {
        let anchor = anchor_for(2);
        assert!(anchor.contains(Anchor::BOTTOM));
    }

    #[test]
    fn above_dock_is_expressed_as_margin() {
        // FullDock：不偏移，直接覆盖任务栏
        assert_eq!(margin_for(2, 1829, 53, false), (0, 0, 0, 0));
        // AboveDock：底部任务栏 → bottom margin = 任务栏高度
        assert_eq!(margin_for(2, 1829, 53, true), (0, 0, 53, 0));
        // 顶部任务栏 → top margin
        assert_eq!(margin_for(0, 1829, 53, true), (53, 0, 0, 0));
    }
}
