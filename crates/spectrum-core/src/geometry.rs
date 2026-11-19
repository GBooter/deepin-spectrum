//! 任务栏运行时几何跟踪（**用户一等需求：任务栏高度可随时被用户调整**）。
//!
//! 设计约定见 `docs/任务栏高度自适应设计.md`：
//!
//! - 高度是**运行时量**，唯一权威源是 D-Bus `FrontendWindowRect` / `geometry`；
//!   `config.toml` 的 `dock.height` 与 `DConfig` 的 `Dock_Size` 只在探测失败时兜底。
//! - 几何变化做 16ms 节流合并：一次拖拽的连续事件只触发一次 `surface` 重建。
//! - 非法 rect 一律拒绝并保留上次有效值，计数供 `doctor` 观察，**绝不 panic**。
//! - 不把 X11 的 y / scale / opacity 假设带进 Wayland（实证修正 M14）。

/// 默认节流窗口（毫秒），约一帧 @90Hz。
pub const DEFAULT_THROTTLE_MS: u64 = 16;

/// 屏幕矩形，单位为**物理像素**（来自 D-Bus 时已是物理值）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    /// 左上角 x。
    pub x: i32,
    /// 左上角 y。
    pub y: i32,
    /// 宽度。
    pub width: u32,
    /// 高度（★ 用户随时可能调整，禁止硬编码）。
    pub height: u32,
}

impl Rect {
    /// 构造。
    #[must_use]
    pub const fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// 宽高是否有效；负坐标允许（多屏左侧屏幕），宽高为 0 视为无效。
    #[must_use]
    pub const fn is_valid(self) -> bool {
        self.width > 0 && self.height > 0
    }

    /// 右边界（饱和加法，避免多屏极端坐标溢出）。
    #[must_use]
    pub fn right(self) -> i32 {
        self.x
            .saturating_add(i32::try_from(self.width).unwrap_or(i32::MAX))
    }

    /// 下边界（饱和加法）。
    #[must_use]
    pub fn bottom(self) -> i32 {
        self.y
            .saturating_add(i32::try_from(self.height).unwrap_or(i32::MAX))
    }

    /// 逻辑像素 → 物理像素（F35 分数缩放；用四舍五入，往返误差 ≤ 1px）。
    #[must_use]
    pub fn scaled(self, scale: f32) -> Self {
        let s = if scale.is_finite() && scale > 0.0 {
            scale
        } else {
            1.0
        };
        Self::new(
            (self.x as f32 * s).round() as i32,
            (self.y as f32 * s).round() as i32,
            (self.width as f32 * s).round().max(1.0) as u32,
            (self.height as f32 * s).round().max(1.0) as u32,
        )
    }

    /// 物理像素 → 逻辑像素。
    #[must_use]
    pub fn unscaled(self, scale: f32) -> Self {
        self.scaled(1.0 / scale)
    }
}

/// 一次几何提交的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeometryEvent {
    /// 与已提交值相同，无需任何动作。
    NoChange,
    /// 有变化但仍在节流窗口内，等待合并。
    Pending,
    /// 节流窗口结束，可以重建 surface。
    Commit(Rect),
    /// 非法 rect，已保留上次有效值。
    Rejected,
}

/// 几何跟踪器（纯逻辑，不触碰任何窗口系统 API）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeometryTracker {
    committed: Option<Rect>,
    latest: Option<Rect>,
    first_change_ms: Option<u64>,
    throttle_ms: u64,
    resize_count: u64,
    invalid_count: u64,
}

impl GeometryTracker {
    /// 以节流窗口构造；用 [`DEFAULT_THROTTLE_MS`] 即可。
    #[must_use]
    pub fn new(throttle_ms: u64) -> Self {
        Self {
            committed: None,
            latest: None,
            first_change_ms: None,
            throttle_ms,
            resize_count: 0,
            invalid_count: 0,
        }
    }

    /// 喂入一次运行时观测值（D-Bus 信号 / 轮询结果）。
    pub fn submit(&mut self, rect: Rect, now_ms: u64) -> GeometryEvent {
        if !rect.is_valid() {
            self.invalid_count += 1;
            return GeometryEvent::Rejected;
        }
        self.latest = Some(rect);
        if self.committed == Some(rect) {
            self.first_change_ms = None;
            return GeometryEvent::NoChange;
        }
        if self.first_change_ms.is_none() {
            self.first_change_ms = Some(now_ms);
        }
        GeometryEvent::Pending
    }

