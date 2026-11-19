//! 叠加层后端抽象（§5.1 `OverlayBackend`）。
//!
//! 本模块**零外部依赖**：只定义几何类型、错误类型与 trait，
//! 具体实现（`x11` / `wayland`）在各自 feature 下提供。
//!
//! 设计约束来自《任务栏高度自适应设计》：
//!
//! - 几何是**运行时量**，`set_geometry` 必须在运行期可反复调用（§3 步骤 5）；
//! - 尺寸变化后**必须重新应用输入穿透**（§3 步骤 8 / F77），
//!   否则拖过高度的叠加层会突然挡住鼠标；
//! - 非法几何一律拒绝并报错，**绝不下传给窗口系统**（§3 步骤 3）。

use spectrum_core::geometry::Rect;

/// 叠加层几何，单位为 X11 的原生长度类型。
///
/// 用 `i16`/`u16` 而不是 `i32`/`u32`，是因为 X11 协议本身就是这个宽度；
/// 在这里一次性转换比在每个协议调用处反复 `try_from` 更安全。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OverlayGeometry {
    /// 左上角 x（可为负，多屏左侧）。
    pub x: i16,
    /// 左上角 y。
    pub y: i16,
    /// 宽度（像素）。
    pub width: u16,
    /// 高度（像素）。★ 用户随时可能调整，禁止硬编码。
    pub height: u16,
}

impl OverlayGeometry {
    /// 构造。
    #[must_use]
    pub const fn new(x: i16, y: i16, width: u16, height: u16) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// 宽高是否有效（X11 不允许 0 尺寸窗口）。
    #[must_use]
    pub const fn is_valid(self) -> bool {
        self.width > 0 && self.height > 0
    }

    /// 由 core 的 [`Rect`]（i32/u32）转换，越界视为非法。
    ///
    /// # Errors
    ///
    /// 坐标或尺寸超出 X11 的 i16/u16 表示范围时返回 [`OverlayError::OutOfRange`]。
    pub fn try_from_rect(rect: Rect) -> Result<Self, OverlayError> {
        let x = i16::try_from(rect.x).map_err(|_| OverlayError::OutOfRange)?;
        let y = i16::try_from(rect.y).map_err(|_| OverlayError::OutOfRange)?;
        let width = u16::try_from(rect.width).map_err(|_| OverlayError::OutOfRange)?;
        let height = u16::try_from(rect.height).map_err(|_| OverlayError::OutOfRange)?;
        let geo = Self::new(x, y, width, height);
        if !geo.is_valid() {
            return Err(OverlayError::InvalidGeometry {
                width: geo.width,
                height: geo.height,
            });
        }
        Ok(geo)
    }

    /// 转回 core 的 [`Rect`]。
    #[must_use]
    pub fn to_rect(self) -> Rect {
        Rect::new(
            i32::from(self.x),
            i32::from(self.y),
            u32::from(self.width),
            u32::from(self.height),
        )
    }

    /// 与另一几何的差异是否超过容差（像素）。用于断言「表面与权威 rect 一致（±1px）」。
    #[must_use]
    pub fn differs_by(self, other: Self, tolerance: u16) -> bool {
        self.x.abs_diff(other.x) > tolerance
            || self.y.abs_diff(other.y) > tolerance
            || self.width.abs_diff(other.width) > tolerance
            || self.height.abs_diff(other.height) > tolerance
    }
}

/// 叠加层后端的统一错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverlayError {
    /// 无法连接窗口系统。
    Connect(String),
    /// 找不到合适的 visual（depth-32 `TrueColor`）。
    NoArgbVisual,
    /// X11 调用失败。
    X11 {
        /// 失败的操作名。
        operation: &'static str,
        /// 错误描述。
        detail: String,
    },
    /// Wayland 调用失败。
    Wayland {
        /// 失败的操作名。
        operation: &'static str,
        /// 错误描述。
        detail: String,
    },
    /// 几何超出协议可表示范围。
    OutOfRange,
    /// 几何非法（宽或高为 0）。
    InvalidGeometry {
        /// 传入的宽度。
        width: u16,
        /// 传入的高度。
        height: u16,
    },
    /// 几何超出屏幕范围（会建出不可见或越界的窗口）。
    GeometryOutsideScreen {
        /// 传入的几何。
        geometry: OverlayGeometry,
        /// 屏幕宽。
        screen_width: u16,
        /// 屏幕高。
        screen_height: u16,
    },
    /// 像素缓冲与声明的尺寸不匹配。
    ///
    /// 这类错误必须在**下发到服务端之前**拦住：长度不符的 `put_image`
    /// 会让服务端读越界或画出错位图像，后者更难查（画面"看起来有东西"）。
    PixelBufferMismatch {
        /// 声明宽度。
        width: u16,
        /// 声明高度。
        height: u16,
        /// 实际像素数。
        pixels: usize,
        /// 期望像素数（`width * height`）。
        expected: usize,
    },
}

