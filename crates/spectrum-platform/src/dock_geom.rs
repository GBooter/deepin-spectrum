//! D-Bus 任务栏几何订阅（`spectrum-platform`）。
//!
//! 职责（§6.6.1 / §6.6.2 +《任务栏高度自适应设计》§2）：
//!
//! - 建立**两个** [`Proxy`]：接口名不同，服务名与对象路径相同。
//!   用同一个 Proxy 读 `FrontendWindowRect` 会报 `UnknownProperty` ——
//!   这是最容易踩的坑，因为两个接口挂在同一个对象路径上，
//!   introspect 时看起来像「一个服务」。
//! - 订阅 `FrontendWindowRectChanged` 等信号，把事件转成 [`DockEvent`]
//!   投递给 core 的 `DockSignalBridge`。
//! - **信号驱动而非轮询**：信号齐全时 500ms 轮询纯属浪费 CPU，
//!   与「静音 ≈0% CPU」目标冲突。
//!
//! ## 线程模型
//!
//! zbus 6 的 [`zbus::block_on`] 会**阻塞**调用线程直到 future 完成，
//! 且信号流需要常驻循环。二者结合意味着订阅必须跑在独立线程上。
//!
//! 更关键的是：**一个 [`zbus::Connection`] 绑定到创建它的那次
//! `block_on` 的运行时**，不能跨线程共享。因此本模块为**每个接口**
//! 起一个线程，每个线程自带一条连接：
//!
//! ```text
//! main 线程                        dbus 线程（每个接口一个）
//! ─────────                        ────────────────────────
//! DockSignalBridge  ◄── channel ── daemon.Dock1 信号流
//!      │                            dock.Dock1 信号流
//!      └─ 每帧 tick() → Commit(Rect) → 重建表面
//! ```
//!
//! main 线程**只消费事件**（不碰 zbus），符合 §5.2「所有状态变更收敛到
//! main 线程」；zbus 自身的 I/O 线程由库管理。
//!
//! ## 特性开关
//!
//! 本模块需要 `dbus` feature（引入 `zbus 6.3`）。**默认构建不含 zbus**，
//! 保持「默认零外部依赖」的既有约定（与 `fft-realfft` / `capture-pulse` 一致）。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};

use spectrum_core::dock_signal::{DockEvent, RectSource};
use zbus::export::ordered_stream::OrderedStreamExt;
use zbus::proxy::Proxy;

/// 物理几何所在接口（服务名与接口名不同名，是最容易踩的坑）。
pub const DAEMON_INTERFACE: &str = "org.deepin.dde.daemon.Dock1";
/// 逻辑几何所在接口。
pub const DOCK_INTERFACE: &str = "org.deepin.dde.Dock1";
/// 两个接口共用的对象路径。
pub const DOCK_OBJECT_PATH: &str = "/org/deepin/dde/Dock1";

// ---- UOS 20 / dde-dock 老接口 ----------------------------------------------
//
// UOS 20（DDE 4.x）的 dde-dock 没有 `org.deepin.dde.*Dock1`，只有下面这套
// `com.deepin.dde.*`。属性名相同（`FrontendWindowRect` / `Position`），但
// 对象路径不同，且**没有矩形变更信号**（introspect 里只有 PropertiesChanged），
// 所以老接口只能低频轮询属性。

/// 老接口的 daemon 服务/接口名（物理矩形 `FrontendWindowRect`）。
pub const LEGACY_DAEMON_INTERFACE: &str = "com.deepin.dde.daemon.Dock";
/// 老接口的对象路径。
pub const LEGACY_DAEMON_PATH: &str = "/com/deepin/dde/daemon/Dock";
/// 老接口轮询间隔：老接口没有变更信号，只能轮询；1s 足够且开销可忽略。
const LEGACY_POLL_MS: u64 = 1_000;

