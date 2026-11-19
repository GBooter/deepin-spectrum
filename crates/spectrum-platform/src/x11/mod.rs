//! X11 叠加层后端：depth-32 ARGB 窗口 + 空输入区（§6.4）。
//!
//! 每个修正点都在代码里标注了对应编号，便于对照开发指导文档核验：
//!
//! | 编号 | 约束 |
//! |---|---|
//! | F1 | `create_window` 的 depth 必须显式传 **32**，不能用 `root_depth`（24） |
//! | F62 | visual 必须**过滤** `TrueColor` + `bits_per_rgb==8`，不能 `.next()` 取第一个 |
//! | F28 | `override_redirect=1` 时**不设**任何 `_NET_WM_STATE_*` 状态（互斥）→
//! |      | 受管（DOCK）/ OR 两种模式都支持；OR 下「显示桌面」/ 最小化不再影响叠加层（F124） |
//! | F124b| OR 窗口仍需声明 `_NET_WM_WINDOW_TYPE_DOCK`：kwin 的「显示桌面」效果
//! |      | 会跳过 DOCK 类型，否则桌面会短暂盖住频谱（实测 150~250ms 闪烁） |
//! | F7 | **绝不**设置 `_NET_WM_STRUT` / `_NET_WM_STRUT_PARTIAL` / `BYPASS_COMPOSITOR` |
//! | F77 | `shape_rectangles` 是 **7 参数**：`SO::SET` / `SK::INPUT` / `ClipOrdering::UNSORTED` |
//! | M10 | `ColormapAlloc`（不是 `ColorMapAlloc`）；`change_property*` 在 `wrapper::ConnectionExt` |
//! | M11 | 容器内 X server 只听 abstract socket，x11rb 走文件路径 → 需要桥接 |
//!
//! ## 为什么尺寸变化后要重设输入区
//!
//! `XShape` 的 Input Region 是**相对窗口**的矩形集合。窗口被 `configure_window`
//! 改尺寸后，X server 会按新尺寸截断/扩展原区域，结果不再是「空」。
//! 一旦 Input Region 变成非空，叠加层就会**吃掉鼠标事件**——这正是需求
//! 「不影响任务栏任何鼠标/键盘操作」最危险的失效方式。
//! 因此每次改尺寸后都必须重新下发一次空区域（《任务栏高度自适应设计》§3 步骤 8）。

pub mod fullscreen;

use std::ffi::OsStr;
use std::path::Path;

use crate::overlay::OverlayError;

/// 连接 X server：优先按环境变量 `DISPLAY`；未设置时从 socket 目录推断 display 号。
///
/// ## 为什么需要兜底（登录自启失效的根因）
///
/// systemd user 服务不继承图形会话的 `DISPLAY`，而 `x11rb::connect(None)` 在
/// `DISPLAY` 未设置时会直接报 "$DISPLAY variable not set" → 建窗失败 → daemon
/// 被 `Restart=on-failure` 反复拉起，表现就是"勾选自启后重启没有任何频谱"。
/// 这里在连接失败时扫描 `/tmp/.X11-unix/X<n>` 推断 display，显式重连一次。
///
/// 真机 X server 可能只监听 abstract socket（容器里尤其如此，M11），文件路径
/// socket 不一定存在；这种情况下两个候选都会失败，保持原错误不掩盖真实原因。
fn connect() -> Result<(RustConnection, usize), OverlayError> {
    // 先按环境变量连（DISPLAY 已设置时走这里，行为与原来完全一致）。
    match x11rb::connect(None) {
        Ok(pair) => Ok(pair),
        Err(first_err) => {
            // DISPLAY 没设置才推断；设置了但仍连不上（如 abstract socket）时，
            // 推断出的 display 也大概率连不上，直接返回原错误避免误导。
            if std::env::var_os("DISPLAY").is_some() {
                return Err(OverlayError::Connect(first_err.to_string()));
            }
            let Some(display) = infer_display() else {
                return Err(OverlayError::Connect(format!(
                    "{first_err}（DISPLAY 未设置，且未能从 socket 目录推断）"
                )));
            };
            x11rb::connect(Some(&display)).map_err(|e| {
                OverlayError::Connect(format!(
                    "{first_err}；按推断的 DISPLAY={display} 重连仍失败：{e}"
                ))
            })
        }
    }
}

/// 从 `/tmp/.X11-unix/X<n>` 推断一个可用的 DISPLAY 值（如 `":0"`）。
///
/// 取序号最小的 socket（通常是主屏 `:0`）；目录不存在或没有合法编号时返回 `None`。
fn infer_display() -> Option<String> {
    let entries = std::fs::read_dir("/tmp/.X11-unix").ok()?;
    let mut numbers: Vec<u32> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name();
            let name: &OsStr = name.as_os_str();
            Path::new(name)
                .file_name()
                .and_then(|s| s.to_str())
                .and_then(|s| s.strip_prefix('X'))
                .and_then(|s| s.parse::<u32>().ok())
        })
        .collect();
    numbers.sort_unstable();
    numbers.first().map(|n| format!(":{n}"))
}