    /// 每帧调用；节流窗口结束后返回一次 [`GeometryEvent::Commit`]。
    ///
    /// 窗口内持续变化时**不重置**起始时间，保证拖拽结束后最多再等一帧就提交，
    /// 合并结果是最后一次观测值（不会用中间高度建窗）。
    pub fn tick(&mut self, now_ms: u64) -> GeometryEvent {
        let Some(first) = self.first_change_ms else {
            return GeometryEvent::NoChange;
        };
        let Some(rect) = self.latest else {
            return GeometryEvent::NoChange;
        };
        if self.committed == Some(rect) {
            self.first_change_ms = None;
            return GeometryEvent::NoChange;
        }
        if now_ms.saturating_sub(first) < self.throttle_ms {
            return GeometryEvent::Pending;
        }
        self.committed = Some(rect);
        self.first_change_ms = None;
        self.resize_count += 1;
        GeometryEvent::Commit(rect)
    }

    /// 已提交（可安全用于 surface 尺寸）的 rect。
    #[must_use]
    pub const fn committed(&self) -> Option<Rect> {
        self.committed
    }

    /// 最近一次合法观测值（含尚未提交的，可用于 X11 位移这类廉价跟随）。
    #[must_use]
    pub const fn latest(&self) -> Option<Rect> {
        self.latest
    }

    /// 当前生效高度；没有任何合法观测时返回 `None`，调用方走配置兜底。
    #[must_use]
    pub fn height(&self) -> Option<u32> {
        self.latest.map(|r| r.height)
    }

    /// 重建次数，供 `doctor` 输出 `resize_count`。
    #[must_use]
    pub const fn resize_count(&self) -> u64 {
        self.resize_count
    }

    /// 非法几何次数，供 `doctor` 输出 `invalid_geometry_count`。
    #[must_use]
    pub const fn invalid_count(&self) -> u64 {
        self.invalid_count
    }

    /// 清空（会话切换 / 屏幕重配置时调用）。
    pub fn reset(&mut self) {
        self.committed = None;
        self.latest = None;
        self.first_change_ms = None;
    }
}

