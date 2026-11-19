//! D-Bus 任务栏信号 → 几何动作的**纯逻辑**桥接层。
//!
//! 本模块是 `GeometryTracker`（见 [`crate::geometry`]）的上层：把平台层收到的 D-Bus 信号
//! 翻译成「要不要重建表面 / 要不要重排 / 要不要回读属性」的决策。
//!
//! 设计依据见 `docs/任务栏高度自适应设计.md`：
//!
//! - **信号驱动而非轮询**（§2）：`FrontendWindowRectChanged` 等信号齐全时，
//!   500ms 轮询属于纯浪费 CPU，与「静音 ≈0% CPU」目标冲突。
//! - **16ms 节流合并**（§3.1）：一次拖拽产生的高频信号只触发一次重建。
//! - **非法值绝不 panic**（§3.3）：非法矩形回退上次有效值并计数。
//! - **差分而非无脑重建**（§3.4）：只有矩形真变了才动表面尺寸；
//!   图标数变化只重算空白区宽度。
//! - **属性回读兜底**（§3.2）：只收到 `WindowSizeFashionChanged` 而没收到
//!   矩形变更信号时，需要主动回读一次权威矩形，而不是干等。
//!
//! 本模块**零外部依赖**：不碰 D-Bus、不碰窗口系统，只做状态机与判定，
//! 因此可以在没有图形会话的环境里完整单测。

use crate::geometry::{GeometryEvent, GeometryTracker, Rect};

/// 权威矩形的来源，决定了后续的坐标换算口径。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RectSource {
    /// D-Bus `org.deepin.dde.daemon.Dock1.FrontendWindowRect`，**物理像素**。
    ///
    /// X11 会话下这是唯一权威值（F35：物理与逻辑混用会错位 1.75 倍）。
    Physical,
    /// D-Bus `org.deepin.dde.Dock1.geometry`，**逻辑像素**，需乘分数缩放。
    ///
    /// Wayland / Treeland 下 `FrontendWindowRect` 与 `geometry` 的 y 坐标不同，
    /// 且 `GetScaleFactor` 不可用（M14），故逻辑矩形需配 `wp_fractional_scale_manager_v1`。
    Logical,
}

impl RectSource {
    /// `doctor` 输出用的稳定字符串。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Physical => "frontend_window_rect_physical",
            Self::Logical => "geometry_logical",
        }
    }
}

/// 平台层转发过来的任务栏信号（已按接口分类，值已完成类型转换）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DockEvent {
    /// `FrontendWindowRectChanged(iiii)`：物理矩形变化。
    RectChanged {
        /// 左上角 x。
        x: i32,
        /// 左上角 y。
        y: i32,
        /// 宽度（物理像素）。
        width: u32,
        /// 高度（物理像素）。
        height: u32,
    },
    /// `PositionChanged(i)`：停靠位置变化（top/right/bottom/left）。
    PositionChanged(i32),
    /// `HideModeChanged(i)`：自动隐藏模式变化。
    HideModeChanged(i32),
    /// `DisplayModeChanged(i)`：显示模式（紧凑 / 时尚等）变化。
    DisplayModeChanged(i32),
    /// `WindowSizeFashionChanged(u)` / `WindowSizeEfficientChanged(u)`：
    /// 任务栏尺寸模式的**逻辑**值变化。
    WindowSizeChanged(u32),
    /// `pluginsChanged()`：插件（图标）集合变化。
    PluginsChanged(usize),
}

/// 订阅失败事件。
///
/// 走**独立通道**而非塞进 [`DockEvent`]，是为了让 [`DockEvent`] 保持 `Copy`
/// （它被高频复制传递，塞入 `String` 会付出克隆代价且破坏 `Copy` 语义）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockSubscriptionFailure {
    /// 失败描述。
    pub detail: String,
}