/// 当前 X11 主屏尺寸（物理像素）；连不上 X server 时返回 `None`。
///
/// 用于 D-Bus 几何不可用时的兜底：宽度取屏幕宽度，高度取配置里的
/// `dock.height`，保证频谱至少能按合理尺寸启动。
#[must_use]
pub fn screen_size() -> Option<(u16, u16)> {
    let (conn, screen_num) = connect().ok()?;
    let root = conn.setup().roots.get(screen_num)?;
    Some((root.width_in_pixels, root.height_in_pixels))
}

use x11rb::connection::Connection;
use x11rb::protocol::shape::{ConnectionExt as _, SK, SO};
use x11rb::protocol::xproto::{
    ClipOrdering, Colormap, ColormapAlloc, ConfigureWindowAux, ConnectionExt as _, CreateGCAux,
    CreateWindowAux, EventMask, Gcontext, ImageFormat, Pixmap, PropMode, Rectangle, StackMode,
    VisualClass, Window, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use crate::overlay::{OverlayBackend, OverlayGeometry};

/// 叠加层的窗口类型。
///
/// **为什么需要区分**：kwin 把所有 `_NET_WM_WINDOW_TYPE_DOCK` 放进同一个
/// dock 层，层内顺序由 kwin 自己决定 —— 实测我们的 DOCK 窗口会被压在
/// dde dock **下面**（`xwininfo -root -children` 的 `QueryTree` 顺序可证），
/// 频谱会被 Dock 挡住。而 D3 实证过「受管普通窗口 + ABOVE」会落在
/// `ABOVE_DOCK` 层级，即 Dock 之上。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayWindowType {
    /// `_NET_WM_WINDOW_TYPE_DOCK`：与任务栏同类。实测会被压在 dock 之下。
    Dock,
    /// 不设窗口类型（= NORMAL）+ `_NET_WM_STATE_ABOVE`。
    ///
    /// 实测仍被压在 dock **之下** —— kwin 的层序是
    /// desktop < below < normal < above < **dock** < notification < osd，
    /// 普通 ABOVE 窗口永远到不了 dock 层之上。
    Normal,
    /// `_NET_WM_WINDOW_TYPE_NOTIFICATION`：kwin 的 notification 层，通常高于 dock。
    Notification,
    /// `_KDE_NET_WM_WINDOW_TYPE_ON_SCREEN_DISPLAY`：kwin 的 OSD 层，最高。
    OnScreenDisplay,
}

/// 叠加层的窗口类名（两个字段都写同一个值，与 D1 探针一致）。
const WM_CLASS: &[u8] = b"deepin-spectrum\0deepin-spectrum\0";

/// X11 叠加层。
///
/// 生命周期：`create` → 若干次 `set_geometry` → `destroy`（或 `Drop` 自动销毁）。
pub struct X11Overlay {
    conn: RustConnection,
    win: Window,
    colormap: Colormap,
    screen_num: usize,
    /// 最近一次成功下发的几何（用于「真变化才下发」的去重）。
    geometry: OverlayGeometry,
    /// 屏幕尺寸，用于拒绝越界几何。
    screen_width: u16,
    screen_height: u16,
    /// 输入区重设次数（尺寸变化后必须 +1，供 doctor 观察）。
    input_region_reapplied: u64,
    /// 窗口类型。
    window_type: OverlayWindowType,
    /// 是否 override-redirect（绕过 WM；「显示桌面」/ 最小化不再影响本窗口）。
    override_redirect: bool,
    /// 离屏后备缓冲（0 = 尚未创建）。
    ///
    /// 渲染先写进这块 depth-32 pixmap，再整块 `copy_area` 到窗口：
    /// ① 避免逐帧直接写窗口造成撕裂；
    /// ② **能回读** —— `GetImage` 对窗口有"必须 viewable 且矩形在窗口内"的
    ///    限制，实测在本环境（Xwayland + kwin）对 depth-32 窗口直接返回
    ///    `BadMatch`；对 pixmap 则没有这条限制。
    back_buffer: Pixmap,
    /// 后备缓冲当前尺寸（用于 resize 时重建）。
    back_size: (u16, u16),
    /// 复用的 GC（0 = 尚未创建）。
    ///
    /// 每帧 `put_image` 都新建 GC 会产生两次往返（create + free），
    /// 在 30fps 下是无谓开销；这里按需创建一次、`destroy` 时释放。
    gc: Gcontext,
    /// 复用的 `put_image` 字节缓冲（小端 ARGB → `B,G,R,A`）。
    ///
    /// 每帧 `Vec::new` 会反复分配 ~1.5MB；这里按尺寸复用同一块内存。
    image_bytes: Vec<u8>,
}

