//! 配置文件 ↔ [`spectrum_core::config::Config`] 的映射，以及**每个键的生效范围登记**。
//!
//! ## 为什么要单独维护一张"生效范围"表
//!
//! 设置界面最容易犯的错不是"没做控件"，而是**做了控件但它不生效**——
//! 用户改完点了确定，画面没变，也不会报错。本项目对这类"静默失效"零容忍
//! （同理见 F33：Wayland 下 `BelowIcons` 必须置灰而不是假存）。
//!
//! 因此这里把每个键标成四种之一：
//!
//! | [`Scope`] | 含义 | `deepin-spectrum settings --explain` 里的显示 |
//! |---|---|---|
//! | `Live` | 改完下一帧生效 | 热生效 |
//! | `LiveRebuild` | 改完下一帧生效，但需重建分析器 | 热生效（重建） |
//! | `Startup` | 只在启动时读一次 | 仅启动 |
//! | `Unimplemented` | 本版本根本没实现对应功能 | **未实现** |
//!
//! `Unimplemented` 的键仍可被解析（不丢用户的值），但会在读取时给出
//! [`Warning::UnimplementedKey`]，并在 `--explain` 里显式列出，绝不假装生效。

use spectrum_core::config::{
    Advanced, Behavior, BlendMode, Config, ConfigWarning, Dock, General, OutputMode, Visual,
    VisualMode, X11Stacking,
};
use std::fmt;

use crate::toml::{Document, Value, quote, value_to_string};

/// 一个键的生效范围。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// 改完下一帧生效。
    Live,
    /// 改完下一帧生效，但需要重建分析器（会多花一帧的时间）。
    LiveRebuild,
    /// 只在启动时读一次（改完要重启 daemon）。
    Startup,
    /// 本版本没有实现对应功能，值被保存但**不生效**。
    Unimplemented,
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Live => "热生效",
            Self::LiveRebuild => "热生效（重建）",
            Self::Startup => "仅启动",
            Self::Unimplemented => "未实现",
        };
        f.write_str(s)
    }
}

/// 键的静态说明。
#[derive(Debug, Clone, Copy)]
pub struct KeySpec {
    /// 所属 section。
    pub section: &'static str,
    /// 键名。
    pub key: &'static str,
    /// 生效范围。
    pub scope: Scope,
    /// 备注（为什么未实现 / 有何限制）。
    pub note: &'static str,
}

const fn k(section: &'static str, key: &'static str, scope: Scope, note: &'static str) -> KeySpec {
    KeySpec {
        section,
        key,
        scope,
        note,
    }
}

/// 全部键及其生效范围（`--explain` 的数据源，也是"键是否存在"的唯一判据）。
pub const KEYS: &[KeySpec] = &[
    k(
        "general",
        "enabled",
        Scope::Live,
        "关 = 立即清屏并停止上屏（采集仍继续，不丢流）",
    ),
    k(
        "general",
        "autostart",
        Scope::Startup,
        "由 systemd 用户单元实际管理（设置界面里的开关直接操作 systemctl）",
    ),
    k(
        "general",
        "fps",
        Scope::Unimplemented,
        "帧率由 hop 决定（1024 帧 ≈ 46.9fps），未实现上限",
    ),
    k("general", "fade_ms", Scope::Unimplemented, "尚无淡出动画"),
    k(
        "general",
        "respect_reduced_motion",
        Scope::Unimplemented,
        "尚无动画",
    ),
    k(
        "visual",
        "mode",
        Scope::Startup,
        "FullDock=覆盖任务栏；AboveDock=任务栏上方（Wayland 热生效，X11 重启 daemon）；\
         LeftBlank/RightBlank 本版本未实现，值会保留但运行时按 FullDock 绘制并告警",
    ),
    k(
        "visual",
        "opacity",
        Scope::Live,
        "→ 渲染 alpha = round(opacity×255)",
    ),
    k(
        "visual",
        "bands",
        Scope::LiveRebuild,
        "重建分析器（同一帧内完成，不停采集）",
    ),
    k(
        "visual",
        "bar_max_height",
        Scope::Unimplemented,
        "AboveDock 预留；当前用任务栏高度",
    ),
    k(
        "visual",
        "bar_gap_ratio",
        Scope::Live,
        "→ gap_px = round(ratio×10)，0.2 ↔ 2px",
    ),
    k("visual", "color_start", Scope::Live, "柱底颜色"),
    k(
        "visual",
        "palette",
        Scope::Live,
        "多色标色板：3~8 个逗号分隔的 #RRGGBB；空 = 双色渐变",
    ),
    k(
        "visual",
        "palette_mode",
        Scope::Live,
        "铺色方式 off（双色）/ across（横向渐变）/ per_bar（每柱一色）/ vertical（柱内多段）",
    ),
    k(
        "visual",
        "color_end",
        Scope::Live,
        "柱顶颜色（与柱底相同即纯色）",
    ),
    k(
        "visual",
        "blend",
        Scope::Unimplemented,
        "固定加法混合（premultiplied）",
    ),
    k("visual", "peak_cap", Scope::Live, "峰值帽（柱顶上方 2px）"),
    k(
        "visual",
        "rounded",
        Scope::Unimplemented,
        "柱为矩形，无圆角",
    ),
    k(
        "analyze",
        "fft_size",
        Scope::LiveRebuild,
        "F56：2048 会让低频段塌缩",
    ),
    k("analyze", "f_min", Scope::LiveRebuild, ""),
    k("analyze", "f_max", Scope::LiveRebuild, ""),
    k("analyze", "db_floor", Scope::LiveRebuild, ""),
    k("analyze", "attack", Scope::LiveRebuild, ""),
    k("analyze", "release", Scope::LiveRebuild, ""),
    k(
        "analyze",
        "peak_hold_ms",
        Scope::LiveRebuild,
        "峰值滑块被顶飞后的悬停时长",
    ),
    k(
        "analyze",
        "peak_fall",
        Scope::LiveRebuild,
        "峰值滑块每帧回落量 0.002~0.05；越小落得越慢",
    ),
    k(
        "analyze",
        "peak_gravity",
        Scope::LiveRebuild,
        "true = 重力加速缓落（越落越快）；false = 匀速",
    ),
    k("analyze", "gamma", Scope::LiveRebuild, ""),
    k("analyze", "sensitivity", Scope::LiveRebuild, ""),
    k("behavior", "silence_timeout_ms", Scope::Live, ""),
    k("behavior", "silence_dbfs", Scope::Live, ""),
    k(
        "behavior",
        "hide_on_fullscreen",
        Scope::Live,
        "X11 轮询 _NET_WM_STATE_FULLSCREEN；Wayland 走原生 toplevel fullscreen（500ms）",
    ),
    k(
        "behavior",
        "hide_on_autohide",
        Scope::Live,
        "Wayland 任务栏高度 <20px 时暂停上屏；X11 未订阅自动隐藏",
    ),
    k(
        "behavior",
        "hide_on_lock",
        Scope::Live,
        "logind LockedHint / ScreenSaver GetActive（500ms）",
    ),
    k(
        "behavior",
        "pause_on_battery_30fps",
        Scope::Unimplemented,
        "未读电池状态",
    ),
    k("dock", "position", Scope::Startup, "D-Bus 探测失败时的兜底"),
    k(
        "dock",
        "height",
        Scope::Startup,
        "★ 仅兜底；运行时以 D-Bus 为准（F38）",
    ),
    k(
        "dock",
        "center_mode",
        Scope::Unimplemented,
        "空白区估算未接入",
    ),
    k(
        "dock",
        "blank_width_px",
        Scope::Unimplemented,
        "空白区估算未接入",
    ),
    k(
        "dock",
        "icon_count_hint",
        Scope::Unimplemented,
        "F52 兜底值，当前未使用",
    ),
    k(
        "dock",
        "icon_size",
        Scope::Unimplemented,
        "空白区估算未接入",
    ),
    k("outputs", "mode", Scope::Unimplemented, "仅主屏"),
    k("outputs", "names", Scope::Unimplemented, "仅主屏"),
    k(
        "advanced",
        "backend",
        Scope::Startup,
        "auto 按会话判断；x11/wayland 可强制（未编译或会话不匹配时回退并告警）",
    ),
    k(
        "advanced",
        "renderer",
        Scope::Startup,
        "固定软件渲染（shm）；gl 未实现",
    ),
    k(
        "advanced",
        "x11_stacking",
        Scope::Startup,
        "override-redirect（默认）/ managed-above；managed-below 未实现",
    ),
];