/// 读取任务栏当前停靠位置（0=top / 1=right / 2=bottom / 3=left）。
///
/// 订阅只会在位置**变化**时发 `PositionChanged`；启动时若用户本来就把
/// 任务栏放在顶部/侧边，必须主动回读一次，否则第一帧会按底部计算偏移。
#[must_use]
pub fn read_position() -> Option<i32> {
    let modern = zbus::block_on(async {
        let conn = zbus::Connection::session().await.ok()?;
        let proxy = Proxy::new(&conn, DAEMON_INTERFACE, DOCK_OBJECT_PATH, DAEMON_INTERFACE)
            .await
            .ok()?;
        proxy.get_property("Position").await.ok()
    });
    modern.or_else(|| {
        zbus::block_on(async {
            let conn = zbus::Connection::session().await.ok()?;
            let proxy = Proxy::new(
                &conn,
                LEGACY_DAEMON_INTERFACE,
                LEGACY_DAEMON_PATH,
                LEGACY_DAEMON_INTERFACE,
            )
            .await
            .ok()?;
            proxy.get_property("Position").await.ok()
        })
    })
}

/// 主动回读一次权威矩形（阻塞调用）。
///
/// 用于 `DockOutcome::NeedsRefresh`：任务栏尺寸模式值变化后并不会伴随
/// 矩形变更信号，必须主动 `get_property` 才能拿到真实高度。
/// 只在尺寸模式变化（低频事件）时调用，阻塞一次 D-Bus 往返可以接受。
#[must_use]
pub fn read_rect(rect_source: RectSource) -> Option<DockEvent> {
    let modern = zbus::block_on(async {
        let conn = zbus::Connection::session().await.ok()?;
        let owner = match rect_source {
            RectSource::Physical => DAEMON_INTERFACE,
            RectSource::Logical => DOCK_INTERFACE,
        };
        let proxy = Proxy::new(&conn, owner, DOCK_OBJECT_PATH, owner)
            .await
            .ok()?;
        read_authoritative_rect(&proxy, rect_source).await
    });
    modern.or_else(read_legacy_rect)
}

/// 读取 UOS 20 / dde-dock 老接口的物理矩形（阻塞调用）。
///
/// 老接口的 `FrontendWindowRect` 签名是 `(iiuu)`（宽高为 `u32`），
/// 不能复用按 `(iiii)` 解析的 `read_authoritative_rect`。
#[must_use]
pub fn read_legacy_rect() -> Option<DockEvent> {
    zbus::block_on(async {
        let conn = zbus::Connection::session().await.ok()?;
        let proxy = Proxy::new(
            &conn,
            LEGACY_DAEMON_INTERFACE,
            LEGACY_DAEMON_PATH,
            LEGACY_DAEMON_INTERFACE,
        )
        .await
        .ok()?;
        let (x, y, width, height) = proxy
            .get_property::<(i32, i32, u32, u32)>("FrontendWindowRect")
            .await
            .ok()?;
        Some(DockEvent::RectChanged {
            x,
            y,
            width,
            height,
        })
    })
}

/// 老接口轮询线程：服务不存在时立即退出，不影响新接口订阅。
///
/// 老接口没有矩形/位置变更信号，只能按 [`LEGACY_POLL_MS`] 回读属性；
/// 值不变时不重复投递，避免给节流桥制造无意义事件。
fn spawn_legacy_poller(tx: &Sender<DockEvent>) {
    let tx = tx.clone();
    let _ = std::thread::Builder::new()
        .name("deepin-spectrum-dbus-legacy".to_owned())
        .spawn(move || {
            if !zbus::block_on(legacy_service_present()) {
                return;
            }
            let mut last_rect: Option<DockEvent> = None;
            let mut last_position: Option<i32> = None;
            loop {
                let snapshot = zbus::block_on(read_legacy_snapshot());
                if let Some((rect, position)) = snapshot {
                    if last_rect != Some(rect) {
                        last_rect = Some(rect);
                        if tx.send(rect).is_err() {
                            return;
                        }
                    }
                    if let Some(position) = position
                        && last_position != Some(position)
                    {
                        last_position = Some(position);
                        if tx.send(DockEvent::PositionChanged(position)).is_err() {
                            return;
                        }
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(LEGACY_POLL_MS));
            }
        });
}

/// 老接口服务是否存在（只探测一次，避免在新系统上永久轮询无人应答的名字）。
async fn legacy_service_present() -> bool {
    let Ok(conn) = zbus::Connection::session().await else {
        return false;
    };
    let Ok(dbus) = zbus::fdo::DBusProxy::new(&conn).await else {
        return false;
    };
    let Ok(name) = zbus::names::BusName::try_from(LEGACY_DAEMON_INTERFACE) else {
        return false;
    };
    dbus.name_has_owner(name).await.unwrap_or(false)
}

/// 一次老接口快照：矩形 + 停靠位置。
async fn read_legacy_snapshot() -> Option<(DockEvent, Option<i32>)> {
    let conn = zbus::Connection::session().await.ok()?;
    let proxy = Proxy::new(
        &conn,
        LEGACY_DAEMON_INTERFACE,
        LEGACY_DAEMON_PATH,
        LEGACY_DAEMON_INTERFACE,
    )
    .await
    .ok()?;
    let (x, y, width, height) = proxy
        .get_property::<(i32, i32, u32, u32)>("FrontendWindowRect")
        .await
        .ok()?;
    let position = proxy.get_property::<i32>("Position").await.ok();
    Some((
        DockEvent::RectChanged {
            x,
            y,
            width,
            height,
        },
        position,
    ))
}

/// 订阅失败或连接丢失的原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DockSubscriptionError {
    /// 无法连接会话总线（无 `DBUS_SESSION_BUS_ADDRESS` 等）。
    Connect(String),
    /// 建立 Proxy 失败。
    Proxy(String),
    /// 订阅信号失败。
    Subscribe(String),
    /// 后台线程已退出（总线断开等）。
    Disconnected,
}