impl std::fmt::Debug for X11Overlay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("X11Overlay")
            .field("win", &format_args!("0x{:x}", self.win))
            .field("geometry", &self.geometry)
            .field("override_redirect", &self.override_redirect)
            .field("screen", &(self.screen_width, self.screen_height))
            .finish_non_exhaustive()
    }
}

impl X11Overlay {
    /// 建立叠加层窗口（受管 DOCK 类型，经典路径）。
    ///
    /// # Errors
    ///
    /// 连接失败、无 depth-32 visual、几何非法或越界、或 X11 调用失败时返回错误。
    pub fn create(geometry: OverlayGeometry) -> Result<Self, OverlayError> {
        Self::create_impl(geometry, OverlayWindowType::Dock, false)
    }

    /// 建立 **override-redirect** 叠加层（绕过 WM）。
    ///
    /// 为什么需要它（F124，用户 2026-11-15 真机反馈）：受管窗口会被
    /// 「显示桌面」/ 最小化等 WM 操作隐藏；OR 窗口完全绕过 WM，
    /// 频谱不会因此消失。代价：不设 `_NET_WM_STATE_*`（F28 的互斥约束），
    /// 且需要**定期 raise** 才能保持在别的 OR 窗口之上。
    ///
    /// 仍会声明 `_NET_WM_WINDOW_TYPE_DOCK`（F124b）：kwin 的「显示桌面」
    /// 动画会跳过 DOCK 类型，否则桌面会短暂盖住频谱（实测 150~250ms）。
    ///
    /// # Errors
    ///
    /// 同 [`X11Overlay::create`]。
    pub fn create_override_redirect(geometry: OverlayGeometry) -> Result<Self, OverlayError> {
        Self::create_impl(geometry, OverlayWindowType::Dock, true)
    }

    /// 是否 override-redirect 模式。
    #[must_use]
    pub const fn is_override_redirect(&self) -> bool {
        self.override_redirect
    }

    /// 以指定窗口类型建立叠加层。
    ///
    /// # Errors
    ///
    /// 同 [`X11Overlay::create`]。
    pub fn create_with(
        geometry: OverlayGeometry,
        window_type: OverlayWindowType,
    ) -> Result<Self, OverlayError> {
        Self::create_impl(geometry, window_type, false)
    }

    fn create_impl(
        geometry: OverlayGeometry,
        window_type: OverlayWindowType,
        override_redirect: bool,
    ) -> Result<Self, OverlayError> {
        let (conn, screen_num) = connect()?;
        let root = conn.setup().roots[screen_num].root;
        let screen_width = conn.setup().roots[screen_num].width_in_pixels;
        let screen_height = conn.setup().roots[screen_num].height_in_pixels;

        let mut overlay = Self {
            conn,
            win: 0,
            colormap: 0,
            screen_num,
            geometry: OverlayGeometry::new(0, 0, 1, 1),
            screen_width,
            screen_height,
            input_region_reapplied: 0,
            window_type: OverlayWindowType::Dock,
            override_redirect,
            back_buffer: 0,
            back_size: (0, 0),
            gc: 0,
            image_bytes: Vec::new(),
        };

        overlay.validate(geometry)?;

        // F62：显式过滤 TrueColor + bits_per_rgb==8
        let visual_id = overlay.pick_argb_visual()?;

        // F28：depth-32 必须配独立 colormap（CreateWindowAux.colormap = CWColormap）
        let colormap = Self::xerr("generate_id", overlay.conn.generate_id())?;
        Self::xerr(
            "create_colormap",
            overlay
                .conn
                .create_colormap(ColormapAlloc::NONE, colormap, root, visual_id),
        )?;

        let win = Self::xerr("generate_id", overlay.conn.generate_id())?;
        let aux = CreateWindowAux::new()
            .background_pixel(0)
            .border_pixel(0)
            .colormap(colormap)
            // 受管窗口 OR=0；OR=1 时**不设** _NET_WM_*（F28 互斥），见 apply_window_properties
            .override_redirect(u32::from(override_redirect))
            .event_mask(EventMask::STRUCTURE_NOTIFY | EventMask::EXPOSURE);
        Self::xerr(
            "create_window",
            overlay.conn.create_window(
                32, // F1：显式 depth=32，不能用 root_depth
                win,
                root,
                geometry.x,
                geometry.y,
                geometry.width,
                geometry.height,
                0,
                WindowClass::INPUT_OUTPUT,
                visual_id,
                &aux,
            ),
        )?;

        overlay.win = win;
        overlay.colormap = colormap;
        overlay.geometry = geometry;
        overlay.window_type = window_type;
        overlay.override_redirect = override_redirect;

        // OR 窗口不设任何 `_NET_WM_STATE_*`（F28：与 override_redirect=1 互斥）；
        // 但仍要声明 DOCK 类型，kwin 的「显示桌面」效果会跳过它（F124b）。
        overlay.apply_wm_class()?;
        if override_redirect {
            overlay.apply_or_window_type()?;
        } else {
            overlay.apply_window_properties()?;
        }
        // F77：空输入区 = 完全穿透
        overlay.apply_empty_input_region()?;
        Self::xerr("map_window", overlay.conn.map_window(win))?;
        // ★ 必须显式置顶：Dock 自己也是 ABOVE 状态的两个同类窗口，
        // 谁在上取决于 map 顺序。若我们不置顶，叠加层会藏在 Dock 后面——
        // 用户看不到频谱（功能失效），而且输入区实验也无法命中它。
        overlay.raise()?;

        Ok(overlay)
    }