/// 指导文档模板里出现、但本版本**没有对应字段**的键。
///
/// 单独列出来是为了给出"我知道这个键，但它没实现"的告警，
/// 而不是笼统的"未知键"——用户照文档抄过来时能立刻看出差别。
pub const DOC_ONLY_KEYS: &[KeySpec] = &[
    k(
        "visual",
        "gradient",
        Scope::Unimplemented,
        "渐变固定为竖直方向",
    ),
    k(
        "advanced",
        "log_level",
        Scope::Unimplemented,
        "本版本无日志框架（RUST_LOG 不生效）",
    ),
    k(
        "analyze",
        "bands",
        Scope::Unimplemented,
        "由 visual.bands 统一控制",
    ),
];

/// 读取配置时产生的告警（不改变行为，只提示）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Warning {
    /// 键不存在于本版本 schema。
    UnknownKey {
        /// section 名（`None` 为根级）。
        section: Option<String>,
        /// 键名。
        key: String,
        /// 行号。
        line: usize,
    },
    /// 键存在但功能未实现。
    UnimplementedKey {
        /// 键名（`section.key`）。
        key: String,
        /// 为什么未实现。
        note: &'static str,
        /// 行号。
        line: usize,
    },
    /// 值的类型不对（该键保持默认值）。
    WrongType {
        /// 键名（`section.key`）。
        key: String,
        /// 期望类型。
        expected: &'static str,
        /// 实际类型。
        found: &'static str,
        /// 行号。
        line: usize,
    },
    /// 被钳制到合法区间（来自 [`Config::validate`]）。
    Clamped {
        /// 键名。
        key: String,
        /// 实际采用的值。
        applied: String,
    },
    /// 被替换为默认值。
    Replaced {
        /// 键名。
        key: String,
        /// 原因。
        reason: String,
    },
    /// 值合法但本版本未实现。
    Unsupported {
        /// 键名。
        key: String,
        /// 未实现说明。
        reason: String,
    },
}

impl fmt::Display for Warning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownKey { section, key, line } => {
                let full = section
                    .as_ref()
                    .map_or_else(|| key.clone(), |s| format!("{s}.{key}"));
                write!(f, "第 {line} 行：未知键 {full}（已忽略）")
            }
            Self::UnimplementedKey { key, note, line } => {
                write!(
                    f,
                    "第 {line} 行：{key} 本版本未实现，值已保存但不生效（{note}）"
                )
            }
            Self::WrongType {
                key,
                expected,
                found,
                line,
            } => write!(
                f,
                "第 {line} 行：{key} 应为 {expected}，实际是 {found}（该键用默认值）"
            ),
            Self::Clamped { key, applied } => write!(f, "{key} 超出合法范围，已钳制为 {applied}"),
            Self::Replaced { key, reason } => write!(f, "{key} 非法，已回退默认（{reason}）"),
            Self::Unsupported { key, reason } => {
                write!(f, "{key} 本版本未实现，值已保留但不生效（{reason}）")
            }
        }
    }
}