impl std::fmt::Display for DockSubscriptionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Connect(e) => write!(f, "连接会话总线失败：{e}"),
            Self::Proxy(e) => write!(f, "建立 Dock Proxy 失败：{e}"),
            Self::Subscribe(e) => write!(f, "订阅 Dock 信号失败：{e}"),
            Self::Disconnected => write!(f, "D-Bus 订阅线程已退出"),
        }
    }
}

impl std::error::Error for DockSubscriptionError {}

/// 订阅过程中发生的**非致命**问题（不中断订阅，只做记录）。
///
/// 例如某个信号反序列化失败 —— 那通常意味着该信号的实际签名与预期不符，
/// 必须让上层知道（否则会静默丢事件），但不该因此终止整个订阅。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockSubscriptionNotice {
    /// 出问题的信号名。
    pub signal: String,
    /// 问题描述。
    pub detail: String,
}

/// D-Bus 订阅句柄。
///
/// 主线程通过 [`DockSubscription::try_recv`] 拉取事件。句柄丢弃后接收端关闭，
/// 后台线程在**下一次发送事件时**发现通道关闭并退出；若此后该接口再无事件，
/// 线程会保留一条空闲连接（这是有意的：等待中的信号流没有可用的取消通道）。
/// 调用方因此不应频繁重建订阅，`daemon` 侧有失败上限保护。
pub struct DockSubscription {
    rx: Receiver<DockEvent>,
    /// 订阅失败通知（独立通道，避免污染 `DockEvent` 的 `Copy` 语义）。
    err_rx: Receiver<String>,
    /// 权威矩形来源；决定调用方如何解读后续数值。
    rect_source: RectSource,
    /// 各订阅线程的存活标志。
    alive: Vec<std::sync::Arc<AtomicBool>>,
    /// 握手期收到的待处理事件。
    pending: Option<DockEvent>,
}

impl std::fmt::Debug for DockSubscription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DockSubscription")
            .field("rect_source", &self.rect_source)
            .field("alive", &self.is_alive())
            .field("pending", &self.pending)
            .finish_non_exhaustive()
    }
}