    /// 写入 `WM_CLASS`（两种模式都写；OR 窗口上它只是调试可读性，不是 WM 状态）。
    fn apply_wm_class(&mut self) -> Result<(), OverlayError> {
        let string_atom = self.atom("STRING")?;
        Self::xerr(
            "change_property8(WM_CLASS)",
            self.conn.change_property8(
                PropMode::REPLACE,
                self.win,
                self.atom("WM_CLASS")?,
                string_atom,
                WM_CLASS,
            ),
        )?;
        self.flush()
    }

    /// OR 模式只声明 `_NET_WM_WINDOW_TYPE_DOCK`，不写任何 `_NET_WM_STATE_*`。
    ///
    /// 为什么需要它（F124b，真机录屏实证）：kwin 的「显示桌面」效果会跳过
    /// DOCK 类型的窗口；不声明类型时，桌面窗口会短暂盖住频谱 150~250ms，
    /// 用户看到「频谱闪一下」。OR 窗口声明类型不会被 WM 接管（仍然 OR）。
    fn apply_or_window_type(&mut self) -> Result<(), OverlayError> {
        let atom_atom = self.atom("ATOM")?;
        Self::xerr(
            "change_property32(_NET_WM_WINDOW_TYPE, OR)",
            self.conn.change_property32(
                PropMode::REPLACE,
                self.win,
                self.atom("_NET_WM_WINDOW_TYPE")?,
                atom_atom,
                &[self.atom("_NET_WM_WINDOW_TYPE_DOCK")?],
            ),
        )?;
        self.flush()
    }

    /// 写入窗口属性：`DOCK` 类型 / `ABOVE`+`SKIP`+`STICKY`；**绝不设 strut**（F7）。
    ///
    /// 仅受管模式调用（OR 模式只走 [`Self::apply_or_window_type`]，
    /// 不写 `_NET_WM_STATE_*`，F28）。
    fn apply_window_properties(&self) -> Result<(), OverlayError> {
        let atom_atom = self.atom("ATOM")?;
        let cardinal_atom = self.atom("CARDINAL")?;

        match self.window_type {
            OverlayWindowType::Dock => {
                Self::xerr(
                    "change_property32(_NET_WM_WINDOW_TYPE)",
                    self.conn.change_property32(
                        PropMode::REPLACE,
                        self.win,
                        self.atom("_NET_WM_WINDOW_TYPE")?,
                        atom_atom,
                        &[self.atom("_NET_WM_WINDOW_TYPE_DOCK")?],
                    ),
                )?;
            }
            OverlayWindowType::Notification | OverlayWindowType::OnScreenDisplay => {
                let atom_name = match self.window_type {
                    OverlayWindowType::OnScreenDisplay => {
                        "_KDE_NET_WM_WINDOW_TYPE_ON_SCREEN_DISPLAY"
                    }
                    _ => "_NET_WM_WINDOW_TYPE_NOTIFICATION",
                };
                Self::xerr(
                    "change_property32(_NET_WM_WINDOW_TYPE)",
                    self.conn.change_property32(
                        PropMode::REPLACE,
                        self.win,
                        self.atom("_NET_WM_WINDOW_TYPE")?,
                        atom_atom,
                        &[self.atom(atom_name)?],
                    ),
                )?;
            }
            OverlayWindowType::Normal => {
                // 显式删除类型属性 = 让 WM 按 NORMAL 处理（D3 的 ManagedAbove 策略）
                Self::xerr(
                    "delete_property(_NET_WM_WINDOW_TYPE)",
                    self.conn
                        .delete_property(self.win, self.atom("_NET_WM_WINDOW_TYPE")?),
                )?;
            }
        }

        Self::xerr(
            "change_property32(_NET_WM_STATE)",
            self.conn.change_property32(
                PropMode::REPLACE,
                self.win,
                self.atom("_NET_WM_STATE")?,
                atom_atom,
                &[
                    self.atom("_NET_WM_STATE_ABOVE")?,
                    self.atom("_NET_WM_STATE_SKIP_TASKBAR")?,
                    self.atom("_NET_WM_STATE_SKIP_PAGER")?,
                    self.atom("_NET_WM_STATE_STICKY")?,
                ],
            ),
        )?;

        Self::xerr(
            "change_property32(_NET_WM_PID)",
            self.conn.change_property32(
                PropMode::REPLACE,
                self.win,
                self.atom("_NET_WM_PID")?,
                cardinal_atom,
                &[std::process::id()],
            ),
        )?;

        // 显式声明：不设 strut、不设 BYPASS_COMPOSITOR（F7）。
        // 这里不是"忘了设"，而是有意识地保持工作区不被篡改。
        Ok(())
    }