/// 一个**本版本不认识**但需要原样保留的键。
///
/// 为什么必须保留：设置界面会把整个配置文件重写一遍。
/// 如果用户在文件里放了新版本才有的键（或者从指导文档抄了 `visual.gradient`），
/// 重写时丢掉就等于**用户改的东西被程序擦了**。
/// 因此解码时把这类键原样留下，回写时再放回去。
#[derive(Debug, Clone, PartialEq)]
pub struct Extra {
    /// 所属 section（`None` 为根级）。
    pub section: Option<String>,
    /// 键名。
    pub key: String,
    /// 原始值。
    pub value: Value,
}

/// 解码结果。
#[derive(Debug, Clone)]
pub struct Decoded {
    /// 解析 + 校验后的配置。
    pub config: Config,
    /// 告警（顺序与文件出现顺序一致，钳制类告警排在最后）。
    pub warnings: Vec<Warning>,
    /// 本版本不认识的键（回写时原样保留）。
    pub extras: Vec<Extra>,
}

/// 把文档解码成配置。
///
/// 规则：**从默认值出发，覆盖文件里出现的键**。因此"文件即完整状态"——
/// 删掉一行等于回到该键的默认值，而不是"保持上次的值"。
///
/// # Errors
///
/// 文件里的 `version` 高于本二进制支持的版本（不能猜语义，必须让用户知道）。
pub fn decode(doc: &Document) -> Result<Decoded, String> {
    let mut cfg = Config::default();
    let mut w: Vec<Warning> = Vec::new();
    let mut extras: Vec<Extra> = Vec::new();

    // 根级只有 version
    for entry in doc.entries().iter().filter(|e| e.section.is_none()) {
        if entry.key == "version" {
            match entry.value.as_int() {
                Some(v) if (0..=i64::from(u32::MAX)).contains(&v) => {
                    cfg.version = u32::try_from(v).unwrap_or(cfg.version);
                }
                _ => w.push(Warning::WrongType {
                    key: "version".to_owned(),
                    expected: "整数",
                    found: entry.value.type_name(),
                    line: entry.line,
                }),
            }
        } else {
            w.push(Warning::UnknownKey {
                section: None,
                key: entry.key.clone(),
                line: entry.line,
            });
            extras.push(Extra {
                section: None,
                key: entry.key.clone(),
                value: entry.value.clone(),
            });
        }
    }

    for entry in doc.entries().iter().filter(|e| e.section.is_some()) {
        let Some(section) = entry.section.as_deref() else {
            continue;
        };
        let full = format!("{section}.{}", entry.key);
        if let Some(spec) = DOC_ONLY_KEYS
            .iter()
            .find(|s| s.section == section && s.key == entry.key)
        {
            w.push(Warning::UnimplementedKey {
                key: full,
                note: spec.note,
                line: entry.line,
            });
            // 值仍然保留：用户可能从指导文档抄了这个键
            extras.push(Extra {
                section: Some(section.to_owned()),
                key: entry.key.clone(),
                value: entry.value.clone(),
            });
            continue;
        }
        if !KEYS
            .iter()
            .any(|s| s.section == section && s.key == entry.key)
        {
            w.push(Warning::UnknownKey {
                section: Some(section.to_owned()),
                key: entry.key.clone(),
                line: entry.line,
            });
            extras.push(Extra {
                section: Some(section.to_owned()),
                key: entry.key.clone(),
                value: entry.value.clone(),
            });
            continue;
        }
        apply_key(&mut cfg, section, entry, &mut w);
    }

    cfg.migrate_version()
        .map_err(|e| format!("配置版本不受支持：{e}"))?;
    for cw in cfg.validate() {
        w.push(match cw {
            ConfigWarning::Clamped { field, applied } => Warning::Clamped {
                key: field.to_owned(),
                applied,
            },
            ConfigWarning::Replaced { field, reason } => Warning::Replaced {
                key: field.to_owned(),
                reason: reason.to_owned(),
            },
            ConfigWarning::Unsupported { field, reason } => Warning::Unsupported {
                key: field.to_owned(),
                reason: reason.to_owned(),
            },
        });
    }
    Ok(Decoded {
        config: cfg,
        warnings: w,
        extras,
    })
}