impl DockSubscription {
    /// 建立订阅。
    ///
    /// `rect_source` 决定权威值口径：X11 用物理像素
    /// （`FrontendWindowRect`），Wayland 用逻辑像素（`geometry` × 分数缩放）。
    ///
    /// 返回 `Err` 表示连会话总线都连不上（无 D-Bus 环境）；此时调用方
    /// 应走降级链 ③/④（X11 属性反推 / 手动配置）。
    pub fn start(rect_source: RectSource) -> Result<Self, DockSubscriptionError> {
        let (tx, rx) = std::sync::mpsc::channel();
        let (err_tx, err_rx) = std::sync::mpsc::channel();
        let mut alive = Vec::new();

        // 每个接口一个线程：zbus 6 的 Connection 与创建它的运行时绑定，
        // 不能跨线程共享，故每线程自带一条连接。
        let handles = [
            spawn_watchdog(
                Watch::Interface(DAEMON_INTERFACE),
                rect_source,
                &tx,
                &err_tx,
            ),
            spawn_watchdog(Watch::Interface(DOCK_INTERFACE), rect_source, &tx, &err_tx),
            spawn_watchdog(Watch::Properties, rect_source, &tx, &err_tx),
        ];

        // UOS 20 / dde-dock 兼容：老接口没有变更信号，单独用低频轮询补一条
        // 事件源；服务不存在时线程立即退出，不影响上面三路新接口订阅。
        spawn_legacy_poller(&tx);

        for handle in handles {
            match handle {
                Ok((thread, thread_flag)) => {
                    alive.push(thread_flag);
                    // 后台线程句柄直接丢弃：它 detach 后仍会运行，
                    // 进程退出时随进程一起结束。
                    drop(thread);
                }
                Err(err) => return Err(err),
            }
        }

        // 握手：等第一帧事件或失败，最长 200ms。
        // 正常情况下订阅线程在几十毫秒内就能连上并取到首值。
        match rx.recv_timeout(std::time::Duration::from_millis(200)) {
            Ok(event) => Ok(Self {
                rx,
                err_rx,
                rect_source,
                alive,
                pending: Some(event),
            }),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // 超时但没有失败 → 订阅线程可能还在连接中。
                // 若错误通道已有内容，则说明确实失败了。
                match err_rx.try_recv() {
                    Ok(msg) => Err(DockSubscriptionError::Connect(msg)),
                    Err(_) => Ok(Self {
                        rx,
                        err_rx,
                        rect_source,
                        alive,
                        pending: None,
                    }),
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => match err_rx.try_recv() {
                Ok(msg) => Err(DockSubscriptionError::Connect(msg)),
                Err(_) => Err(DockSubscriptionError::Disconnected),
            },
        }
    }

    /// 非阻塞拉取一个事件。
    ///
    /// 返回 `Ok(None)` 表示当前没有待处理事件（不是错误）。
    ///
    /// 需要 `&mut self`：握手期可能缓存了一个事件，取出后要清空槽位。
    pub fn try_recv(&mut self) -> Result<Option<DockEvent>, DockSubscriptionError> {
        if let Some(event) = self.pending.take() {
            return Ok(Some(event));
        }
        match self.rx.try_recv() {
            Ok(event) => Ok(Some(event)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err(DockSubscriptionError::Disconnected),
        }
    }

    /// 权威矩形来源。
    #[must_use]
    pub const fn rect_source(&self) -> RectSource {
        self.rect_source
    }

    /// 所有订阅线程是否仍在运行。
    #[must_use]
    pub fn is_alive(&self) -> bool {
        self.alive.iter().all(|f| f.load(Ordering::SeqCst))
    }

    /// 拉取订阅失败通知（若有）。
    ///
    /// 订阅线程在**建立连接之后**才可能失败（如总线中途断开），
    /// 这类失败不会让 `start` 返回 `Err`，只能异步通知。
    #[must_use]
    pub fn try_recv_failure(&self) -> Option<String> {
        self.err_rx.try_recv().ok()
    }
}

/// 订阅目标：一个接口的显式信号，或属性变更信号。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Watch {
    /// 某个业务接口的显式信号（`receive_all_signals`）。
    Interface(&'static str),
    /// `org.freedesktop.DBus.Properties.PropertiesChanged`。
    Properties,
}

impl Watch {
    /// 线程名后缀。
    const fn name(self) -> &'static str {
        match self {
            Self::Interface(iface) => iface,
            Self::Properties => "Properties",
        }
    }
}

/// 启动一个订阅线程。
fn spawn_watchdog(
    watch: Watch,
    rect_source: RectSource,
    tx: &Sender<DockEvent>,
    err_tx: &Sender<String>,
) -> Result<(std::thread::JoinHandle<()>, std::sync::Arc<AtomicBool>), DockSubscriptionError> {
    let tx = tx.clone();
    let err_tx = err_tx.clone();
    let alive = std::sync::Arc::new(AtomicBool::new(true));
    let alive_thread = alive.clone();

    let thread = std::thread::Builder::new()
        .name(format!("deepin-spectrum-dbus-{}", watch.name()))
        .spawn(move || {
            let result = zbus::block_on(watch_stream(watch, rect_source, tx));
            if let Err(err) = result {
                eprintln!("[dock_geom] 订阅结束：{err}");
                let _ = err_tx.send(err.to_string());
            }
            alive_thread.store(false, Ordering::SeqCst);
        })
        .map_err(|e| DockSubscriptionError::Connect(format!("无法创建订阅线程：{e}")))?;

    Ok((thread, alive))
}

/// 订阅线程主体：按 [`Watch`] 分派。
async fn watch_stream(
    watch: Watch,
    rect_source: RectSource,
    tx: Sender<DockEvent>,
) -> Result<(), DockSubscriptionError> {
    match watch {
        Watch::Interface(interface) => watch_interface(interface, rect_source, tx).await,
        Watch::Properties => watch_properties(rect_source, tx).await,
    }
}

/// 订阅某个业务接口的**显式信号**。
async fn watch_interface(
    interface: &'static str,
    rect_source: RectSource,
    tx: Sender<DockEvent>,
) -> Result<(), DockSubscriptionError> {
    let conn = zbus::Connection::session()
        .await
        .map_err(|e| DockSubscriptionError::Connect(e.to_string()))?;

    let proxy = Proxy::new(&conn, interface, DOCK_OBJECT_PATH, interface)
        .await
        .map_err(|e| DockSubscriptionError::Proxy(e.to_string()))?;

    let authoritative = is_authoritative_for(interface, rect_source);

    // 首值：只由权威接口提供，避免物理/逻辑两个口径同时涌入同一个通道。
    if authoritative && let Some(event) = read_authoritative_rect(&proxy, rect_source).await {
        let _ = tx.send(event);
    }

    // 首值：图标数。`pluginsChanged` 只在变化时发；不在订阅时主动取一次，
    // 空白区宽度就要等用户第一次增删图标才能算出来（可见的功能缺失）。
    if interface == DOCK_INTERFACE
        && let Some(count) = plugin_count(&proxy).await
    {
        let _ = tx.send(DockEvent::PluginsChanged(count));
    }

    let mut stream = proxy
        .receive_all_signals()
        .await
        .map_err(|e| DockSubscriptionError::Subscribe(e.to_string()))?;

    while let Some(msg) = stream.next().await {
        let Some(member) = msg.header().member().map(ToString::to_string) else {
            continue;
        };

        // `pluginsChanged` 不带参数，图标数只能调 `plugins()` 数 visible==true
        // （F100：`GetLoadedPlugins()` 实测恒空，不可用）。
        let event = if member == "pluginsChanged" {
            plugin_count(&proxy).await.map(DockEvent::PluginsChanged)
        } else {
            decode_event(&member, &msg, authoritative)
        };

        // 主线程已退出 → 无需继续。
        if let Some(event) = event
            && tx.send(event).is_err()
        {
            return Ok(());
        }
    }

    Ok(())
}

/// 订阅 `PropertiesChanged`，用于**属性形态的几何变化**。
///
/// 这一路是**逻辑模式（Wayland）的唯一矩形来源**：`geometry` 是属性，
/// 没有专属的 `GeometryChanged` 信号（实测 introspect 确认，只有
/// `emits-change` 标记）。而 `receive_all_signals` 的匹配规则绑定了
/// proxy 自身的接口名，收不到 `org.freedesktop.DBus.Properties` 发出的
/// `PropertiesChanged` —— 所以必须用独立 Proxy 单独订阅。
///
/// 收到后**回读权威属性**而不是解析 variant 载荷：少一层类型推断，
/// 且保证拿到的必是权威值（设计文档 §3 步骤 2）。
async fn watch_properties(
    rect_source: RectSource,
    tx: Sender<DockEvent>,
) -> Result<(), DockSubscriptionError> {
    let conn = zbus::Connection::session()
        .await
        .map_err(|e| DockSubscriptionError::Connect(e.to_string()))?;

    // 权威属性所在的接口。
    let owner = match rect_source {
        RectSource::Physical => DAEMON_INTERFACE,
        RectSource::Logical => DOCK_INTERFACE,
    };
    let owner_proxy = Proxy::new(&conn, owner, DOCK_OBJECT_PATH, owner)
        .await
        .map_err(|e| DockSubscriptionError::Proxy(e.to_string()))?;

    let props_proxy = Proxy::new(
        &conn,
        owner,
        DOCK_OBJECT_PATH,
        "org.freedesktop.DBus.Properties",
    )
    .await
    .map_err(|e| DockSubscriptionError::Proxy(e.to_string()))?;

    let mut stream = props_proxy
        .receive_signal("PropertiesChanged")
        .await
        .map_err(|e| DockSubscriptionError::Subscribe(e.to_string()))?;

    while let Some(msg) = stream.next().await {
        // `PropertiesChanged(sa{sv}as)`：接口名 / 变更字典 / 失效列表。
        // 只需要接口名做过滤，载荷一律回读权威属性 —— 故只解析第一个字段。
        let Ok(iface) = msg.body().deserialize::<(String,)>().map(|(s,)| s) else {
            // 载荷结构不符（可能被其它实现改写），保守起见仍然回读一次。
            if let Some(event) = read_authoritative_rect(&owner_proxy, rect_source).await
                && tx.send(event).is_err()
            {
                return Ok(());
            }
            continue;
        };

        if iface != owner {
            continue;
        }

        if let Some(event) = read_authoritative_rect(&owner_proxy, rect_source).await
            && tx.send(event).is_err()
        {
            return Ok(());
        }
    }

    Ok(())
}

/// 回读权威矩形属性。
async fn read_authoritative_rect(proxy: &Proxy<'_>, rect_source: RectSource) -> Option<DockEvent> {
    let property = match rect_source {
        RectSource::Physical => "FrontendWindowRect",
        RectSource::Logical => "geometry",
    };
    let (x, y, w, h) = proxy
        .get_property::<(i32, i32, i32, i32)>(property)
        .await
        .ok()?;
    Some(DockEvent::RectChanged {
        x,
        y,
        width: u32::try_from(w).unwrap_or(0),
        height: u32::try_from(h).unwrap_or(0),
    })
}

/// 该接口是否为 `rect_source` 的权威矩形来源。
///
/// - X11：`org.deepin.dde.daemon.Dock1.FrontendWindowRect`（物理像素）
/// - Wayland：`org.deepin.dde.Dock1.geometry`（逻辑像素，需乘分数缩放）
fn is_authoritative_for(interface: &str, rect_source: RectSource) -> bool {
    match rect_source {
        RectSource::Physical => interface == DAEMON_INTERFACE,
        RectSource::Logical => interface == DOCK_INTERFACE,
    }
}

/// 统计 `plugins()` 返回中 `visible == true` 的条目数。
///
/// 签名 `a(sssssb)`：5 个字符串 + 1 个 bool（`visible`）。
/// 实测 18 条 / 8 条 visible——**必须数 visible**，不能直接用数组长度（F100）。
async fn plugin_count(proxy: &Proxy<'_>) -> Option<usize> {
    let reply = proxy.call_method("plugins", &()).await.ok()?;
    let entries: Vec<(String, String, String, String, String, bool)> =
        reply.body().deserialize().ok()?;
    Some(entries.iter().filter(|entry| entry.5).count())
}
fn decode_event(member: &str, msg: &zbus::Message, is_authoritative: bool) -> Option<DockEvent> {
    match member {
        "FrontendWindowRectChanged" => {
            if !is_authoritative {
                // 非权威接口的矩形会与权威值口径冲突，必须丢弃。
                return None;
            }
            let (x, y, w, h) = msg.body().deserialize::<(i32, i32, i32, i32)>().ok()?;
            Some(DockEvent::RectChanged {
                x,
                y,
                width: u32::try_from(w).unwrap_or(0),
                height: u32::try_from(h).unwrap_or(0),
            })
        }
        "WindowSizeFashionChanged" | "WindowSizeEfficientChanged" => {
            let value = msg.body().deserialize::<u32>().ok()?;
            Some(DockEvent::WindowSizeChanged(value))
        }
        "PositionChanged" => {
            let value = msg.body().deserialize::<i32>().ok()?;
            Some(DockEvent::PositionChanged(value))
        }
        "HideModeChanged" => {
            let value = msg.body().deserialize::<i32>().ok()?;
            Some(DockEvent::HideModeChanged(value))
        }
        "DisplayModeChanged" => {
            let value = msg.body().deserialize::<i32>().ok()?;
            Some(DockEvent::DisplayModeChanged(value))
        }
        _ => None,
    }
}