/// 需要「重排但不一定改表面尺寸」的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayoutReason {
    /// 停靠位置变了 → 坐标换算、锚点、`useColumnLayout` 都要重算（F131）。
    Position,
    /// 自动隐藏模式变了 → 状态机淡入淡出（F129）。
    HideMode,
    /// 显示模式变了 → 紧凑模式要置灰 `LeftBlank` / `RightBlank`。
    DisplayMode,
    /// 尺寸模式值变了 → 需要回读权威矩形并重算空白区。
    WindowSize,
    /// 图标数变了 → 只重算空白区宽度，**不动表面尺寸**（F52 / F100）。
    Plugins,
}

impl RelayoutReason {
    /// `doctor` 输出用的稳定字符串。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Position => "position",
            Self::HideMode => "hide_mode",
            Self::DisplayMode => "display_mode",
            Self::WindowSize => "window_size",
            Self::Plugins => "plugins",
        }
    }
}

/// 一次信号处理的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DockOutcome {
    /// 无事发生。
    NoChange,
    /// 已变化但在 16ms 节流窗口内，等待合并。
    Pending,
    /// 矩形真变了 → 需要按新矩形重建 / resize 表面。
    Commit(Rect),
    /// 需要重排（不一定改表面尺寸）。
    Relayout(RelayoutReason),
    /// 只收到了「尺寸变了」类信号、尚未收到矩形变更信号 → 需要回读权威矩形。
    NeedsRefresh,
    /// 非法矩形 → 已保留上次有效值（见 [`DockSignalBridge::invalid_count`]）。
    Rejected,
}

/// `doctor` 输出的任务栏运行时状态（§6.12.4）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockDoctor {
    /// 当前生效的任务栏高度（物理像素）；没有任何合法观测时为 `None`。
    pub height_runtime: Option<u32>,
    /// 权威矩形的来源。
    pub rect_source: &'static str,
    /// 表面重建次数。
    pub resize_count: u64,
    /// 非法几何被拒绝的次数。
    pub invalid_geometry_count: u64,
    /// 可见图标数（来自 `pluginsChanged`）。
    pub icon_count: Option<usize>,
    /// 当前停靠位置。
    pub position: Option<i32>,
    /// 当前自动隐藏模式。
    pub hide_mode: Option<i32>,
    /// 当前显示模式。
    pub display_mode: Option<i32>,
    /// 最近一次重建提交的相对时刻（毫秒），供 `last_resize_ms` 使用。
    pub last_commit_ms: Option<u64>,
}

/// 任务栏信号桥：把 [`DockEvent`] 翻译成几何 / 重排决策。
///
/// 线程模型：本结构**不是**线程安全的，约定只在 main 线程使用
/// （与 §5.2「所有状态变更收敛到 main 线程」一致）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockSignalBridge {
    tracker: GeometryTracker,
    rect_source: RectSource,
    icon_count: Option<usize>,
    position: Option<i32>,
    hide_mode: Option<i32>,
    display_mode: Option<i32>,
    /// 已收到「尺寸变了」信号但还没收到对应矩形变更信号。
    awaiting_rect_refresh: bool,
    /// 最近一次提交（重建）时刻。
    last_commit_ms: Option<u64>,
}

impl DockSignalBridge {
    /// 以给定权威来源构造。
    #[must_use]
    pub fn new(rect_source: RectSource) -> Self {
        Self {
            tracker: GeometryTracker::default(),
            rect_source,
            icon_count: None,
            position: None,
            hide_mode: None,
            display_mode: None,
            awaiting_rect_refresh: false,
            last_commit_ms: None,
        }
    }