// 逐键赋值本质是一张扁平表（1 个键 1 行），拆成函数只会让"加一个键要改三处"。
#[allow(clippy::too_many_lines)]
fn apply_key(cfg: &mut Config, section: &str, e: &crate::toml::Entry, w: &mut Vec<Warning>) {
    let line = e.line;
    let full = format!("{section}.{}", e.key);
    let v = &e.value;
    match (section, e.key.as_str()) {
        ("general", "enabled") => take_bool(v, &full, line, w, |x| cfg.general.enabled = x),
        ("general", "autostart") => take_bool(v, &full, line, w, |x| cfg.general.autostart = x),
        ("general", "fps") => take_int(v, &full, line, w, |x| {
            cfg.general.fps = u32::try_from(x).unwrap_or(0);
        }),
        ("general", "fade_ms") => take_int(v, &full, line, w, |x| {
            cfg.general.fade_ms = u32::try_from(x).unwrap_or(0);
        }),
        ("general", "respect_reduced_motion") => {
            take_bool(v, &full, line, w, |x| {
                cfg.general.respect_reduced_motion = x;
            });
        }
        ("visual", "mode") => {
            let table = [
                ("FullDock", VisualMode::FullDock),
                ("LeftBlank", VisualMode::LeftBlank),
                ("RightBlank", VisualMode::RightBlank),
                ("AboveDock", VisualMode::AboveDock),
            ];
            if let Some(m) = take_enum(v, &full, line, &table, w) {
                cfg.visual.mode = m;
            }
        }
        ("visual", "opacity") => take_float(v, &full, line, w, |x| cfg.visual.opacity = x),
        ("visual", "bands") => take_int(v, &full, line, w, |x| {
            cfg.visual.bands = usize::try_from(x).unwrap_or(0);
        }),
        ("visual", "bar_max_height") => take_int(v, &full, line, w, |x| {
            cfg.visual.bar_max_height = u32::try_from(x).unwrap_or(0);
        }),
        ("visual", "bar_gap_ratio") => {
            take_float(v, &full, line, w, |x| cfg.visual.bar_gap_ratio = x);
        }
        ("visual", "color_start") => {
            take_string(v, &full, line, w, |x| cfg.visual.color_start = x);
        }
        ("visual", "color_end") => {
            take_string(v, &full, line, w, |x| cfg.visual.color_end = x);
        }
        ("visual", "palette") => {
            take_string(v, &full, line, w, |x| cfg.visual.palette = x);
        }
        ("visual", "palette_mode") => {
            take_string(v, &full, line, w, |x| cfg.visual.palette_mode = x);
        }
        ("visual", "blend") => {
            let table = [
                ("additive", BlendMode::Additive),
                ("alpha", BlendMode::Alpha),
            ];
            if let Some(m) = take_enum(v, &full, line, &table, w) {
                cfg.visual.blend = m;
            }
        }
        ("visual", "peak_cap") => take_bool(v, &full, line, w, |x| cfg.visual.peak_cap = x),
        ("visual", "rounded") => take_bool(v, &full, line, w, |x| cfg.visual.rounded = x),
        ("analyze", "fft_size") => take_int(v, &full, line, w, |x| {
            cfg.analyze.fft_size = usize::try_from(x).unwrap_or(0);
        }),
        ("analyze", "f_min") => take_float(v, &full, line, w, |x| cfg.analyze.f_min = x),
        ("analyze", "f_max") => take_float(v, &full, line, w, |x| cfg.analyze.f_max = x),
        ("analyze", "db_floor") => take_float(v, &full, line, w, |x| cfg.analyze.db_floor = x),
        ("analyze", "attack") => take_float(v, &full, line, w, |x| cfg.analyze.attack = x),
        ("analyze", "release") => take_float(v, &full, line, w, |x| cfg.analyze.release = x),
        ("analyze", "peak_hold_ms") => take_int(v, &full, line, w, |x| {
            cfg.analyze.peak_hold_ms = u32::try_from(x).unwrap_or(0);
        }),
        ("analyze", "peak_fall") => take_float(v, &full, line, w, |x| {
            cfg.analyze.peak_fall = x;
        }),
        ("analyze", "peak_gravity") => take_bool(v, &full, line, w, |x| {
            cfg.analyze.peak_gravity = x;
        }),
        ("analyze", "gamma") => take_float(v, &full, line, w, |x| cfg.analyze.gamma = x),
        ("analyze", "sensitivity") => {
            take_float(v, &full, line, w, |x| cfg.analyze.sensitivity = x);
        }
        ("behavior", "silence_timeout_ms") => take_int(v, &full, line, w, |x| {
            cfg.behavior.silence_timeout_ms = u64::try_from(x).unwrap_or(0);
        }),
        ("behavior", "silence_dbfs") => {
            take_float(v, &full, line, w, |x| cfg.behavior.silence_dbfs = x);
        }
        ("behavior", "hide_on_fullscreen") => take_bool(v, &full, line, w, |x| {
            cfg.behavior.hide_on_fullscreen = x;
        }),
        ("behavior", "hide_on_autohide") => take_bool(v, &full, line, w, |x| {
            cfg.behavior.hide_on_autohide = x;
        }),
        ("behavior", "hide_on_lock") => take_bool(v, &full, line, w, |x| {
            cfg.behavior.hide_on_lock = x;
        }),
        ("behavior", "pause_on_battery_30fps") => take_bool(v, &full, line, w, |x| {
            cfg.behavior.pause_on_battery_30fps = x;
        }),
        ("dock", "position") => take_string(v, &full, line, w, |x| cfg.dock.position = x),
        ("dock", "height") => take_int(v, &full, line, w, |x| {
            cfg.dock.height = u32::try_from(x).unwrap_or(0);
        }),
        ("dock", "center_mode") => take_bool(v, &full, line, w, |x| cfg.dock.center_mode = x),
        ("dock", "blank_width_px") => take_int(v, &full, line, w, |x| {
            cfg.dock.blank_width_px = u32::try_from(x).unwrap_or(0);
        }),
        ("dock", "icon_count_hint") => take_int(v, &full, line, w, |x| {
            cfg.dock.icon_count_hint = u32::try_from(x).unwrap_or(0);
        }),
        ("dock", "icon_size") => take_float(v, &full, line, w, |x| cfg.dock.icon_size = x),
        ("outputs", "mode") => {
            let table = [
                ("primary", OutputMode::Primary),
                ("all", OutputMode::All),
                ("names", OutputMode::Names),
            ];
            if let Some(m) = take_enum(v, &full, line, &table, w) {
                cfg.outputs.mode = m;
            }
        }
        ("outputs", "names") => {
            if let Some(items) = v.as_str_array() {
                cfg.outputs.names = items;
            } else {
                w.push(Warning::WrongType {
                    key: full,
                    expected: "字符串数组",
                    found: v.type_name(),
                    line,
                });
            }
        }
        ("advanced", "backend") => take_string(v, &full, line, w, |x| cfg.advanced.backend = x),
        ("advanced", "renderer") => take_string(v, &full, line, w, |x| cfg.advanced.renderer = x),
        ("advanced", "x11_stacking") => {
            let table = [
                ("managed-above", X11Stacking::ManagedAbove),
                ("managed-below", X11Stacking::ManagedBelow),
                ("override-redirect", X11Stacking::OverrideRedirect),
            ];
            if let Some(m) = take_enum(v, &full, line, &table, w) {
                cfg.advanced.x11_stacking = m;
            }
        }
        _ => {}
    }
}