impl Default for GeometryTracker {
    fn default() -> Self {
        Self::new(DEFAULT_THROTTLE_MS)
    }
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_THROTTLE_MS, GeometryEvent, GeometryTracker, Rect};

    #[test]
    fn first_valid_rect_is_committed_after_throttle() {
        let mut t = GeometryTracker::default();
        let r = Rect::new(0, 1921, 3200, 78);
        assert_eq!(t.submit(r, 0), GeometryEvent::Pending);
        assert_eq!(t.tick(8), GeometryEvent::Pending);
        assert_eq!(t.tick(16), GeometryEvent::Commit(r));
        assert_eq!(t.committed(), Some(r));
        assert_eq!(t.height(), Some(78));
        assert_eq!(t.resize_count(), 1);
        assert_eq!(t.tick(32), GeometryEvent::NoChange);
    }

    /// 用户把任务栏高度从 78 调到别的任意值，必须跟随且不写死常量。
    #[test]
    fn user_adjusted_heights_are_followed() {
        let mut t = GeometryTracker::default();
        let base = Rect::new(0, 1921, 3200, 78);
        t.submit(base, 0);
        t.tick(16);

        for (h, now) in [(60, 1_000), (105, 2_000), (45, 3_000), (200, 4_000)] {
            let r = Rect::new(0, 2_000 - i32::try_from(h).unwrap_or(0), 3200, h);
            assert_eq!(t.submit(r, now), GeometryEvent::Pending);
            assert_eq!(t.tick(now + DEFAULT_THROTTLE_MS), GeometryEvent::Commit(r));
            assert_eq!(t.height(), Some(h), "高度 {h} 应被采用");
        }
        assert_eq!(t.resize_count(), 5, "首次 + 4 次调整");
    }

    /// 一次拖拽的连续几何事件只应触发一次重建，且取最后一个值。
    #[test]
    fn throttling_merges_burst_into_single_commit() {
        let mut t = GeometryTracker::default();
        let first = Rect::new(0, 1921, 3200, 78);
        t.submit(first, 0);
        t.tick(16);
        let before = t.resize_count();

        for (i, h) in [79, 82, 88, 95, 101].into_iter().enumerate() {
            let now = 1_000 + i as u64 * 3;
            assert_eq!(
                t.submit(Rect::new(0, 2_000 - h, 3200, h as u32), now),
                GeometryEvent::Pending
            );
        }
        assert_eq!(t.tick(1_010), GeometryEvent::Pending);
        let last = Rect::new(0, 2_000 - 101, 3200, 101);
        assert_eq!(t.tick(1_016), GeometryEvent::Commit(last));
        assert_eq!(t.resize_count(), before + 1, "突发变化只重建一次");
        assert_eq!(t.height(), Some(101));
    }

    #[test]
    fn invalid_rect_keeps_last_valid_and_counts() {
        let mut t = GeometryTracker::default();
        let good = Rect::new(0, 1921, 3200, 78);
        t.submit(good, 0);
        t.tick(16);

        for bad in [Rect::new(0, 0, 0, 78), Rect::new(0, 0, 3200, 0)] {
            assert_eq!(t.submit(bad, 100), GeometryEvent::Rejected);
        }
        assert_eq!(t.invalid_count(), 2);
        assert_eq!(t.latest(), Some(good), "非法值不得覆盖上次有效值");
        assert_eq!(t.height(), Some(78));
        assert_eq!(t.tick(1_000), GeometryEvent::NoChange);
    }

    #[test]
    fn returning_to_committed_value_cancels_pending_commit() {
        let mut t = GeometryTracker::default();
        let a = Rect::new(0, 1921, 3200, 78);
        t.submit(a, 0);
        t.tick(16);
        t.submit(Rect::new(0, 1900, 3200, 100), 100);
        assert_eq!(t.submit(a, 105), GeometryEvent::NoChange);
        assert_eq!(t.tick(200), GeometryEvent::NoChange);
        assert_eq!(t.resize_count(), 1);
    }

    /// ★ F35：物理↔逻辑换算往返误差 ≤ 1px（1.75 / 1.25 / 2.0）。
    #[test]
    fn fractional_scale_roundtrip_within_one_pixel() {
        for scale in [1.75_f32, 1.25, 2.0, 1.0] {
            let logical = Rect::new(0, 1098, 1829, 45);
            let physical = logical.scaled(scale);
            let back = physical.unscaled(scale);
            for (name, a, b) in [
                ("x", logical.x, back.x),
                ("y", logical.y, back.y),
                (
                    "w",
                    i32::try_from(logical.width).unwrap_or(0),
                    i32::try_from(back.width).unwrap_or(0),
                ),
                (
                    "h",
                    i32::try_from(logical.height).unwrap_or(0),
                    i32::try_from(back.height).unwrap_or(0),
                ),
            ] {
                assert!((a - b).abs() <= 1, "scale={scale} {name}: {a} vs {b}");
            }
        }
        // 本机实测：45 逻辑 @1.75 → 79（D-Bus 实测 78，属合成器取整差异，见设计文档）
        let p = Rect::new(0, 1098, 1829, 45).scaled(1.75);
        assert!((i32::try_from(p.height).unwrap_or(0) - 78).abs() <= 1);
    }

    /// M14：同一高度在 X11 / Treeland 下 y 不同；跟踪器不得对 y 做假设。
    #[test]
    fn cross_session_y_is_treated_as_opaque_data() {
        let x11 = Rect::new(0, 1921, 3200, 78);
        let treeland = Rect::new(0, 1919, 3200, 78);
        let mut t = GeometryTracker::default();
        t.submit(x11, 0);
        assert_eq!(t.tick(16), GeometryEvent::Commit(x11));
        t.submit(treeland, 100);
        assert_eq!(t.tick(116), GeometryEvent::Commit(treeland));
        assert_eq!(t.latest().map(|r| r.y), Some(1919));
    }

    #[test]
    fn reset_clears_geometry_but_keeps_counters() {
        let mut t = GeometryTracker::default();
        t.submit(Rect::new(0, 0, 100, 45), 0);
        t.tick(16);
        t.reset();
        assert_eq!(t.committed(), None);
        assert_eq!(t.latest(), None);
        assert_eq!(t.resize_count(), 1);
    }

    #[test]
    fn rect_edges_saturate() {
        let r = Rect::new(i32::MAX - 2, i32::MAX - 2, 100, 100);
        assert_eq!(r.right(), i32::MAX);
        assert_eq!(r.bottom(), i32::MAX);
    }
}