    /// 喂入一次信号。
    pub fn submit(&mut self, event: DockEvent, now_ms: u64) -> DockOutcome {
        match event {
            DockEvent::RectChanged {
                x,
                y,
                width,
                height,
            } => {
                // 收到权威矩形 → 之前的「待回读」标记解除。
                self.awaiting_rect_refresh = false;
                let rect = Rect::new(x, y, width, height);
                match self.tracker.submit(rect, now_ms) {
                    GeometryEvent::NoChange => DockOutcome::NoChange,
                    GeometryEvent::Pending => DockOutcome::Pending,
                    GeometryEvent::Commit(committed) => {
                        self.last_commit_ms = Some(now_ms);
                        DockOutcome::Commit(committed)
                    }
                    GeometryEvent::Rejected => DockOutcome::Rejected,
                }
            }
            DockEvent::PositionChanged(pos) => {
                self.position = Some(pos);
                DockOutcome::Relayout(RelayoutReason::Position)
            }
            DockEvent::HideModeChanged(mode) => {
                self.hide_mode = Some(mode);
                DockOutcome::Relayout(RelayoutReason::HideMode)
            }
            DockEvent::DisplayModeChanged(mode) => {
                self.display_mode = Some(mode);
                DockOutcome::Relayout(RelayoutReason::DisplayMode)
            }
            DockEvent::WindowSizeChanged(_) => {
                // 尺寸模式值变了，但**它不是权威值**（F38：配置是请求值）。
                // 必须回读 `FrontendWindowRect` 才能拿到真实高度。
                self.awaiting_rect_refresh = true;
                DockOutcome::Relayout(RelayoutReason::WindowSize)
            }
            DockEvent::PluginsChanged(count) => {
                self.icon_count = Some(count);
                DockOutcome::Relayout(RelayoutReason::Plugins)
            }
        }
    }

    /// 每帧调用；节流窗口结束后产出一次 [`DockOutcome::Commit`]。
    ///
    /// 若处于「待回读权威矩形」状态且本帧没有新矩形到达，
    /// 则返回 [`DockOutcome::NeedsRefresh`]，由调用方回读属性。
    pub fn tick(&mut self, now_ms: u64) -> DockOutcome {
        match self.tracker.tick(now_ms) {
            GeometryEvent::Commit(committed) => {
                self.last_commit_ms = Some(now_ms);
                DockOutcome::Commit(committed)
            }
            GeometryEvent::Pending => DockOutcome::Pending,
            GeometryEvent::Rejected => DockOutcome::Rejected,
            GeometryEvent::NoChange => {
                if self.awaiting_rect_refresh {
                    DockOutcome::NeedsRefresh
                } else {
                    DockOutcome::NoChange
                }
            }
        }
    }

    /// 权威矩形的来源。
    #[must_use]
    pub const fn rect_source(&self) -> RectSource {
        self.rect_source
    }

    /// 当前生效高度；没有合法观测时为 `None`（调用方走配置兜底）。
    #[must_use]
    pub fn height(&self) -> Option<u32> {
        self.tracker.height()
    }

    /// 最近一次合法矩形（含尚未提交的），供 X11 廉价位移这类跟随使用。
    #[must_use]
    pub fn latest_rect(&self) -> Option<Rect> {
        self.tracker.latest()
    }

    /// 表面重建次数。
    #[must_use]
    pub const fn resize_count(&self) -> u64 {
        self.tracker.resize_count()
    }

    /// 非法几何被拒绝的次数。
    #[must_use]
    pub const fn invalid_count(&self) -> u64 {
        self.tracker.invalid_count()
    }

    /// 可见图标数。
    #[must_use]
    pub const fn icon_count(&self) -> Option<usize> {
        self.icon_count
    }

    /// 是否处于「收到尺寸变化信号但未收到矩形变更信号」的待回读状态。
    #[must_use]
    pub const fn awaiting_rect_refresh(&self) -> bool {
        self.awaiting_rect_refresh
    }

    /// 导出 `doctor` 报告。
    #[must_use]
    pub fn doctor(&self) -> DockDoctor {
        DockDoctor {
            height_runtime: self.tracker.height(),
            rect_source: self.rect_source.as_str(),
            resize_count: self.tracker.resize_count(),
            invalid_geometry_count: self.tracker.invalid_count(),
            icon_count: self.icon_count,
            position: self.position,
            hide_mode: self.hide_mode,
            display_mode: self.display_mode,
            last_commit_ms: self.last_commit_ms,
        }
    }
}