fn take_bool(v: &Value, key: &str, line: usize, w: &mut Vec<Warning>, set: impl FnOnce(bool)) {
    match v.as_bool() {
        Some(x) => set(x),
        None => w.push(Warning::WrongType {
            key: key.to_owned(),
            expected: "布尔",
            found: v.type_name(),
            line,
        }),
    }
}

fn take_int(v: &Value, key: &str, line: usize, w: &mut Vec<Warning>, set: impl FnOnce(i64)) {
    match v.as_int() {
        Some(x) => set(x),
        None => w.push(Warning::WrongType {
            key: key.to_owned(),
            expected: "整数",
            found: v.type_name(),
            line,
        }),
    }
}

fn take_float(v: &Value, key: &str, line: usize, w: &mut Vec<Warning>, set: impl FnOnce(f32)) {
    match v.as_float() {
        Some(x) => set(x as f32),
        None => w.push(Warning::WrongType {
            key: key.to_owned(),
            expected: "数字",
            found: v.type_name(),
            line,
        }),
    }
}

fn take_string(v: &Value, key: &str, line: usize, w: &mut Vec<Warning>, set: impl FnOnce(String)) {
    match v.as_str() {
        Some(x) => set(x.to_owned()),
        None => w.push(Warning::WrongType {
            key: key.to_owned(),
            expected: "字符串",
            found: v.type_name(),
            line,
        }),
    }
}

/// 大小写、连字符、下划线都不敏感（`ManagedAbove` = `managed-above` = `managed_above`）。
fn normalize(s: &str) -> String {
    s.chars()
        .filter(char::is_ascii_alphanumeric)
        .flat_map(char::to_lowercase)
        .collect()
}

fn take_enum<T: Copy>(
    v: &Value,
    key: &str,
    line: usize,
    table: &[(&str, T)],
    w: &mut Vec<Warning>,
) -> Option<T> {
    let Some(s) = v.as_str() else {
        w.push(Warning::WrongType {
            key: key.to_owned(),
            expected: "字符串",
            found: v.type_name(),
            line,
        });
        return None;
    };
    let want = normalize(s);
    table
        .iter()
        .find(|(name, _)| normalize(name) == want)
        .map(|(_, val)| *val)
        .or_else(|| {
            let allowed: Vec<&str> = table.iter().map(|(n, _)| *n).collect();
            w.push(Warning::Replaced {
                key: key.to_owned(),
                reason: format!("只接受 {}", allowed.join(" / ")),
            });
            None
        })
}

/// 浮点写成 TOML 时**必须**带小数点：`1` 是整数，读回来会撞 [`Warning::WrongType`]。
fn num(v: f32) -> String {
    if !v.is_finite() {
        return "0.0".to_owned();
    }
    let s = format!("{v}");
    if s.contains('.') || s.contains('e') {
        s
    } else {
        format!("{s}.0")
    }
}

/// 序列化成 TOML（键序与指导文档 §6.8 一致，便于 diff）。
#[must_use]
pub fn encode(cfg: &Config) -> String {
    encode_with_extras(cfg, &[])
}