impl std::fmt::Display for OverlayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Connect(e) => write!(f, "连接窗口系统失败：{e}"),
            Self::NoArgbVisual => write!(f, "找不到 depth-32 TrueColor visual（F1/F62）"),
            Self::X11 { operation, detail } => write!(f, "X11 操作 {operation} 失败：{detail}"),
            Self::Wayland { operation, detail } => {
                write!(f, "Wayland 操作 {operation} 失败：{detail}")
            }
            Self::OutOfRange => write!(f, "几何超出协议可表示范围"),
            Self::InvalidGeometry { width, height } => {
                write!(f, "非法几何 {width}x{height}（宽高必须 > 0）")
            }
            Self::GeometryOutsideScreen {
                geometry,
                screen_width,
                screen_height,
            } => write!(
                f,
                "几何 {}x{} 超出屏幕 {screen_width}x{screen_height}",
                geometry.width, geometry.height
            ),
            Self::PixelBufferMismatch {
                width,
                height,
                pixels,
                expected,
            } => write!(
                f,
                "像素缓冲长度不符：声明 {width}x{height}（期望 {expected}），实际 {pixels}"
            ),
        }
    }
}

impl std::error::Error for OverlayError {}

/// 叠加层后端（§5.1）。X11 与 Wayland 各自实现。
///
/// 约定：
/// - 所有方法都**不得 panic**；非法输入返回错误。
/// - `set_geometry` 内部负责「尺寸变化后重新应用输入穿透」。
/// - 实现的 `Drop` 必须销毁窗口，绝不在用户桌面留下残留。
pub trait OverlayBackend {
    /// 读取当前实际生效的几何（来自窗口系统，不是缓存）。
    ///
    /// # Errors
    ///
    /// 窗口已被销毁或查询失败时返回错误。
    fn geometry(&self) -> Result<OverlayGeometry, OverlayError>;

    /// 应用新几何。
    ///
    /// 实现必须：① 拒绝非法几何；② 只在真变化时才下发请求；
    /// ③ 尺寸变化后重新应用空输入区。
    ///
    /// # Errors
    ///
    /// 几何非法或窗口系统调用失败时返回错误。
    fn set_geometry(&mut self, geometry: OverlayGeometry) -> Result<(), OverlayError>;

    /// 显示（`map`）。
    ///
    /// # Errors
    ///
    /// 窗口系统调用失败时返回错误。
    fn show(&mut self) -> Result<(), OverlayError>;

    /// 隐藏（`unmap`）。
    ///
    /// # Errors
    ///
    /// 窗口系统调用失败时返回错误。
    fn hide(&mut self) -> Result<(), OverlayError>;

    /// 把累积的请求刷到窗口系统。
    ///
    /// # Errors
    ///
    /// 窗口系统调用失败时返回错误。
    fn flush(&mut self) -> Result<(), OverlayError>;
}

#[cfg(test)]
mod tests {
    use super::{OverlayError, OverlayGeometry};
    use spectrum_core::geometry::Rect;

    #[test]
    fn rect_roundtrip_preserves_values() {
        let rect = Rect::new(0, 1876, 3200, 124);
        let geo = OverlayGeometry::try_from_rect(rect).expect("合法 rect");
        assert_eq!(geo, OverlayGeometry::new(0, 1876, 3200, 124));
        assert_eq!(geo.to_rect(), rect);
    }

    #[test]
    fn negative_origin_is_allowed_for_multi_monitor() {
        let rect = Rect::new(-1920, 0, 1920, 78);
        let geo = OverlayGeometry::try_from_rect(rect).expect("负坐标应允许");
        assert_eq!(geo.x, -1920);
        assert!(geo.is_valid());
    }

    #[test]
    fn zero_size_is_rejected() {
        for rect in [Rect::new(0, 0, 0, 78), Rect::new(0, 0, 3200, 0)] {
            assert!(matches!(
                OverlayGeometry::try_from_rect(rect),
                Err(OverlayError::InvalidGeometry { .. })
            ));
        }
    }

    #[test]
    fn out_of_range_is_rejected() {
        // u32 宽度超过 u16::MAX
        let rect = Rect::new(0, 0, 70_000, 78);
        assert_eq!(
            OverlayGeometry::try_from_rect(rect),
            Err(OverlayError::OutOfRange)
        );
        // x 超过 i16::MAX
        let rect = Rect::new(40_000, 0, 100, 78);
        assert_eq!(
            OverlayGeometry::try_from_rect(rect),
            Err(OverlayError::OutOfRange)
        );
    }

    /// ★ T-H1 判据：表面 rect 与权威 rect 一致（±1px）。
    #[test]
    fn tolerance_check_matches_assertion_rule() {
        let target = OverlayGeometry::new(0, 1876, 3200, 124);
        // 差 1px 以内视为一致
        assert!(!target.differs_by(OverlayGeometry::new(0, 1877, 3200, 125), 1));
        // 差 2px 就不一致
        assert!(target.differs_by(OverlayGeometry::new(0, 1878, 3200, 124), 1));
        assert!(target.differs_by(OverlayGeometry::new(0, 1876, 3200, 122), 1));
    }

    /// 高度必须能跟随任意合法值，不允许写死。
    #[test]
    fn arbitrary_heights_are_representable() {
        for h in [45_u16, 60, 78, 124, 173, 400] {
            let rect = Rect::new(0, 2000 - i32::from(h), 3200, u32::from(h));
            let geo = OverlayGeometry::try_from_rect(rect).expect("合法高度");
            assert_eq!(geo.height, h);
        }
    }
}