impl Default for DockSignalBridge {
    fn default() -> Self {
        Self::new(RectSource::Physical)
    }
}

#[cfg(test)]
mod tests {
    use super::{DockEvent, DockOutcome, DockSignalBridge, RectSource, RelayoutReason};
    use crate::geometry::Rect;

    /// 首个权威矩形应在节流窗口结束后提交。
    #[test]
    fn first_rect_commits_after_throttle() {
        let mut b = DockSignalBridge::default();
        assert_eq!(
            b.submit(
                DockEvent::RectChanged {
                    x: 0,
                    y: 1921,
                    width: 3200,
                    height: 78
                },
                0
            ),
            DockOutcome::Pending
        );
        assert_eq!(b.tick(8), DockOutcome::Pending);
        assert_eq!(
            b.tick(16),
            DockOutcome::Commit(Rect::new(0, 1921, 3200, 78))
        );
        assert_eq!(b.height(), Some(78));
        assert_eq!(b.resize_count(), 1);
    }

    /// ★ 用户一等需求：拖动任务栏改高度，必须跟随且不写死常量。
    #[test]
    fn follows_user_adjusted_height() {
        let mut b = DockSignalBridge::default();
        b.submit(
            DockEvent::RectChanged {
                x: 0,
                y: 1921,
                width: 3200,
                height: 78,
            },
            0,
        );
        b.tick(16);

        for (h, now) in [(60_u32, 1_000_u64), (105, 2_000), (45, 3_000), (200, 4_000)] {
            b.submit(
                DockEvent::RectChanged {
                    x: 0,
                    y: 2_000 - i32::try_from(h).unwrap_or(0),
                    width: 3200,
                    height: h,
                },
                now,
            );
            assert_eq!(
                b.tick(now + 16),
                DockOutcome::Commit(Rect::new(0, 2_000 - i32::try_from(h).unwrap_or(0), 3200, h))
            );
            assert_eq!(b.height(), Some(h), "高度 {h} 应被采用");
        }
        assert_eq!(b.resize_count(), 5, "首次 + 4 次调整");
    }

    /// 一次拖拽的连续信号只重建一次，且取最后一次的值。
    #[test]
    fn burst_is_merged_into_single_commit() {
        let mut b = DockSignalBridge::default();
        b.submit(
            DockEvent::RectChanged {
                x: 0,
                y: 1921,
                width: 3200,
                height: 78,
            },
            0,
        );
        b.tick(16);
        let before = b.resize_count();

        for (i, h) in [79_u32, 82, 88, 95, 101].into_iter().enumerate() {
            let now = 1_000 + u64::try_from(i).unwrap_or(0) * 3;
            assert_eq!(
                b.submit(
                    DockEvent::RectChanged {
                        x: 0,
                        y: 2_000 - i32::try_from(h).unwrap_or(0),
                        width: 3200,
                        height: h
                    },
                    now
                ),
                DockOutcome::Pending
            );
        }
        assert_eq!(b.tick(1_010), DockOutcome::Pending);
        assert_eq!(
            b.tick(1_016),
            DockOutcome::Commit(Rect::new(0, 1_899, 3200, 101))
        );
        assert_eq!(b.resize_count(), before + 1, "突发变化只重建一次");
    }

    /// 非法矩形必须被拒绝，并保留上次有效值。
    #[test]
    fn invalid_rect_is_rejected_and_keeps_last_valid() {
        let mut b = DockSignalBridge::default();
        b.submit(
            DockEvent::RectChanged {
                x: 0,
                y: 1921,
                width: 3200,
                height: 78,
            },
            0,
        );
        b.tick(16);

        assert_eq!(
            b.submit(
                DockEvent::RectChanged {
                    x: 0,
                    y: 0,
                    width: 0,
                    height: 78
                },
                100
            ),
            DockOutcome::Rejected
        );
        assert_eq!(b.invalid_count(), 1);
        assert_eq!(b.height(), Some(78), "非法值不得覆盖上次有效值");
        assert_eq!(b.resize_count(), 1, "非法值不得触发重建");
    }