/// 序列化成 TOML，并把**本版本不认识的键原样放回**。
///
/// 设置界面会重写整个配置文件；没有这一步，用户手写的"未来键"会被程序擦掉。
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn encode_with_extras(cfg: &Config, extras: &[Extra]) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    out.push_str("# deepin-spectrum 配置（自动生成；改完保存即热生效，无需重启）\n");
    out.push_str("# 每个键的生效范围见 `deepin-spectrum settings --explain`\n");
    let _ = writeln!(out, "version = {}\n", cfg.version);

    let mut sections: Vec<(&'static str, Vec<String>)> = Vec::new();

    let general: &General = &cfg.general;
    sections.push((
        "general",
        vec![
            format!("enabled = {}", general.enabled),
            format!("autostart = {}", general.autostart),
            format!("fps = {}", general.fps),
            format!("fade_ms = {}", general.fade_ms),
            format!(
                "respect_reduced_motion = {}",
                general.respect_reduced_motion
            ),
        ],
    ));

    let visual: &Visual = &cfg.visual;
    sections.push((
        "visual",
        vec![
            format!("mode = {}", quote(mode_name(visual.mode))),
            format!("opacity = {}", num(visual.opacity)),
            format!("bands = {}", visual.bands),
            format!("bar_max_height = {}", visual.bar_max_height),
            format!("bar_gap_ratio = {}", num(visual.bar_gap_ratio)),
            format!("color_start = {}", quote(&visual.color_start)),
            format!("color_end = {}", quote(&visual.color_end)),
            format!("palette = {}", quote(&visual.palette)),
            format!("palette_mode = {}", quote(&visual.palette_mode)),
            format!("blend = {}", quote(blend_name(visual.blend))),
            format!("peak_cap = {}", visual.peak_cap),
            format!("rounded = {}", visual.rounded),
        ],
    ));

    let analyze = &cfg.analyze;
    sections.push((
        "analyze",
        vec![
            format!("fft_size = {}", analyze.fft_size),
            format!("f_min = {}", num(analyze.f_min)),
            format!("f_max = {}", num(analyze.f_max)),
            format!("db_floor = {}", num(analyze.db_floor)),
            format!("attack = {}", num(analyze.attack)),
            format!("release = {}", num(analyze.release)),
            format!("peak_hold_ms = {}", analyze.peak_hold_ms),
            format!("peak_fall = {}", num(analyze.peak_fall)),
            format!("peak_gravity = {}", analyze.peak_gravity),
            format!("gamma = {}", num(analyze.gamma)),
            format!("sensitivity = {}", num(analyze.sensitivity)),
        ],
    ));

    let behavior: &Behavior = &cfg.behavior;
    sections.push((
        "behavior",
        vec![
            format!("silence_timeout_ms = {}", behavior.silence_timeout_ms),
            format!("silence_dbfs = {}", num(behavior.silence_dbfs)),
            format!("hide_on_fullscreen = {}", behavior.hide_on_fullscreen),
            format!("hide_on_autohide = {}", behavior.hide_on_autohide),
            format!("hide_on_lock = {}", behavior.hide_on_lock),
            format!(
                "pause_on_battery_30fps = {}",
                behavior.pause_on_battery_30fps
            ),
        ],
    ));

    let dock: &Dock = &cfg.dock;
    sections.push((
        "dock",
        vec![
            format!("position = {}", quote(&dock.position)),
            format!("height = {}", dock.height),
            format!("center_mode = {}", dock.center_mode),
            format!("blank_width_px = {}", dock.blank_width_px),
            format!("icon_count_hint = {}", dock.icon_count_hint),
            format!("icon_size = {}", num(dock.icon_size)),
        ],
    ));

    let names: Vec<String> = cfg.outputs.names.iter().map(|n| quote(n)).collect();
    sections.push((
        "outputs",
        vec![
            format!("mode = {}", quote(output_name(cfg.outputs.mode))),
            format!("names = [{}]", names.join(", ")),
        ],
    ));

    let advanced: &Advanced = &cfg.advanced;
    sections.push((
        "advanced",
        vec![
            format!("backend = {}", quote(&advanced.backend)),
            format!("renderer = {}", quote(&advanced.renderer)),
            format!(
                "x11_stacking = {}",
                quote(stacking_name(advanced.x11_stacking))
            ),
        ],
    ));

    // 根级未知键放最前面（version 之后）
    for extra in extras.iter().filter(|e| e.section.is_none()) {
        let _ = writeln!(
            out,
            "{} = {}",
            encode_key(&extra.key),
            value_to_string(&extra.value)
        );
    }
    let _ = writeln!(out);

    for (name, lines) in &sections {
        let _ = writeln!(out, "[{}]", encode_key(name));
        for line in lines {
            let _ = writeln!(out, "{line}");
        }
        for extra in extras
            .iter()
            .filter(|e| e.section.as_deref() == Some(*name))
        {
            let _ = writeln!(
                out,
                "{} = {}",
                encode_key(&extra.key),
                value_to_string(&extra.value)
            );
        }
        let _ = writeln!(out);
    }

    // 属于未知 section 的键：单独开一个段
    let mut orphan_sections: Vec<&str> = Vec::new();
    for extra in extras {
        let Some(section) = extra.section.as_deref() else {
            continue;
        };
        if sections.iter().any(|(name, _)| *name == section) {
            continue;
        }
        if !orphan_sections.contains(&section) {
            orphan_sections.push(section);
        }
    }
    for section in orphan_sections {
        let _ = writeln!(out, "[{}]", encode_key(section));
        for extra in extras
            .iter()
            .filter(|e| e.section.as_deref() == Some(section))
        {
            let _ = writeln!(
                out,
                "{} = {}",
                encode_key(&extra.key),
                value_to_string(&extra.value)
            );
        }
        let _ = writeln!(out);
    }

    out
}

/// 裸键/裸段名可以直接写；含引号、空白、`]`、`=` 等字符时必须 quote。
fn encode_key(name: &str) -> String {
    if !name.is_empty()
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
    {
        name.to_owned()
    } else {
        quote(name)
    }
}

const fn mode_name(m: VisualMode) -> &'static str {
    match m {
        VisualMode::FullDock => "FullDock",
        VisualMode::LeftBlank => "LeftBlank",
        VisualMode::RightBlank => "RightBlank",
        VisualMode::AboveDock => "AboveDock",
    }
}

const fn blend_name(b: BlendMode) -> &'static str {
    match b {
        BlendMode::Additive => "additive",
        BlendMode::Alpha => "alpha",
    }
}

const fn output_name(o: OutputMode) -> &'static str {
    match o {
        OutputMode::Primary => "primary",
        OutputMode::All => "all",
        OutputMode::Names => "names",
    }
}

const fn stacking_name(s: X11Stacking) -> &'static str {
    match s {
        X11Stacking::ManagedAbove => "managed-above",
        X11Stacking::ManagedBelow => "managed-below",
        X11Stacking::OverrideRedirect => "override-redirect",
    }
}

/// 首次运行写出的带注释模板（等于全默认值）。
///
/// 顶部注释里的 `version` 与 [`Config::default`] 必须一致，
/// 单测 `template_equals_defaults` 会在两者分叉时立刻失败。
pub const TEMPLATE: &str = r##"# deepin-spectrum 配置（schema v1）
#
# 改完保存即热生效，不需要重启 daemon，也不需要重启托盘。
# 生效范围不确定时看： deepin-spectrum settings --explain
#
# 合法区间（越界会被钳制并告警，不会崩溃）：
#   opacity        0.05 ~ 1.0      → 渲染 alpha
#   bands          16 ~ 128        → 柱数（改它要重建分析器，仍在一帧内完成）
#   bar_gap_ratio  0.0 ~ 0.9       → 柱间隙 = round(ratio×10) 像素
#   fft_size       2 的幂（4096 推荐；2048 会让低频段塌缩，F56）
#   colors         #RRGGBB
version = 2