    /// F77：`shape_rectangles` 7 参数，空矩形数组 = Input Region 为空 = 完全穿透。
    ///
    /// 用 `SK::BOUNDING` 会变成"整窗可点"，那是相反的语义。
    fn apply_empty_input_region(&mut self) -> Result<(), OverlayError> {
        Self::xerr(
            "shape_rectangles(SK::INPUT, ∅)",
            self.conn.shape_rectangles(
                SO::SET,
                SK::INPUT,
                ClipOrdering::UNSORTED,
                self.win,
                0,
                0,
                &[],
            ),
        )?;
        self.input_region_reapplied += 1;
        Ok(())
    }

    /// 用给定 ARGB 颜色填满整个窗口（仅用于探针的可视化确认）。
    ///
    /// 颜色是**预乘 alpha** 的 ARGB（与 §6.3 的渲染约定一致）：
    /// 例如 25% 红 = `0x40FF0000`。
    ///
    /// # Errors
    ///
    /// 创建 GC 或绘制失败时返回错误。
    pub fn fill(&mut self, argb: u32) -> Result<(), OverlayError> {
        let gc: Gcontext = Self::xerr("generate_id", self.conn.generate_id())?;
        // GC 建在目标窗口上（不是 root），保证 depth 与 32 位 drawable 一致。
        Self::xerr(
            "create_gc",
            self.conn.create_gc(
                gc,
                self.win,
                &CreateGCAux::new().foreground(argb).graphics_exposures(0),
            ),
        )?;
        Self::xerr(
            "poly_fill_rectangle",
            self.conn.poly_fill_rectangle(
                self.win,
                gc,
                &[Rectangle {
                    x: 0,
                    y: 0,
                    width: self.geometry.width,
                    height: self.geometry.height,
                }],
            ),
        )?;
        Self::xerr("free_gc", self.conn.free_gc(gc))?;
        self.flush()
    }

    /// **仅用于实证**：把输入区设为整窗，构造「穿透失效」的反例。
    ///
    /// 存在的理由：`xwininfo -shape` 对普通窗口也报 "No window shape defined"，
    /// **无法区分**"输入区为空"与"从未设置过输入区"。所以 F77 只能用
    /// **行为**验证：指针落在窗口上时，`XQueryPointer` 是否会选中它。
    /// 本方法提供"反向状态"（整窗可点），用来证明那条行为断言**会红**——
    /// 否则就是又一个假 PASS（§1.4 方法论）。
    ///
    /// 生产代码**不得调用**。
    #[doc(hidden)]
    pub fn debug_set_full_input_region(&mut self) -> Result<(), OverlayError> {
        Self::xerr(
            "shape_rectangles(SK::INPUT, 整窗)",
            self.conn.shape_rectangles(
                SO::SET,
                SK::INPUT,
                ClipOrdering::UNSORTED,
                self.win,
                0,
                0,
                &[Rectangle {
                    x: 0,
                    y: 0,
                    width: self.geometry.width,
                    height: self.geometry.height,
                }],
            ),
        )?;
        self.flush()
    }

    /// 把窗口提到同层的最上面。
    ///
    /// # Errors
    ///
    /// X11 调用失败时返回错误。
    pub fn raise(&mut self) -> Result<(), OverlayError> {
        Self::xerr(
            "configure_window(StackMode::ABOVE)",
            self.conn.configure_window(
                self.win,
                &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
            ),
        )?;
        self.flush()
    }

    /// 输入区重设次数（尺寸变化后应为每次 +1）。
    #[must_use]
    pub const fn input_region_reapplied(&self) -> u64 {
        self.input_region_reapplied
    }

