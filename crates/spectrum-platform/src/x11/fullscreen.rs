//! 全屏检测（EWMH：`_NET_ACTIVE_WINDOW` + `_NET_WM_STATE_FULLSCREEN`）。
//!
//! ## 为什么是轮询而不是事件订阅
//!
//! `_NET_ACTIVE_WINDOW` 变化没有 X 事件（它是根窗口上的普通属性，不是窗口属性），
//! X11 下只能靠 PropertyNotify（根窗口上的属性变化**有**事件吗？—— X 协议里
//! `PropertyNotify` 只发给**选中了该属性**的窗口，而根窗口属性可以选！但实测各 WM
//! 的行为不一致，kwin 改 `_NET_ACTIVE_WINDOW` 不总是发事件）。轮询 500ms 足够：
//! 全屏切换本来就是秒级操作，两次 `GetProperty` 往返（微秒级）可忽略。
//!
//! ## 为什么在 daemon 里做而不是靠 WM 信号
//!
//! 「全屏播放视频时隐藏频谱」是**产品行为**（F39），不是平台能力——
//! 它需要主动查询而不是被动等通知。EWMH 约定 `_NET_ACTIVE_WINDOW` 指向
//! 当前焦点窗口，其 `_NET_WM_STATE` 含 `FULLSCREEN` 即视为全屏。
//!
//! ## 错误策略
//!
//! 任何一步失败（活动窗口刚好被关掉、WM 不支持 EWMH、连接中断）都按
//! **"非全屏"** 处理：误隐藏比漏隐藏恼火得多。

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{Atom, AtomEnum, ConnectionExt, Window};
use x11rb::rust_connection::RustConnection;

use super::OverlayError;

/// 全屏监视器（独立 X 连接，与叠加窗互不干扰）。
pub struct FullscreenMonitor {
    conn: RustConnection,
    root: Window,
    atom_active: Atom,
    atom_state: Atom,
    atom_fullscreen: Atom,
}

impl FullscreenMonitor {
    /// 建立连接并解析所需 atom。
    ///
    /// # Errors
    ///
    /// X 连接失败或 atom 分配失败。
    pub fn connect() -> Result<Self, OverlayError> {
        let (conn, screen_num) = super::connect()?;
        let root = conn.setup().roots[screen_num].root;
        let atom_active = Self::intern(&conn, "_NET_ACTIVE_WINDOW")?;
        let atom_state = Self::intern(&conn, "_NET_WM_STATE")?;
        let atom_fullscreen = Self::intern(&conn, "_NET_WM_STATE_FULLSCREEN")?;
        Ok(Self {
            conn,
            root,
            atom_active,
            atom_state,
            atom_fullscreen,
        })
    }

    fn intern(conn: &RustConnection, name: &str) -> Result<Atom, OverlayError> {
        let cookie = conn
            .intern_atom(false, name.as_bytes())
            .map_err(|e| OverlayError::X11 {
                operation: "intern_atom",
                detail: e.to_string(),
            })?;
        let reply = cookie.reply().map_err(|e| OverlayError::X11 {
            operation: "intern_atom.reply",
            detail: e.to_string(),
        })?;
        Ok(reply.atom)
    }

    /// 当前活动窗口是否全屏。
    ///
    /// 阻塞量级：两次 `GetProperty` 往返（局域 X 上 < 1ms），轮询周期 500ms 足够。
    /// 任何错误一律按 `false` 处理（见模块文档的错误策略）。
    pub fn is_fullscreen(&mut self) -> bool {
        // 1) 活动窗口（EWMH：根窗口上的 CARDINAL）
        let Some(active) = self.window_u32(self.root, self.atom_active) else {
            return false;
        };
        if active == 0 {
            return false;
        }
        // 2) 它的 _NET_WM_STATE 是否含 FULLSCREEN（ATOM 数组）；
        //    窗口刚好被关掉（BadWindow）等情况按非全屏处理。
        //
        // ★ `type` 必须传属性**真实的类型**（ATOM），不能传属性名 atom 本身：
        //   X11 的 GetProperty 在 type 不匹配时返回空值而不是报错，
        //   上一版传 `atom_state` 导致这里永远拿不到状态（全屏检测恒为 false）。
        let Ok(cookie) =
            self.conn
                .get_property(false, active, self.atom_state, AtomEnum::ATOM, 0, 1024)
        else {
            return false;
        };
        let Ok(reply) = cookie.reply() else {
            return false;
        };
        reply
            .value32()
            .is_some_and(|mut atoms| atoms.any(|atom| atom == self.atom_fullscreen))
    }

    /// 读一个 CARDINAL/WINDOW 属性的第一个值。
    ///
    /// `_NET_ACTIVE_WINDOW` 的真实类型是 `WINDOW`（`AtomEnum::WINDOW`），
    /// 不是属性名 atom —— 理由同 [`Self::is_fullscreen`] 里的注释。
    fn window_u32(&mut self, win: Window, prop: Atom) -> Option<u32> {
        let cookie = self
            .conn
            .get_property(false, win, prop, AtomEnum::WINDOW, 0, 1)
            .ok()?;
        let reply = cookie.reply().ok()?;
        reply.value32().and_then(|mut v| v.next())
    }
}