[general]
# 总开关：false = 立即清屏并停止上屏（采集不断，重新打开无缝继续）
enabled = true
# 开机自启（本版本未实现，需要 systemd user unit）
autostart = true
# 0 = 跟随刷新率（本版本帧率由 hop 决定，见 --explain）
fps = 0
fade_ms = 150
respect_reduced_motion = true

[visual]
mode = "FullDock"
opacity = 0.65
bands = 64
bar_max_height = 120
bar_gap_ratio = 0.2
# 柱底 → 柱顶的渐变；两色相同即纯色
color_start = "#00C8FF"
color_end = "#7B61FF"
# 多色标：留空即双色渐变；填 3~8 个 #RRGGBB 后配合 palette_mode 生效
palette = ""
palette_mode = "off"
blend = "additive"
peak_cap = true
rounded = true

[analyze]
fft_size = 4096
f_min = 40.0
f_max = 16000.0
db_floor = -80.0
attack = 0.55
release = 0.12
peak_hold_ms = 800
# 峰值滑块：被顶飞后悬停 800ms，再按 peak_fall 缓慢回落
# 0.02 是 v0.2.6 及之前的硬编码值；gravity = true 则越落越快
peak_fall = 0.02
peak_gravity = false
gamma = 0.8
sensitivity = 1.0

[behavior]
silence_timeout_ms = 800
silence_dbfs = -62.0
hide_on_fullscreen = true
hide_on_autohide = true
hide_on_lock = true
pause_on_battery_30fps = true

[dock]
position = "bottom"
# ★ 仅 D-Bus 探测失败时的兜底；运行时以任务栏实际高度为准（F38）
height = 45
center_mode = true
blank_width_px = 0
icon_count_hint = 10
icon_size = 32.0

[outputs]
mode = "primary"
names = []

[advanced]
backend = "auto"
renderer = "auto"
# override-redirect = 绕过窗口管理器：「显示桌面」/最小化不再隐藏频谱（F124）
# 改回 "managed-above" 则回到受管窗口
x11_stacking = "override-redirect"
"##;

#[cfg(test)]
mod tests {
    use super::{KEYS, Scope, TEMPLATE, Warning, decode, encode};
    use crate::toml::parse;
    use spectrum_core::config::{Config, VisualMode};

    fn decoded(src: &str) -> super::Decoded {
        decode(&parse(src).expect("语法错误")).expect("解码失败")
    }

    #[test]
    fn template_equals_defaults() {
        let d = decoded(TEMPLATE);
        assert_eq!(d.config, Config::default(), "模板与默认值分叉了");
        assert!(d.warnings.is_empty(), "模板不应产生告警：{:?}", d.warnings);
    }

    #[test]
    fn encode_round_trips() {
        let cfg = Config::default();
        let text = encode(&cfg);
        let d = decoded(&text);
        assert_eq!(d.config, cfg);
        assert!(d.warnings.is_empty(), "{:?}", d.warnings);
    }

    #[test]
    fn encode_round_trips_non_defaults() {
        let mut cfg = Config::default();
        cfg.visual.opacity = 1.0; // 整数取值：必须写成 "1.0" 才能读回 f32
        cfg.visual.bands = 96;
        cfg.visual.color_start = "#FF0000".to_owned();
        cfg.visual.color_end = "#FF0000".to_owned();
        cfg.visual.bar_gap_ratio = 0.0;
        cfg.analyze.fft_size = 2048;
        cfg.analyze.f_min = 80.0;
        cfg.analyze.sensitivity = 2.5;
        cfg.behavior.silence_dbfs = -50.0;
        cfg.general.enabled = false;
        cfg.general.fps = 60;
        cfg.visual.mode = VisualMode::AboveDock;
        cfg.outputs.names = vec!["DP-1".to_owned()];
        cfg.dock.height = 120;
        cfg.normalize().expect("校验失败");
        let d = decoded(&encode(&cfg));
        assert_eq!(d.config, cfg, "往返后配置变了");
        assert!(d.warnings.is_empty(), "{:?}", d.warnings);
    }

    #[test]
    fn quoted_unknown_keys_round_trip_without_injection() {
        let src = "[general]\n\"weird] key\" = 2\n[\"future section\"]\n\"x = y\" = 3\n";
        let d = decoded(src);
        assert_eq!(d.extras.len(), 2, "两个未知键都应保留：{:?}", d.extras);
        let text = super::encode_with_extras(&d.config, &d.extras);
        let d2 = decoded(&text);
        assert_eq!(d2.extras.len(), 2, "回写后未知键丢失：\n{text}");
        assert!(
            d2.extras.iter().any(|e| e.key == "weird] key"),
            "带 `]` 的未知键没有正确 quote：\n{text}"
        );
        assert!(
            d2.extras
                .iter()
                .any(|e| e.section.as_deref() == Some("future section")),
            "带空格的未知 section 没有正确 quote：\n{text}"
        );
    }

    #[test]
    fn left_blank_mode_is_preserved_and_warned() {
        let d = decoded("[visual]\nmode = \"LeftBlank\"\n");
        assert_eq!(d.config.visual.mode, VisualMode::LeftBlank);
        assert!(
            d.warnings
                .iter()
                .any(|w| matches!(w, Warning::Unsupported { key, .. } if key == "visual.mode")),
            "未实现模式必须有可见告警：{:?}",
            d.warnings
        );
    }