    /// 只收到尺寸信号时必须回读权威矩形，不能直接把配置值当真。
    #[test]
    fn window_size_signal_requests_rect_refresh() {
        let mut b = DockSignalBridge::default();
        assert_eq!(
            b.submit(DockEvent::WindowSizeChanged(45), 10),
            DockOutcome::Relayout(RelayoutReason::WindowSize)
        );
        assert!(b.awaiting_rect_refresh());
        assert_eq!(b.tick(20), DockOutcome::NeedsRefresh);
        assert_eq!(b.height(), None, "未拿到权威矩形前高度未知");

        // 回读后解除待回读状态。
        b.submit(
            DockEvent::RectChanged {
                x: 0,
                y: 1921,
                width: 3200,
                height: 78,
            },
            30,
        );
        assert!(!b.awaiting_rect_refresh());
        b.tick(46);
        assert_eq!(b.height(), Some(78));
        assert_eq!(b.tick(100), DockOutcome::NoChange);
    }

    /// 图标数变化只触发重排，不改表面尺寸。
    #[test]
    fn plugin_count_change_does_not_resize_surface() {
        let mut b = DockSignalBridge::default();
        b.submit(
            DockEvent::RectChanged {
                x: 0,
                y: 1921,
                width: 3200,
                height: 78,
            },
            0,
        );
        b.tick(16);
        let before = b.resize_count();

        assert_eq!(
            b.submit(DockEvent::PluginsChanged(14), 100),
            DockOutcome::Relayout(RelayoutReason::Plugins)
        );
        assert_eq!(b.icon_count(), Some(14));
        assert_eq!(b.resize_count(), before, "图标数变化不得重建表面");
    }

    /// 停靠位置 / 隐藏模式 / 显示模式各自触发对应重排原因。
    #[test]
    fn layout_signals_map_to_reasons() {
        let mut b = DockSignalBridge::default();
        assert_eq!(
            b.submit(DockEvent::PositionChanged(3), 0),
            DockOutcome::Relayout(RelayoutReason::Position)
        );
        assert_eq!(
            b.submit(DockEvent::HideModeChanged(1), 0),
            DockOutcome::Relayout(RelayoutReason::HideMode)
        );
        assert_eq!(
            b.submit(DockEvent::DisplayModeChanged(2), 0),
            DockOutcome::Relayout(RelayoutReason::DisplayMode)
        );
        let d = b.doctor();
        assert_eq!(d.position, Some(3));
        assert_eq!(d.hide_mode, Some(1));
        assert_eq!(d.display_mode, Some(2));
    }

    /// `doctor` 报告必须带运行时高度、来源与计数。
    #[test]
    fn doctor_reports_runtime_state() {
        let mut b = DockSignalBridge::new(RectSource::Logical);
        b.submit(
            DockEvent::RectChanged {
                x: 0,
                y: 0,
                width: 1829,
                height: 45,
            },
            0,
        );
        b.tick(16);
        b.submit(DockEvent::PluginsChanged(8), 20);

        let d = b.doctor();
        assert_eq!(d.height_runtime, Some(45));
        assert_eq!(d.rect_source, "geometry_logical");
        assert_eq!(d.resize_count, 1);
        assert_eq!(d.invalid_geometry_count, 0);
        assert_eq!(d.icon_count, Some(8));
        assert_eq!(d.last_commit_ms, Some(16));
    }

    /// 物理 / 逻辑来源必须可区分（M14 实证：X11 与 Treeland 的 y 完全不同）。
    #[test]
    fn rect_source_is_reported_distinctly() {
        assert_eq!(
            RectSource::Physical.as_str(),
            "frontend_window_rect_physical"
        );
        assert_eq!(RectSource::Logical.as_str(), "geometry_logical");
    }
}