    /// 把一整块 ARGB 像素缓冲送到窗口。
    ///
    /// `pixels` 为行主序、**预乘 alpha** 的 `0xAARRGGBB`，长度必须等于
    /// `width * height`；长度不符会在下发前被拒绝
    /// （[`OverlayError::PixelBufferMismatch`]）—— 长度错位的 `put_image`
    /// 在服务端会画出"看起来有东西但位置全错"的图像，比直接报错难查得多。
    ///
    /// 写入的是**离屏后备缓冲**，随后整块 `copy_area` 到窗口。
    /// 这样做有两个理由：逐帧直写窗口会撕裂；而且 pixmap 可回读（见 [`Self::get_image`]）。
    ///
    /// # Errors
    ///
    /// 缓冲长度不符、或 X11 调用失败时返回错误。
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
        let gc = self.ensure_gc()?;
        let back = self.ensure_back_buffer(width, height)?;
        // 预乘 ARGB 的 u32 在小端机器上的内存布局是 B,G,R,A，
        // 与 depth-32 ZPixmap 的字节序一致，因此直接按小端展开即可。
        // 复用 `image_bytes`：每帧 1.5MB 的分配在 47fps 下是纯浪费。
        self.image_bytes.resize(expected * 4, 0);
        for (chunk, pixel) in self.image_bytes.chunks_exact_mut(4).zip(pixels) {
            chunk.copy_from_slice(&pixel.to_le_bytes());
        }
        Self::xerr(
            "put_image",
            self.conn.put_image(
                ImageFormat::Z_PIXMAP,
                back,
                gc,
                width,
                height,
                0,
                0,
                0,
                32,
                &self.image_bytes,
            ),
        )?;
        self.xerr_copy_to_window(back, gc, width, height)?;
        self.flush()
    }

    /// 把后备缓冲整块拷到窗口。
    fn xerr_copy_to_window(
        &self,
        source: Pixmap,
        gc: Gcontext,
        width: u16,
        height: u16,
    ) -> Result<(), OverlayError> {
        let cookie = Self::xerr(
            "copy_area",
            self.conn
                .copy_area(source, self.win, gc, 0, 0, 0, 0, width, height),
        )?;
        // copy_area 是 void 请求：必须 check 才能拿到服务端错误，
        // 否则 BadMatch（比如 pixmap 与窗口 depth 不一致）会被静默吞掉，
        // 画面表现成"什么都没有"，反而比报错更难查。
        Self::xerr("copy_area.check", cookie.check())
    }

    /// 回读**后备缓冲**的像素（行主序 ARGB）。
    ///
    /// 存在的意义是**验证**：断言"我发上去的和我以为的一样"不能靠自己的缓冲，
    /// 必须问服务端要回真实像素。仅用于探针与 doctor，不要放进渲染循环
    /// （每帧回读会拖垮帧率）。
    ///
    /// 读的是 pixmap 而不是窗口：`GetImage` 对窗口要求"viewable 且矩形在窗口内"，
    /// 本环境对 depth-32 窗口直接返回 `BadMatch`（探针实测），对 pixmap 无此限制。
    ///
    /// # Errors
    ///
    /// 尚未渲染过、尺寸越界或 X11 调用失败时返回错误。
    pub fn get_image(
        &mut self,
        x: u16,
        y: u16,
        width: u16,
        height: u16,
    ) -> Result<Vec<u32>, OverlayError> {
        if self.back_buffer == 0 {
            return Err(OverlayError::X11 {
                operation: "get_image",
                detail: "后备缓冲尚未创建（先调用 put_image）".to_string(),
            });
        }
        let back = self.back_buffer;
        self.read_pixels(back, x, y, width, height)
    }

    /// 回读**窗口**的像素。
    ///
    /// 与 [`Self::get_image`] 的区别：这里问的是"屏幕上那块区域到底是什么"。
    /// 在本环境（Xwayland + kwin）对 depth-32 窗口会返回 `BadMatch`，
    /// 探针会把这条失败**如实记录**，而不是当成通过。
    ///
    /// # Errors
    ///
    /// 同 [`Self::get_image`]。
    pub fn get_window_image(
        &mut self,
        x: u16,
        y: u16,
        width: u16,
        height: u16,
    ) -> Result<Vec<u32>, OverlayError> {
        let win = self.win;
        self.read_pixels(win, x, y, width, height)
    }

    /// 从任意 drawable 读像素。
    fn read_pixels(
        &mut self,
        drawable: u32,
        x: u16,
        y: u16,
        width: u16,
        height: u16,
    ) -> Result<Vec<u32>, OverlayError> {
        let sink_x = i16::try_from(x).map_err(|_| OverlayError::OutOfRange)?;
        let sink_y = i16::try_from(y).map_err(|_| OverlayError::OutOfRange)?;
        let cookie = Self::xerr(
            "get_image",
            self.conn.get_image(
                ImageFormat::Z_PIXMAP,
                drawable,
                sink_x,
                sink_y,
                width,
                height,
                !0,
            ),
        )?;
        let reply = Self::xerr("get_image.reply", cookie.reply())?;
        let pixels: Vec<u32> = reply
            .data
            .chunks_exact(4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect();
        let expected = usize::from(width) * usize::from(height);
        if pixels.len() != expected {
            return Err(OverlayError::PixelBufferMismatch {
                width,
                height,
                pixels: pixels.len(),
                expected,
            });
        }
        Ok(pixels)
    }

    /// 确保后备缓冲与请求尺寸一致（不一致就重建）。
    fn ensure_back_buffer(&mut self, width: u16, height: u16) -> Result<Pixmap, OverlayError> {
        if self.back_buffer != 0 && self.back_size == (width, height) {
            return Ok(self.back_buffer);
        }
        if self.back_buffer != 0 {
            let stale = std::mem::take(&mut self.back_buffer);
            Self::xerr("free_pixmap", self.conn.free_pixmap(stale))?;
        }
        let pixmap: Pixmap = Self::xerr("generate_id", self.conn.generate_id())?;
        let root = self.conn.setup().roots[self.screen_num].root;
        // depth 显式 32：与窗口一致，否则 copy_area 会 BadMatch
        Self::xerr(
            "create_pixmap",
            self.conn.create_pixmap(32, pixmap, root, width, height),
        )?;
        self.back_buffer = pixmap;
        self.back_size = (width, height);
        Ok(pixmap)
    }

    /// 按需创建并复用 GC。
    ///
    /// GC 必须建在**目标窗口**上：depth-32 drawable 配 depth-24 的 GC
    /// 在部分服务端会 `BadMatch`。
    fn ensure_gc(&mut self) -> Result<Gcontext, OverlayError> {
        if self.gc != 0 {
            return Ok(self.gc);
        }
        let gc: Gcontext = Self::xerr("generate_id", self.conn.generate_id())?;
        Self::xerr(
            "create_gc",
            self.conn.create_gc(
                gc,
                self.win,
                &CreateGCAux::new().foreground(0).graphics_exposures(0),
            ),
        )?;
        self.gc = gc;
        Ok(gc)
    }

    /// 屏幕尺寸。
    #[must_use]
    pub const fn screen_size(&self) -> (u16, u16) {
        (self.screen_width, self.screen_height)
    }

    /// 窗口 id（供 `xprop -id` 复核）。
    #[must_use]
    pub const fn window_id(&self) -> u32 {
        self.win
    }

    /// 销毁窗口与 colormap。幂等。
    ///
    /// # Errors
    ///
    /// X11 调用失败时返回错误（窗口可能已随连接关闭而消失）。
    pub fn destroy(&mut self) -> Result<(), OverlayError> {
        if self.win == 0 {
            return Ok(());
        }
        let win = std::mem::take(&mut self.win);
        let colormap = std::mem::take(&mut self.colormap);
        let back = std::mem::take(&mut self.back_buffer);
        if back != 0 {
            Self::xerr("free_pixmap", self.conn.free_pixmap(back))?;
        }
        let gc = std::mem::take(&mut self.gc);
        if gc != 0 {
            // GC 依附于窗口；窗口销毁前释放，避免服务端留垃圾
            Self::xerr("free_gc", self.conn.free_gc(gc))?;
        }
        Self::xerr("destroy_window", self.conn.destroy_window(win))?;
        Self::xerr("free_colormap", self.conn.free_colormap(colormap))?;
        self.conn.flush().map_err(|e| OverlayError::X11 {
            operation: "flush",
            detail: e.to_string(),
        })?;
        Ok(())
    }

    /// 校验几何：非零尺寸 + 不越屏幕。
    fn validate(&self, geometry: OverlayGeometry) -> Result<(), OverlayError> {
        if !geometry.is_valid() {
            return Err(OverlayError::InvalidGeometry {
                width: geometry.width,
                height: geometry.height,
            });
        }
        if geometry.width > self.screen_width || geometry.height > self.screen_height {
            return Err(OverlayError::GeometryOutsideScreen {
                geometry,
                screen_width: self.screen_width,
                screen_height: self.screen_height,
            });
        }
        Ok(())
    }

    /// F62：挑 depth-32 的 `TrueColor` visual。
    fn pick_argb_visual(&self) -> Result<u32, OverlayError> {
        for depth in &self.conn.setup().roots[self.screen_num].allowed_depths {
            if depth.depth != 32 {
                continue;
            }
            for visual in &depth.visuals {
                if visual.class == VisualClass::TRUE_COLOR && visual.bits_per_rgb_value == 8 {
                    return Ok(visual.visual_id);
                }
            }
        }
        Err(OverlayError::NoArgbVisual)
    }

    /// 取 atom。用 `intern_atom(...).reply()?.atom` 得到明确的 `u32`，
    /// 避免 `AtomEnum::X.into()` 的 u8/u16/u32 推断歧义（M10）。
    fn atom(&self, name: &str) -> Result<u32, OverlayError> {
        let cookie = Self::xerr("intern_atom", self.conn.intern_atom(false, name.as_bytes()))?;
        let reply = Self::xerr("intern_atom.reply", cookie.reply())?;
        Ok(reply.atom)
    }

    /// 统一的错误映射。
    ///
    /// x11rb 有 `ConnectionError` / `ReplyError` / `ReplyOrIdError` 三种错误类型，
    /// 都实现 `Display`，故用泛型一次覆盖，避免为每种类型写一个辅助函数
    /// （否则 `generate_id` 与 `reply()` 会因类型不匹配而编译失败）。
    fn xerr<T, E: std::fmt::Display>(
        operation: &'static str,
        result: Result<T, E>,
    ) -> Result<T, OverlayError> {
        result.map_err(|e| OverlayError::X11 {
            operation,
            detail: e.to_string(),
        })
    }
}