    #[test]
    fn unknown_and_doc_only_keys_warn() {
        let d = decoded(
            "[visual]\nopacity = 0.5\nnonsense = 1\ngradient = \"vertical\"\n[analyze]\nbands = 32\n",
        );
        assert!((d.config.visual.opacity - 0.5).abs() < 1e-9);
        assert_eq!(d.warnings.len(), 3, "{:?}", d.warnings);
        assert!(d.warnings.iter().any(|w| matches!(
            w, Warning::UnknownKey { key, .. } if key == "nonsense"
        )));
        assert!(d.warnings.iter().any(|w| matches!(
            w, Warning::UnimplementedKey { key, .. } if key == "visual.gradient"
        )));
        assert!(d.warnings.iter().any(|w| matches!(
            w, Warning::UnimplementedKey { key, .. } if key == "analyze.bands"
        )));
    }

    #[test]
    fn wrong_type_keeps_default_and_warns() {
        let d = decoded("[visual]\nopacity = \"loose\"\nbands = 1.5\n");
        assert!((d.config.visual.opacity - 0.65).abs() < 1e-9, "应保持默认");
        assert_eq!(d.config.visual.bands, 64, "浮点不得隐式取整成 1");
        assert_eq!(d.warnings.len(), 2);
    }

    #[test]
    fn illegal_values_are_clamped_with_warning() {
        let d = decoded("[visual]\nopacity = 9.0\nbands = 4\n");
        assert!((d.config.visual.opacity - 1.0).abs() < 1e-9);
        assert_eq!(d.config.visual.bands, 64);
        assert!(
            d.warnings
                .iter()
                .any(|w| matches!(w, Warning::Clamped { key, .. } if key == "visual.opacity"))
        );
    }

    #[test]
    fn enum_accepts_multiple_spellings() {
        for spelling in [
            "managed-above",
            "ManagedAbove",
            "managed_above",
            "MANAGEDABOVE",
        ] {
            let d = decoded(&format!("[advanced]\nx11_stacking = \"{spelling}\"\n"));
            assert_eq!(
                d.config.advanced.x11_stacking,
                spectrum_core::config::X11Stacking::ManagedAbove,
                "{spelling} 应当被接受"
            );
            assert!(d.warnings.is_empty(), "{spelling}: {:?}", d.warnings);
        }
        let bad = decoded("[advanced]\nx11_stacking = \"below\"\n");
        assert!(
            bad.warnings.iter().any(
                |w| matches!(w, Warning::Replaced { key, .. } if key == "advanced.x11_stacking")
            )
        );
    }

    #[test]
    fn future_version_is_rejected() {
        let doc = parse("version = 999\n").expect("语法错误");
        assert!(decode(&doc).is_err(), "未来版本必须报错而不是猜");
    }

    #[test]
    fn empty_document_is_defaults() {
        let d = decoded("");
        assert_eq!(d.config, Config::default());
        assert!(d.warnings.is_empty());
    }

    #[test]
    fn every_key_in_registry_decodes_without_warning() {
        // 反向门：registry 里有但 apply_key 没处理的键，会静默保持默认值。
        // 用 encode 的输出逐键确认「键名识别 + 类型正确」。
        let text = encode(&Config::default());
        let d = decoded(&text);
        assert!(d.warnings.is_empty());
        for spec in KEYS {
            let needle = format!("{}.{}", spec.section, spec.key);
            assert!(
                text.contains(&format!("\n{} = ", spec.key)),
                "encode 缺少键 {needle}"
            );
        }
    }

    #[test]
    fn hide_on_fullscreen_is_live() {
        let spec = KEYS
            .iter()
            .find(|s| s.section == "behavior" && s.key == "hide_on_fullscreen")
            .expect("registry 缺少 behavior.hide_on_fullscreen");
        assert_eq!(spec.scope, Scope::Live);
    }

    #[test]
    fn x11_stacking_is_startup_scope() {
        let spec = KEYS
            .iter()
            .find(|s| s.section == "advanced" && s.key == "x11_stacking")
            .expect("registry 缺少 advanced.x11_stacking");
        assert_eq!(spec.scope, Scope::Startup);
    }

    #[test]
    fn scope_labels_are_distinct() {
        let labels: Vec<String> = [
            Scope::Live,
            Scope::LiveRebuild,
            Scope::Startup,
            Scope::Unimplemented,
        ]
        .iter()
        .map(ToString::to_string)
        .collect();
        assert_eq!(labels.len(), 4);
        assert!(labels.contains(&"热生效".to_owned()));
        assert!(labels.contains(&"未实现".to_owned()));
    }

    #[test]
    fn hot_reloadable_keys_are_a_strict_subset() {
        // 至少要有这些键能热改，否则"热重载"名不副实。
        for key in [
            "general.enabled",
            "visual.opacity",
            "visual.bands",
            "visual.color_start",
            "visual.color_end",
            "visual.bar_gap_ratio",
            "analyze.fft_size",
            "analyze.sensitivity",
            "behavior.silence_dbfs",
            "behavior.silence_timeout_ms",
        ] {
            let (section, name) = key.split_once('.').expect("键名带点");
            let spec = KEYS
                .iter()
                .find(|s| s.section == section && s.key == name)
                .unwrap_or_else(|| panic!("registry 缺少 {key}"));
            assert!(
                matches!(spec.scope, Scope::Live | Scope::LiveRebuild),
                "{key} 应当是热生效的"
            );
        }
    }
}
