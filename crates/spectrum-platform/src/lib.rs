//! `spectrum-platform`：平台抽象层与 X11 / Wayland 后端（§5.1）。
//!
//! F113：`src/x11/` 与 `src/wayland/` 必须是**目录**（不是 x11.rs 单文件），
//! 否则 §S9 回归脚本的 grep 会恒空 → 假 PASS。
//!
//! ## 已落地模块
//!
//! - [`overlay`]：**零依赖**。`OverlayBackend` trait、`OverlayGeometry`、`OverlayError`。
//!   所有后端实现共用的几何契约（含 ±1px 一致性判据与越界拒绝）。
//! - `x11`（**需 `x11` feature**）：depth-32 ARGB 叠加窗口、独立 colormap、
//!   `DOCK` 类型 + `ABOVE`/`SKIP`/`STICKY` 状态、XShape 空输入区；`set_geometry` 后
//!   自动重设输入区（否则拖过高度就开始吃鼠标）。
//! - `dock_geom`（**需 `dbus` feature**）：D-Bus 任务栏几何订阅，订阅
//!   `FrontendWindowRectChanged` 等信号，转成 core 的 `DockEvent`。
//!
//! ## 特性组合
//!
//! | feature | 引入 | 用途 |
//! |---|---|---|
//! | *(默认)* | 无 | 纯逻辑，可离线/CI 编译 |
//! | `dbus` | zbus 6.3 | 任务栏几何信号 |
//! | `x11` | x11rb 0.14 | 叠加窗口 |
//! | `dbus` + `x11` | 两者 | 完整叠加层（跟随真实高度变化） |
//!
//! > 受 feature 控制的模块在默认构建下不存在，因此文档里用行内代码而非
//! > intra-doc 链接引用（否则默认构建的 `RUSTDOCFLAGS=-D warnings`
//! > 会因链接目标缺失而失败）。

#[cfg(feature = "dbus")]
pub mod dock_geom;
#[cfg(feature = "dbus")]
pub mod lock;
pub mod overlay;
#[cfg(feature = "wayland")]
pub mod wayland;
#[cfg(feature = "x11")]
pub mod x11;