impl OverlayBackend for X11Overlay {
    fn geometry(&self) -> Result<OverlayGeometry, OverlayError> {
        let cookie = Self::xerr("get_geometry", self.conn.get_geometry(self.win))?;
        let reply = Self::xerr("get_geometry.reply", cookie.reply())?;

        // ★ 受管窗口会被 WM reparent 进框架窗口，`get_geometry` 返回的是
        // **相对父窗口**的坐标（通常 y=0），不是根坐标。直接拿它和
        // `FrontendWindowRect` 比较会得到上千像素的假误差。
        // 必须用 `translate_coordinates` 换算到 root 才是屏幕绝对位置。
        // （与 M26 同族：reparent 后坐标系变了。）
        let root = self.conn.setup().roots[self.screen_num].root;
        let (x, y) = match self.conn.translate_coordinates(self.win, root, 0, 0) {
            Ok(cookie) => match cookie.reply() {
                Ok(reply) => (reply.dst_x, reply.dst_y),
                Err(_) => (reply.x, reply.y),
            },
            Err(_) => (reply.x, reply.y),
        };

        Ok(OverlayGeometry::new(x, y, reply.width, reply.height))
    }

    fn set_geometry(&mut self, geometry: OverlayGeometry) -> Result<(), OverlayError> {
        self.validate(geometry)?;
        if geometry == self.geometry {
            // 差分优先：没变就不下发，避免拖动时产生无意义的请求风暴（§3 步骤 4）
            return Ok(());
        }

        let aux = ConfigureWindowAux::new()
            .x(i32::from(geometry.x))
            .y(i32::from(geometry.y))
            .width(u32::from(geometry.width))
            .height(u32::from(geometry.height));
        Self::xerr(
            "configure_window",
            self.conn.configure_window(self.win, &aux),
        )?;
        self.geometry = geometry;

        // ★ 尺寸变化后必须重设空输入区，否则叠加层会开始吃鼠标事件。
        self.apply_empty_input_region()?;
        self.flush()
    }

    fn show(&mut self) -> Result<(), OverlayError> {
        Self::xerr("map_window", self.conn.map_window(self.win))?;
        self.flush()
    }

    fn hide(&mut self) -> Result<(), OverlayError> {
        // 先重设一次空输入区：unmap 期间若有残留区域，重新 map 时会立刻生效。
        self.apply_empty_input_region()?;
        Self::xerr("unmap_window", self.conn.unmap_window(self.win))?;
        self.flush()
    }

    fn flush(&mut self) -> Result<(), OverlayError> {
        self.conn.flush().map_err(|e| OverlayError::X11 {
            operation: "flush",
            detail: e.to_string(),
        })
    }
}

impl Drop for X11Overlay {
    fn drop(&mut self) {
        // 兜底：绝不在用户桌面留下残留窗口。失败也不 panic。
        let _ = self.destroy();
    }
}

#[cfg(test)]
mod tests {
    //! 本模块的单元测试只覆盖**不依赖 X server** 的纯逻辑：
    //! 几何校验、视觉选择的前提条件等。真正建窗由 `overlay_follow_probe` 实证。

    use crate::overlay::{OverlayError, OverlayGeometry};

    #[test]
    fn infer_display_returns_colon_number_or_none() {
        // 只验证返回值形态：要么是 ":数字"，要么是 None（无 socket 目录）。
        // 不断言具体值，避免依赖测试机上恰好存在的 X display。
        if let Some(d) = super::infer_display() {
            assert!(
                d.starts_with(':') && d[1..].chars().all(|c| c.is_ascii_digit()),
                "推断出的 DISPLAY 形态非法：{d}"
            );
        }
    }

    #[test]
    fn invalid_geometry_is_rejected_before_touching_x11() {
        // validate 是纯函数，可在无 X server 环境验证
        let screen = (3200_u16, 2000_u16);
        let check = |geometry: OverlayGeometry| -> Result<(), OverlayError> {
            if !geometry.is_valid() {
                return Err(OverlayError::InvalidGeometry {
                    width: geometry.width,
                    height: geometry.height,
                });
            }
            if geometry.width > screen.0 || geometry.height > screen.1 {
                return Err(OverlayError::GeometryOutsideScreen {
                    geometry,
                    screen_width: screen.0,
                    screen_height: screen.1,
                });
            }
            Ok(())
        };

        assert!(check(OverlayGeometry::new(0, 1876, 3200, 124)).is_ok());
        assert!(matches!(
            check(OverlayGeometry::new(0, 0, 0, 124)),
            Err(OverlayError::InvalidGeometry { .. })
        ));
        assert!(matches!(
            check(OverlayGeometry::new(0, 0, 3200, 2001)),
            Err(OverlayError::GeometryOutsideScreen { .. })
        ));
    }
}
