//! 设置界面（Slint 1.18，**GPL-3.0-only** —— §11.1 方案 A）。
//!
//! ## 这个界面做什么、不做什么
//!
//! - **做**：把已实现的参数做成控件，改动后自动落盘，daemon 250ms 内热应用；
//! - **不做**：不显示未实现的键（`visual.rounded` / `general.fps` / …）。
//!   界面上出现"改了没反应"的控件比没有更糟 —— 用户分不清是程序坏了还是自己没找对。
//!   其余键的生效范围用 `deepin-spectrum settings --explain` 查。
//!
//! ## 与 daemon 的耦合
//!
//! **零耦合**：界面只负责写配置文件，daemon 通过内容哈希轮询发现变化。
//! 因此界面可以随时关、daemon 可以独立跑，两者互不阻塞。
//!
//! ## 写盘策略
//!
//! 滑块拖动会连续触发 `changed`（每秒几十次）。这里用
//! [`spectrum_settings::writer::ConfigWriter`] 合并：300ms 窗口内只保留最后一次改动，
//! 落盘一次 —— daemon 也只会被热应用一次。

// 界面里的数值都是小范围量（柱数 ≤128、超时 ≤60000ms、门限 ±100dB），
// f32 ↔ usize/u64 的窄化/精度换算不会越界。
//
// `float_cmp`：这里比的是"**同一个来源**的值有没有被钳制过"，
// 必须逐位相等，用近似比较反而会漏掉改动。
#![allow(clippy::float_cmp)]
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use slint::{ComponentHandle, ModelRc, SharedString, Timer, TimerMode, VecModel};
use spectrum_core::config::{Config, VisualMode};
use spectrum_settings::presets::{self, UserPresetStore};
use spectrum_settings::store::ConfigStore;
use spectrum_settings::writer::ConfigWriter;

use crate::i18n::{Lang, UiPrefs};
use crate::preview::{Preview, PreviewStyle};

pub mod autostart;
pub mod daemonctl;
pub mod i18n;
pub mod preview;
pub mod trayctl;

/// Slint 生成的代码。
///
/// 生成的代码**不是我们写的**，却会被 workspace 的 deny 级 lint 扫到
/// （例如它内部写 `sp::Option<sp::SharedString>`，触发 `unused_qualifications`）。
/// 因此整块放开 —— 对生成代码做人工 lint 审查没有意义。
#[allow(unused_qualifications, missing_docs, clippy::all, clippy::pedantic)]
mod generated {
    slint::include_modules!();
}

use generated::{Gui, PresetCard, Strings as SlintStrings, UserCard};

/// 合并写盘的等待窗口（略大于 daemon 的 250ms 轮询，保证一次拖动只落盘一次）。
const WRITE_DELAY: Duration = Duration::from_millis(300);

/// FFT 档位（滑块下标 → 点数）。
const FFT_SIZES: [usize; 4] = [1024, 2048, 4096, 8192];

/// 界面侧共享状态（语言、当前方案、是否被手动改过）。
#[derive(Debug)]
struct UiState {
    lang: Lang,
    current_id: Option<String>,
    custom: bool,
}
impl UiState {
    const fn new() -> Self {
        Self {
            lang: Lang::Zh,
            current_id: None,
            custom: false,
        }
    }
}

/// 6 个情绪分类：`id / 中文名 / 英文名 / 中文副标题 / 英文副标题`。
/// 顺序与 `presets.json` 的 `families` 一致，由 `verify_design.py` 守门。
const FAMILY_META: [(&str, &str, &str, &str, &str); 6] = [
    (
        "cheer",
        "欢快明快",
        "Cheerful",
        "高饱和、跳跃、带笑意",
        "Bright, bouncy, high saturation",
    ),
    (
        "calm",
        "平静专注",
        "Calm",
        "低对比、慢呼吸、不抢戏",
        "Low contrast, slow breathing",
    ),
    (
        "warm",
        "温暖怀旧",
        "Warm",
        "琥珀、黑胶、老设备的余温",
        "Amber, vinyl, old gear",
    ),
    (
        "melancholy",
        "忧伤低回",
        "Melancholy",
        "低饱和冷调、拖得很慢",
        "Muted cool tones, slow fall",
    ),
    (
        "serious",
        "严肃仪表",
        "Serious",
        "克制、精确、工具感",
        "Restrained, precise, instrument-like",
    ),
    (
        "rad",
        "炫酷实验",
        "Radical",
        "强对比、快起音、敢出格",
        "High contrast, fast attack, loud",
    ),
];

/// `ComboBox` 下标 → `visual.palette_mode`。
const PALETTE_MODES: [&str; 4] = ["off", "across", "per_bar", "vertical"];

fn palette_mode_index(mode: &str) -> i32 {
    PALETTE_MODES
        .iter()
        .position(|m| *m == mode)
        .and_then(|i| i32::try_from(i).ok())
        .unwrap_or(0)
}

fn hex_u32(s: &str) -> u32 {
    let b = s.as_bytes();
    if b.len() == 7 && b[0] == b'#' {
        u32::from_str_radix(&s[1..], 16).unwrap_or(0)
    } else {
        0
    }
}

#[allow(clippy::many_single_char_names)] // RGB 通道插值里 a/b/t/x/y 是领域惯例
fn mix_color(a: u32, b: u32, t: u32) -> u32 {
    let ch = |shift: u32| {
        let x = (a >> shift) & 0xFF;
        let y = (b >> shift) & 0xFF;
        ((x * (255 - t) + y * t) / 255) & 0xFF
    };
    (ch(16) << 16) | (ch(8) << 8) | ch(0)
}

/// 取 5 个色标样本：多色方案沿调色板等距采样，双色方案在起止色间插值。
/// 色卡与「当前色标」条共用，保证界面与真实渲染的取色一致。
fn palette_samples(palette: &str, start_hex: &str, end_hex: &str) -> [u32; 5] {
    let (pal, len) = spectrum_core::config::parse_palette(palette);
    let mut out = [0_u32; 5];
    for (k, v) in out.iter_mut().enumerate() {
        let t = (k as u32) * 255 / 4;
        *v = if len >= 3 {
            let segments = u32::from(len - 1);
            let pos = t * segments;
            let seg = (pos / 255).min(segments - 1);
            let f = pos.saturating_sub(seg * 255).min(255);
            mix_color(pal[seg as usize], pal[(seg + 1) as usize], f)
        } else {
            mix_color(hex_u32(start_hex), hex_u32(end_hex), t)
        };
    }
    out
}

/// 静态文案表 → Slint 生成的结构。
fn to_slint_strings(s: &i18n::Strings) -> SlintStrings {
    SlintStrings {
        app_name: s.app_name.into(),
        contents: s.contents.into(),
        mast_title: s.mast_title.into(),
        custom_label: s.custom_label.into(),
        mast_subtitle: s.mast_subtitle.into(),
        nav_presets: s.nav_presets.into(),
        nav_tune: s.nav_tune.into(),
        nav_motion: s.nav_motion.into(),
        nav_runtime: s.nav_runtime.into(),
        theme_paper: s.theme_paper.into(),
        theme_ink: s.theme_ink.into(),
        stage_kicker: s.stage_kicker.into(),
        dock_dark: s.dock_dark.into(),
        dock_light: s.dock_light.into(),
        slow_on: s.slow_on.into(),
        slow_off: s.slow_off.into(),
        play: s.play.into(),
        pause: s.pause.into(),
        presets_title: s.presets_title.into(),
        presets_desc: s.presets_desc.into(),
        group_hint: s.group_hint.into(),
        save_bar_title: s.save_bar_title.into(),
        save_bar_desc: s.save_bar_desc.into(),
        palette_title: s.palette_title.into(),
        palette_desc: s.palette_desc.into(),
        palette_mode: s.palette_mode.into(),
        palette_stops: s.palette_stops.into(),
        pm_off: s.pm_off.into(),
        pm_across: s.pm_across.into(),
        pm_per_bar: s.pm_per_bar.into(),
        pm_vertical: s.pm_vertical.into(),
        runtime_mode_label: s.runtime_mode_label.into(),
        mode_full: s.mode_full.into(),
        mode_above: s.mode_above.into(),
        mine_title: s.mine_title.into(),
        mine_desc: s.mine_desc.into(),
        save_hint: s.save_hint.into(),
        save_btn: s.save_btn.into(),
        delete_btn: s.delete_btn.into(),
        use_btn: s.use_btn.into(),
        now_label: s.now_label.into(),
        best_dark: s.best_dark.into(),
        best_light: s.best_light.into(),
        best_both: s.best_both.into(),
        tune_title: s.tune_title.into(),
        opacity: s.opacity.into(),
        bands: s.bands.into(),
        gap: s.gap.into(),
        color_start: s.color_start.into(),
        color_end: s.color_end.into(),
        fx_title: s.fx_title.into(),
        fx_desc: s.fx_desc.into(),
        fx_enable: s.fx_enable.into(),
        fx_hold: s.fx_hold.into(),
        fx_fall: s.fx_fall.into(),
        fx_gravity: s.fx_gravity.into(),
        motion_title: s.motion_title.into(),
        sensitivity: s.sensitivity.into(),
        attack: s.attack.into(),
        release: s.release.into(),
        db_floor: s.db_floor.into(),
        fft: s.fft.into(),
        runtime_title: s.runtime_title.into(),
        start: s.start.into(),
        stop: s.stop.into(),
        autostart: s.autostart.into(),
        tray: s.tray.into(),
        fullscreen: s.fullscreen.into(),
        silence_dbfs: s.silence_dbfs.into(),
        silence_timeout: s.silence_timeout.into(),
        footer_reset: s.footer_reset.into(),
        footer_hint: s.footer_hint.into(),
        status_saved: s.status_saved.into(),
        status_applied: s.status_applied.into(),
        status_custom: s.status_custom.into(),
        lang_label: s.lang_label.into(),
        nav_about: s.nav_about.into(),
        about_title: s.about_title.into(),
        about_kicker: s.about_kicker.into(),
        about_tagline: s.about_tagline.into(),
        about_author: s.about_author.into(),
        about_email: s.about_email.into(),
        about_license: s.about_license.into(),
        about_version: s.about_version.into(),
        about_stack: s.about_stack.into(),
        about_license_note: s.about_license_note.into(),
        about_license_file: s.about_license_file.into(),
        about_thanks: s.about_thanks.into(),
    }
}

/// 回填当前语言的所有文案，包括两个 `ComboBox` 的选项与 6 个分类标题。
fn apply_language(ui: &Gui, state: &Rc<RefCell<UiState>>) {
    let st = state.borrow();
    let s = i18n::strings(st.lang);
    let en = st.lang == Lang::En;
    ui.set_s(to_slint_strings(s));
    ui.set_lang_en(en);
    ui.set_mode_labels(ModelRc::new(VecModel::from(vec![
        SharedString::from(s.mode_full),
        SharedString::from(s.mode_above),
    ])));
    ui.set_palette_mode_labels(ModelRc::new(VecModel::from(vec![
        SharedString::from(s.pm_off),
        SharedString::from(s.pm_across),
        SharedString::from(s.pm_per_bar),
        SharedString::from(s.pm_vertical),
    ])));
    let titles: Vec<SharedString> = FAMILY_META
        .iter()
        .map(|m| SharedString::from(if en { m.2 } else { m.1 }))
        .collect();
    let subs: Vec<SharedString> = FAMILY_META
        .iter()
        .enumerate()
        .map(|(i, m)| {
            SharedString::from(format!(
                "NO.{:02}  ·  {}  ·  {}",
                i + 1,
                if en { m.4 } else { m.3 },
                if en { "5 PRESETS" } else { "5 套" }
            ))
        })
        .collect();
    ui.set_group_titles(ModelRc::new(VecModel::from(titles)));
    ui.set_group_subs(ModelRc::new(VecModel::from(subs)));
}

/// 刷新「多色标」区的色标条与说明。
fn refresh_palette_ui(ui: &Gui, cfg: &Config) {
    let samples = palette_samples(
        &cfg.visual.palette,
        &cfg.visual.color_start,
        &cfg.visual.color_end,
    );
    let swatch: Vec<_> = samples
        .iter()
        .map(|v| color_of(&format!("#{v:06X}")))
        .collect();
    ui.set_palette_swatch(ModelRc::new(VecModel::from(swatch)));
    let (_, len) = spectrum_core::config::parse_palette(&cfg.visual.palette);
    let count = if len >= 3 { len } else { 2 };
    ui.set_palette_info(SharedString::from(if ui.get_lang_en() {
        format!("{count} stops")
    } else {
        format!("{count} 色标")
    }));
}

/// 重建 6 个情绪分类的色卡（每类一行 5 张，高度由 Slint 组件固定）。
fn refresh_cards(ui: &Gui, state: &Rc<RefCell<UiState>>) {
    let st = state.borrow();
    let en = st.lang == Lang::En;
    let mut groups: [Vec<PresetCard>; 6] = std::array::from_fn(|_| Vec::new());
    for (i, p) in presets::PRESETS.iter().enumerate() {
        let gi = FAMILY_META
            .iter()
            .position(|m| m.0 == p.family)
            .unwrap_or(5);
        let selected = !st.custom && st.current_id.as_deref() == Some(p.id);
        let sw = palette_samples(p.palette, p.color_start, p.color_end);
        groups[gi].push(PresetCard {
            id: SharedString::from(p.id),
            no: SharedString::from(format!("{:02}", i + 1)),
            name: SharedString::from(if en { p.en } else { p.name }),
            en: SharedString::from(if en { FAMILY_META[gi].4 } else { p.en }),
            keys: SharedString::from(format!(
                "{} · {}% · {}ms",
                p.bands,
                (p.opacity * 100.0).round() as i32,
                p.peak_hold_ms
            )),
            c0: color_of(&format!("#{:06X}", sw[0])),
            c1: color_of(&format!("#{:06X}", sw[1])),
            c2: color_of(&format!("#{:06X}", sw[2])),
            c3: color_of(&format!("#{:06X}", sw[3])),
            c4: color_of(&format!("#{:06X}", sw[4])),
            selected,
        });
    }
    drop(st);
    let mut it = groups.into_iter();
    ui.set_group0(ModelRc::new(VecModel::from(it.next().unwrap_or_default())));
    ui.set_group1(ModelRc::new(VecModel::from(it.next().unwrap_or_default())));
    ui.set_group2(ModelRc::new(VecModel::from(it.next().unwrap_or_default())));
    ui.set_group3(ModelRc::new(VecModel::from(it.next().unwrap_or_default())));
    ui.set_group4(ModelRc::new(VecModel::from(it.next().unwrap_or_default())));
    ui.set_group5(ModelRc::new(VecModel::from(it.next().unwrap_or_default())));
}

/// 重建「我的方案」列表。
fn refresh_users(ui: &Gui, state: &Rc<RefCell<UiState>>) {
    let st = state.borrow();
    let cards: Vec<UserCard> = UserPresetStore::open()
        .list()
        .iter()
        .map(|u| UserCard {
            id: SharedString::from(u.id.as_str()),
            name: SharedString::from(u.name.as_str()),
            meta: SharedString::from(format!(
                "{} bars · {} FFT · {}",
                u.cfg.visual.bands,
                u.cfg.analyze.fft_size,
                if u.cfg.analyze.peak_gravity {
                    "GRAVITY"
                } else {
                    "LINEAR"
                }
            )),
            active: st.current_id.as_deref() == Some(u.id.as_str()),
        })
        .collect();
    drop(st);
    ui.set_user_cards(ModelRc::new(VecModel::from(cards)));
}

/// 舞台下方的「当前方案」标题与参数摘要。
fn update_now(ui: &Gui, state: &Rc<RefCell<UiState>>, cfg: &Config) {
    let st = state.borrow();
    let s = i18n::strings(st.lang);
    let name = if st.custom {
        s.custom_label.to_owned()
    } else if let Some(id) = st.current_id.as_deref() {
        presets::by_id(id)
            .map(|p| if st.lang == Lang::En { p.en } else { p.name }.to_owned())
            .or_else(|| UserPresetStore::open().get(id).map(|u| u.name))
            .unwrap_or_default()
    } else {
        String::new()
    };
    drop(st);
    ui.set_now_name(SharedString::from(name));
    ui.set_now_meta(SharedString::from(format!(
        "{} bars · {} FFT · {}ms",
        cfg.visual.bands, cfg.analyze.fft_size, cfg.analyze.peak_hold_ms
    )));
}

/// 立即生成并回填一帧预览。
fn render_preview(ui: &Gui, cfg: &Config, pv: &Rc<RefCell<Preview>>, speed: f32, substeps: usize) {
    let style = PreviewStyle::from_config(cfg);
    let image = pv.borrow_mut().frame(&style, speed, substeps);
    ui.set_preview_image(image);
}

/// 当前是否为 Treeland 会话。
///
/// 只有 Treeland 会"合成器画一条标题栏 + winit 再画一条客户端标题栏"；
/// 其它窗管（X11 / 普通 Wayland）只画一条，不能一并关掉装饰，否则会变成零标题栏。
///
/// 判据按可靠性排序：显式环境变量 → socket 名 → 运行时目录下的 treeland socket。
fn is_treeland_session() -> bool {
    if std::env::var("DDE_CURRENT_COMPOSITOR")
        .is_ok_and(|value| value.eq_ignore_ascii_case("treeland"))
    {
        return true;
    }
    if std::env::var("WAYLAND_DISPLAY").is_ok_and(|value| value.contains("treeland")) {
        return true;
    }
    std::env::var_os("XDG_RUNTIME_DIR")
        .is_some_and(|dir| PathBuf::from(dir).join("treeland.socket").is_file())
}

/// 打开设置窗口。
///
/// # Errors
///
/// 窗口创建失败、配置文件不可写。
#[allow(clippy::too_many_lines, clippy::similar_names)]
pub fn run(path: &Path) -> Result<(), String> {
    let _instance = GuiLock::acquire()?;
    let ui = Gui::new().map_err(|e| format!("创建窗口失败：{e}"))?;
    // Treeland 只在 client 模式下隐藏原生标题栏，而它自己不会补标题栏；
    // winit 的 Adwaita 标题栏又会把中文渲染成方块。因此 Treeland 会话改为
    // 无边框 + 应用内自绘标题栏（gui.slint 的 frameless 分支）；
    // X11/普通 Wayland 保持窗管标题栏，不能误关，否则会变成零标题栏。
    ui.set_frameless(is_treeland_session());

    let store = ConfigStore::open(path);
    let base = Rc::new(RefCell::new(store.config().clone()));
    let extras = store.extras().to_vec();
    let suppress = Arc::new(AtomicBool::new(true));
    let state = Rc::new(RefCell::new(UiState::new()));
    let prefs = UiPrefs::open();
    let preview = Rc::new(RefCell::new(Preview::new(900, 96)));
    let last_display_mode = Rc::new(Cell::new(0));

    fill(&ui, &base.borrow());
    // 关于页的专有名词与版本号：版本直接取自 Cargo 元数据，避免手抄漂移。
    ui.set_about_author(SharedString::from("Feelup"));
    ui.set_about_email(SharedString::from("testangel@163.com"));
    ui.set_about_version(SharedString::from(env!("CARGO_PKG_VERSION")));
    last_display_mode.set(ui.get_display_mode());
    state.borrow_mut().lang = prefs.load();
    apply_language(&ui, &state);
    refresh_palette_ui(&ui, &base.borrow()); // 用已保存的语言重刷色标说明
    refresh_cards(&ui, &state);
    refresh_users(&ui, &state);
    update_now(&ui, &state, &base.borrow());
    render_preview(&ui, &base.borrow(), &preview, 1.0, 9);

    // ---- 运行时开关的初始状态：以系统为准回填 ----
    // 开机自启的权威源是 systemd 的 enable 链接，托盘的权威源是正在运行的托盘进程；
    // 二者都不在 Config 的 fill/collect 链路里，必须单独回填。
    // 否则每次打开界面都显示默认"未勾选"，看起来像上一次的设置被自动关闭了。
    let initial_autostart = autostart::is_enabled().unwrap_or(false);
    ui.set_autostart(initial_autostart);
    ui.set_tray_enabled(trayctl::is_running());
    // 只同步内存里的意图记录，不写盘：保持"仅打开界面不改配置文件"的契约。
    base.borrow_mut().general.autostart = initial_autostart;

    ui.set_status_text(SharedString::from(
        i18n::strings(state.borrow().lang).status_saved,
    ));

    let weak = ui.as_weak();
    let initial_hash = spectrum_settings::store::content_hash(path);
    let writer = Rc::new(ConfigWriter::spawn_checked(
        path.to_path_buf(),
        extras,
        WRITE_DELAY,
        initial_hash,
        Some(Arc::new(move |result| {
            let failure = match result {
                Ok(()) => None,
                Err(err) => Some(format!("保存失败：{err}")),
            };
            let inner_weak = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                let Some(ui) = inner_weak.upgrade() else {
                    return;
                };
                match failure {
                    None => {
                        let s = if ui.get_lang_en() {
                            i18n::EN.status_saved
                        } else {
                            i18n::ZH.status_saved
                        };
                        ui.set_status_text(SharedString::from(s));
                        ui.set_status_error(false);
                    }
                    Some(err) => {
                        ui.set_status_text(SharedString::from(err));
                        ui.set_status_error(true);
                    }
                }
            });
        })),
    ));

    let weak = ui.as_weak();
    let base_cb = Rc::clone(&base);
    let suppress_cb = Arc::clone(&suppress);
    let writer_cb = Rc::clone(&writer);
    let last_mode_cb = Rc::clone(&last_display_mode);
    let state_cb = Rc::clone(&state);
    let preview_cb = Rc::clone(&preview);
    ui.on_config_changed(move || {
        if suppress_cb.load(Ordering::SeqCst) {
            return;
        }
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let ui_mode = ui.get_display_mode();
        let dirty = ui_mode != last_mode_cb.get();
        last_mode_cb.set(ui_mode);
        let mut cfg = base_cb.borrow().clone();
        collect(&ui, &mut cfg, dirty);
        let before = cfg.clone();
        let warnings = cfg.validate();
        sync_back(&ui, &before, &cfg);
        if *base_cb.borrow() == cfg {
            return;
        }
        *base_cb.borrow_mut() = cfg.clone();
        writer_cb.submit(cfg.clone());
        state_cb.borrow_mut().custom = true;
        let s = i18n::strings(state_cb.borrow().lang);
        ui.set_status_text(SharedString::from(s.status_custom));
        ui.set_status_error(false);
        update_now(&ui, &state_cb, &cfg);
        refresh_cards(&ui, &state_cb); // 手动改过参数就取消色卡选中态，避免「看着是 A 实际是 B」
        render_preview(&ui, &cfg, &preview_cb, 1.0, 1);
        if let Some(w) = warnings.first() {
            ui.set_status_text(SharedString::from(format!("{w:?}")));
            ui.set_status_error(true);
        }
    });

    let weak = ui.as_weak();
    let suppress_reset = Arc::clone(&suppress);
    let last_mode_reset = Rc::clone(&last_display_mode);
    let state_reset = Rc::clone(&state);
    let preview_reset = Rc::clone(&preview);
    ui.on_reset_defaults(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let defaults = Config::default();
        suppress_reset.store(true, Ordering::SeqCst);
        fill(&ui, &defaults);
        suppress_reset.store(false, Ordering::SeqCst);
        last_mode_reset.set(-1);
        ui.invoke_config_changed();
        {
            let mut st = state_reset.borrow_mut();
            st.custom = false;
            st.current_id = Some(presets::DEFAULT_PRESET.to_owned());
        }
        refresh_cards(&ui, &state_reset);
        refresh_users(&ui, &state_reset);
        update_now(&ui, &state_reset, &defaults);
        render_preview(&ui, &defaults, &preview_reset, 1.0, 1);
    });

    // ---- 频谱运行控制 ----
    // 状态轮询放后台线程（is_running 有 socket 往返，别堵 UI 线程），
    // 结果经 invoke_from_event_loop 回 UI。
    let weak = ui.as_weak();
    let _status_watcher = daemonctl::spawn_status_watcher(Arc::new(move |running, text| {
        let text = text.to_owned();
        let inner_weak = weak.clone();
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = inner_weak.upgrade() else {
                return;
            };
            ui.set_daemon_running(running);
            ui.set_daemon_status_text(SharedString::from(text));
        });
    }));

    let weak = ui.as_weak();
    // daemon 必须读界面正在编辑的同一个文件（`--config` 透传）。
    let start_config = path.to_path_buf();
    // start/stop 都含 socket 往返（stop 最长 800ms），必须在后台线程执行；
    // 同一时间只允许一个操作，避免连点产生多个进程/竞态。
    let control_busy = Arc::new(AtomicBool::new(false));
    let busy = Arc::clone(&control_busy);
    ui.on_start_spectrum(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if busy.swap(true, Ordering::SeqCst) {
            ui.set_status_text(SharedString::from("上一条操作还没结束，请稍候"));
            ui.set_status_error(false);
            return;
        }
        ui.set_status_text(SharedString::from("正在启动频谱…"));
        ui.set_status_error(false);

        let weak = ui.as_weak();
        let config = start_config.clone();
        let busy = Arc::clone(&busy);
        std::thread::spawn(move || {
            // 已在运行就不要再 spawn 一个：子进程会立刻因单实例保护退出。
            let result = if daemonctl::is_running() {
                Ok(false)
            } else {
                // 内部先静默重试一次，确认存活后才算成功；仍失败会带回日志末行。
                daemonctl::start_verified(Some(&config)).map(|()| true)
            };
            busy.store(false, Ordering::SeqCst);
            let _ = slint::invoke_from_event_loop(move || {
                let Some(ui) = weak.upgrade() else {
                    return;
                };
                match result {
                    Ok(true) => {
                        // 已确认存活，立即更新按钮状态；500ms 轮询随后继续校准。
                        ui.set_daemon_running(true);
                        ui.set_daemon_status_text(SharedString::from("运行中"));
                        ui.set_status_text(SharedString::from("已启动频谱（采集真实音频）"));
                        ui.set_status_error(false);
                    }
                    Ok(false) => {
                        ui.set_daemon_running(true);
                        ui.set_daemon_status_text(SharedString::from("运行中"));
                        ui.set_status_text(SharedString::from("频谱已在运行"));
                        ui.set_status_error(false);
                    }
                    Err(err) => {
                        ui.set_status_text(SharedString::from(format!("启动失败：{err}")));
                        ui.set_status_error(true);
                    }
                }
            });
        });
    });

    let weak = ui.as_weak();
    let busy = Arc::clone(&control_busy);
    ui.on_stop_spectrum(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if busy.swap(true, Ordering::SeqCst) {
            ui.set_status_text(SharedString::from("上一条操作还没结束，请稍候"));
            ui.set_status_error(false);
            return;
        }
        ui.set_status_text(SharedString::from("正在停止频谱…"));
        ui.set_status_error(false);

        let weak = ui.as_weak();
        let busy = Arc::clone(&busy);
        std::thread::spawn(move || {
            let result = if daemonctl::is_running() {
                daemonctl::stop().map(|()| true)
            } else {
                Ok(false)
            };
            busy.store(false, Ordering::SeqCst);
            let _ = slint::invoke_from_event_loop(move || {
                let Some(ui) = weak.upgrade() else {
                    return;
                };
                match result {
                    Ok(true) => {
                        // `stop()` 已等到进程真正退出，立即更新按钮状态。
                        ui.set_daemon_running(false);
                        ui.set_daemon_status_text(SharedString::from("未运行"));
                        ui.set_status_text(SharedString::from("已停止频谱"));
                        ui.set_status_error(false);
                    }
                    Ok(false) => {
                        ui.set_daemon_running(false);
                        ui.set_daemon_status_text(SharedString::from("未运行"));
                        ui.set_status_text(SharedString::from("频谱未运行"));
                        ui.set_status_error(false);
                    }
                    Err(err) => {
                        ui.set_status_text(SharedString::from(format!("停止失败：{err}")));
                        ui.set_status_error(true);
                    }
                }
            });
        });
    });

    // ---- P1.5：导航 / 筛选 / 语言 / 色卡 / 我的方案 / 动画预览 ----
    let weak = ui.as_weak();
    ui.on_nav_changed(move |index| {
        if let Some(ui) = weak.upgrade() {
            ui.set_panel_index(index);
        }
    });

    let weak = ui.as_weak();
    let prefs_lang = prefs.clone();
    let state_lang = Rc::clone(&state);
    let base_lang = Rc::clone(&base);
    ui.on_language_changed(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let lang = if ui.get_lang_en() { Lang::En } else { Lang::Zh };
        state_lang.borrow_mut().lang = lang;
        let saved = prefs_lang.save(lang);
        apply_language(&ui, &state_lang);
        refresh_cards(&ui, &state_lang);
        refresh_users(&ui, &state_lang);
        update_now(&ui, &state_lang, &base_lang.borrow());
        refresh_palette_ui(&ui, &base_lang.borrow());
        match saved {
            Ok(()) => {
                ui.set_status_text(SharedString::from(i18n::strings(lang).status_saved));
                ui.set_status_error(false);
            }
            Err(err) => {
                ui.set_status_text(SharedString::from(err));
                ui.set_status_error(true);
            }
        }
    });

    // ---- 开机自启 / 托盘开关：真正落到 systemd 与 XDG autostart ----
    // gui.slint 里这两个 CheckBox 的 toggled 会触发 autostart_changed / tray_changed，
    // 但此前 Rust 侧从未注册处理器，导致勾选只改了界面布尔值、系统侧毫无动作，
    // 且下次打开又被默认值回填成"关闭"。这里补上：
    //   ① 调用 autostart::set_enabled / trayctl::set_enabled 做真实变更；
    //   ② 失败时回滚界面勾态（不能让界面显示一个没生效的状态）。
    // general.autostart 不是权威源（autostart.rs 明确以 systemd 为准，collect() 也
    // 从不收集它），因此不写回 config —— 否则反而制造"配置说开、systemd 说关"的分叉。
    // systemctl / 进程探测是阻塞调用，放后台线程；事件循环闭包只碰 UI（只捕获 Send 的
    // Weak/bool/String，不能捕获 Rc<ConfigWriter>）。
    let weak = ui.as_weak();
    let busy_auto = Arc::new(AtomicBool::new(false));
    ui.on_autostart_changed(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if busy_auto.swap(true, Ordering::SeqCst) {
            ui.set_status_text(SharedString::from("开机自启正在生效，请稍候"));
            return;
        }
        let want = ui.get_autostart();
        let en = ui.get_lang_en();
        let weak = ui.as_weak();
        let busy = Arc::clone(&busy_auto);
        std::thread::spawn(move || {
            let result = autostart::set_enabled(want);
            busy.store(false, Ordering::SeqCst);
            let _ = slint::invoke_from_event_loop(move || {
                let Some(ui) = weak.upgrade() else {
                    return;
                };
                match result {
                    Ok(()) => {
                        let s = i18n::strings(if en { Lang::En } else { Lang::Zh });
                        ui.set_status_text(SharedString::from(s.status_saved));
                        ui.set_status_error(false);
                    }
                    Err(err) => {
                        ui.set_autostart(!want); // 回滚：界面不能显示未生效的状态
                        ui.set_status_text(SharedString::from(format!("开机自启设置失败：{err}")));
                        ui.set_status_error(true);
                    }
                }
            });
        });
    });

    let weak = ui.as_weak();
    let busy_tray = Arc::new(AtomicBool::new(false));
    ui.on_tray_changed(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if busy_tray.swap(true, Ordering::SeqCst) {
            ui.set_status_text(SharedString::from("托盘开关正在生效，请稍候"));
            return;
        }
        let want = ui.get_tray_enabled();
        let en = ui.get_lang_en();
        let weak = ui.as_weak();
        let busy = Arc::clone(&busy_tray);
        std::thread::spawn(move || {
            let result = trayctl::set_enabled(want);
            busy.store(false, Ordering::SeqCst);
            let _ = slint::invoke_from_event_loop(move || {
                let Some(ui) = weak.upgrade() else {
                    return;
                };
                match result {
                    Ok(()) => {
                        let s = i18n::strings(if en { Lang::En } else { Lang::Zh });
                        ui.set_status_text(SharedString::from(s.status_saved));
                        ui.set_status_error(false);
                    }
                    Err(err) => {
                        ui.set_tray_enabled(!want);
                        ui.set_status_text(SharedString::from(format!("托盘开关设置失败：{err}")));
                        ui.set_status_error(true);
                    }
                }
            });
        });
    });

    let weak = ui.as_weak();
    let state_sel = Rc::clone(&state);
    let base_sel = Rc::clone(&base);
    let writer_sel = Rc::clone(&writer);
    let suppress_sel = Arc::clone(&suppress);
    let preview_sel = Rc::clone(&preview);
    ui.on_preset_selected(move |picked| {
        if suppress_sel.load(Ordering::SeqCst) {
            return;
        }
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let id = picked.to_string();
        let mut cfg = base_sel.borrow().clone();
        if let Err(err) = presets::apply_preset(&mut cfg, &id) {
            ui.set_status_text(SharedString::from(err));
            ui.set_status_error(true);
            return;
        }
        suppress_sel.store(true, Ordering::SeqCst);
        fill(&ui, &cfg);
        suppress_sel.store(false, Ordering::SeqCst);
        *base_sel.borrow_mut() = cfg.clone();
        writer_sel.submit(cfg.clone());
        {
            let mut st = state_sel.borrow_mut();
            st.current_id = Some(id.clone());
            st.custom = false;
        }
        refresh_cards(&ui, &state_sel);
        refresh_users(&ui, &state_sel);
        update_now(&ui, &state_sel, &cfg);
        render_preview(&ui, &cfg, &preview_sel, 1.0, 1);
        let s = i18n::strings(state_sel.borrow().lang);
        ui.set_status_text(SharedString::from(s.status_saved));
        ui.set_status_error(false);
        if let Some(p) = presets::by_id(&id) {
            let tag = if state_sel.borrow().lang == Lang::En {
                p.en
            } else {
                p.tagline
            };
            ui.set_preset_status(SharedString::from(tag));
        }
    });

    let weak = ui.as_weak();
    let base_save = Rc::clone(&base);
    let state_save = Rc::clone(&state);
    ui.on_save_preset(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let name = ui.get_save_name().to_string();
        match UserPresetStore::open().save(&name, &base_save.borrow()) {
            Ok(saved) => {
                ui.set_save_name(SharedString::from(""));
                state_save.borrow_mut().current_id = Some(saved.id);
                refresh_users(&ui, &state_save);
                ui.set_status_text(SharedString::from(
                    i18n::strings(state_save.borrow().lang).status_saved,
                ));
                ui.set_status_error(false);
            }
            Err(err) => {
                ui.set_status_text(SharedString::from(err));
                ui.set_status_error(true);
            }
        }
    });

    let weak = ui.as_weak();
    let base_use = Rc::clone(&base);
    let writer_use = Rc::clone(&writer);
    let suppress_use = Arc::clone(&suppress);
    let state_use = Rc::clone(&state);
    let preview_use = Rc::clone(&preview);
    ui.on_user_use(move |id| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let id = id.to_string();
        let Some(item) = UserPresetStore::open().get(&id) else {
            ui.set_status_text(SharedString::from(format!("方案 {id} 不存在")));
            ui.set_status_error(true);
            return;
        };
        let mut cfg = base_use.borrow().clone();
        item.apply_to(&mut cfg);
        suppress_use.store(true, Ordering::SeqCst);
        fill(&ui, &cfg);
        suppress_use.store(false, Ordering::SeqCst);
        *base_use.borrow_mut() = cfg.clone();
        writer_use.submit(cfg.clone());
        {
            let mut st = state_use.borrow_mut();
            st.current_id = Some(id);
            st.custom = false;
        }
        refresh_cards(&ui, &state_use);
        refresh_users(&ui, &state_use);
        update_now(&ui, &state_use, &cfg);
        render_preview(&ui, &cfg, &preview_use, 1.0, 1);
    });

    let weak = ui.as_weak();
    let state_del = Rc::clone(&state);
    ui.on_user_delete(move |id| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let id = id.to_string();
        match UserPresetStore::open().delete(&id) {
            Ok(()) => {
                let is_current = state_del.borrow().current_id.as_deref() == Some(id.as_str());
                if is_current {
                    state_del.borrow_mut().current_id = None;
                }
                refresh_users(&ui, &state_del);
            }
            Err(err) => {
                ui.set_status_text(SharedString::from(err));
                ui.set_status_error(true);
            }
        }
    });

    let weak = ui.as_weak();
    let base_prev = Rc::clone(&base);
    let pv_prev = Rc::clone(&preview);
    let repaint = move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let speed = if ui.get_preview_slow() { 0.35 } else { 1.0 };
        render_preview(&ui, &base_prev.borrow(), &pv_prev, speed, 1);
    };
    ui.on_dock_bg_changed(repaint.clone());
    ui.on_preview_toggle(repaint.clone());
    ui.on_slow_toggle(repaint);

    let weak = ui.as_weak();
    let base_timer = Rc::clone(&base);
    let pv_timer = Rc::clone(&preview);
    let preview_timer = Timer::default();
    preview_timer.start(TimerMode::Repeated, Duration::from_millis(200), move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if !ui.get_preview_playing() {
            return;
        }
        let speed = if ui.get_preview_slow() { 0.35 } else { 1.0 };
        render_preview(&ui, &base_timer.borrow(), &pv_timer, speed, 9);
    });

    let timer = Timer::default();
    {
        let suppress_timer = Arc::clone(&suppress);
        timer.start(
            TimerMode::SingleShot,
            Duration::from_millis(400),
            move || suppress_timer.store(false, Ordering::SeqCst),
        );
    }

    ui.run().map_err(|e| format!("窗口运行失败：{e}"))?;
    writer.flush();
    let _ = (timer, preview_timer);
    Ok(())
}

/// 把配置灌进界面。
fn fill(ui: &Gui, cfg: &Config) {
    ui.set_enabled(cfg.general.enabled);
    ui.set_hide_on_fullscreen(cfg.behavior.hide_on_fullscreen);
    ui.set_display_mode(i32::from(cfg.visual.mode == VisualMode::AboveDock));
    ui.set_opacity_value(cfg.visual.opacity);
    ui.set_bands_value(cfg.visual.bands as f32);
    ui.set_gap_value(cfg.visual.bar_gap_ratio);
    ui.set_color_start(SharedString::from(cfg.visual.color_start.clone()));
    ui.set_color_end(SharedString::from(cfg.visual.color_end.clone()));
    ui.set_preview_start(color_of(&cfg.visual.color_start));
    ui.set_preview_end(color_of(&cfg.visual.color_end));
    // ★ 多色标必须回填：否则换预置后 collect() 会用界面旧值覆盖，静默失效。
    ui.set_palette_value(SharedString::from(cfg.visual.palette.clone()));
    ui.set_palette_mode_index(palette_mode_index(&cfg.visual.palette_mode));
    refresh_palette_ui(ui, cfg);
    ui.set_sensitivity_value(cfg.analyze.sensitivity);
    ui.set_attack_value(cfg.analyze.attack);
    ui.set_release_value(cfg.analyze.release);
    ui.set_db_floor_value(cfg.analyze.db_floor);
    ui.set_fft_index_value(fft_index(cfg.analyze.fft_size) as f32);
    ui.set_silence_dbfs_value(cfg.behavior.silence_dbfs);
    ui.set_silence_timeout_value(cfg.behavior.silence_timeout_ms as f32);

    ui.set_opacity_text(SharedString::from(format!(
        "{:.0}%",
        cfg.visual.opacity * 100.0
    )));
    ui.set_bands_text(SharedString::from(cfg.visual.bands.to_string()));
    ui.set_gap_text(SharedString::from(format!(
        "{:.2}",
        cfg.visual.bar_gap_ratio
    )));
    ui.set_sensitivity_text(SharedString::from(format!(
        "{:.2}",
        cfg.analyze.sensitivity
    )));
    ui.set_attack_text(SharedString::from(format!("{:.2}", cfg.analyze.attack)));
    ui.set_release_text(SharedString::from(format!("{:.2}", cfg.analyze.release)));
    ui.set_db_floor_text(SharedString::from(format!(
        "{:.0} dB",
        cfg.analyze.db_floor
    )));
    ui.set_fft_text(SharedString::from(cfg.analyze.fft_size.to_string()));
    ui.set_silence_text(SharedString::from(format!(
        "{:.0} dB",
        cfg.behavior.silence_dbfs
    )));
    ui.set_timeout_text(SharedString::from(format!(
        "{} ms",
        cfg.behavior.silence_timeout_ms
    )));
    // P1：峰值滑块
    ui.set_peak_cap(cfg.visual.peak_cap);
    ui.set_peak_hold_value(cfg.analyze.peak_hold_ms as f32);
    ui.set_peak_fall_value(cfg.analyze.peak_fall);
    ui.set_peak_gravity(cfg.analyze.peak_gravity);
    ui.set_peak_hold_text(SharedString::from(format!(
        "{} ms",
        cfg.analyze.peak_hold_ms
    )));
    ui.set_peak_fall_text(SharedString::from(format!("{:.3}", cfg.analyze.peak_fall)));
}

/// 从界面收集参数（写到 `cfg` 的对应字段上）。
///
/// `display_mode_dirty` 为 false 时保留 `cfg.visual.mode` 原值：
/// LeftBlank/RightBlank 未被 GUI 支持，改其他控件不能把它们静默覆盖掉。
fn collect(ui: &Gui, cfg: &mut Config, display_mode_dirty: bool) {
    cfg.general.enabled = ui.get_enabled();
    cfg.behavior.hide_on_fullscreen = ui.get_hide_on_fullscreen();
    if display_mode_dirty {
        cfg.visual.mode = if ui.get_display_mode() == 1 {
            VisualMode::AboveDock
        } else {
            VisualMode::FullDock
        };
    }
    cfg.visual.opacity = ui.get_opacity_value();
    cfg.visual.bands = ui.get_bands_value().round() as usize;
    cfg.visual.bar_gap_ratio = ui.get_gap_value();
    cfg.visual.color_start = ui.get_color_start().to_string();
    cfg.visual.color_end = ui.get_color_end().to_string();
    cfg.visual.palette = ui.get_palette_value().to_string();
    let mode_idx = ui.get_palette_mode_index().clamp(0, 3) as usize;
    PALETTE_MODES[mode_idx].clone_into(&mut cfg.visual.palette_mode);
    cfg.analyze.sensitivity = ui.get_sensitivity_value();
    cfg.analyze.attack = ui.get_attack_value();
    cfg.analyze.release = ui.get_release_value();
    cfg.analyze.db_floor = ui.get_db_floor_value();
    cfg.analyze.fft_size = fft_from_index(ui.get_fft_index_value().round() as i32);
    cfg.behavior.silence_dbfs = ui.get_silence_dbfs_value();
    cfg.behavior.silence_timeout_ms = ui.get_silence_timeout_value().round() as u64;
    // ★ R3：这四个字段必须写回，否则预置/我的方案里的峰值滑块参数会被旧值覆盖，
    //   界面显示改了、实际没生效 —— 正是本项目零容忍的「静默失效」。
    cfg.visual.peak_cap = ui.get_peak_cap();
    cfg.analyze.peak_hold_ms = ui.get_peak_hold_value().round() as u32;
    cfg.analyze.peak_fall = ui.get_peak_fall_value();
    cfg.analyze.peak_gravity = ui.get_peak_gravity();
}

/// 把被校验改过的值同步回界面（只改**确实变过**的字段）。
///
/// 例如用户把颜色填成 `#GGG` → 校验回退默认色 → 界面必须显示回退后的值，
/// 否则用户会以为"程序偷偷改了我的配置"。
fn sync_back(ui: &Gui, before: &Config, after: &Config) {
    if before.visual.opacity != after.visual.opacity {
        ui.set_opacity_value(after.visual.opacity);
        ui.set_opacity_text(SharedString::from(format!(
            "{:.0}%",
            after.visual.opacity * 100.0
        )));
    }
    if before.visual.bands != after.visual.bands {
        ui.set_bands_value(after.visual.bands as f32);
        ui.set_bands_text(SharedString::from(after.visual.bands.to_string()));
    }
    if before.visual.bar_gap_ratio != after.visual.bar_gap_ratio {
        ui.set_gap_value(after.visual.bar_gap_ratio);
        ui.set_gap_text(SharedString::from(format!(
            "{:.2}",
            after.visual.bar_gap_ratio
        )));
    }
    if before.visual.color_start != after.visual.color_start {
        ui.set_color_start(SharedString::from(after.visual.color_start.clone()));
        ui.set_preview_start(color_of(&after.visual.color_start));
    }
    if before.visual.color_end != after.visual.color_end {
        ui.set_color_end(SharedString::from(after.visual.color_end.clone()));
        ui.set_preview_end(color_of(&after.visual.color_end));
    }
    if before.visual.palette != after.visual.palette {
        ui.set_palette_value(SharedString::from(after.visual.palette.clone()));
        refresh_palette_ui(ui, after);
    }
    if before.visual.palette_mode != after.visual.palette_mode {
        ui.set_palette_mode_index(palette_mode_index(&after.visual.palette_mode));
    }
    if before.analyze.sensitivity != after.analyze.sensitivity {
        ui.set_sensitivity_value(after.analyze.sensitivity);
        ui.set_sensitivity_text(SharedString::from(format!(
            "{:.2}",
            after.analyze.sensitivity
        )));
    }
    if before.analyze.attack != after.analyze.attack {
        ui.set_attack_value(after.analyze.attack);
        ui.set_attack_text(SharedString::from(format!("{:.2}", after.analyze.attack)));
    }
    if before.analyze.release != after.analyze.release {
        ui.set_release_value(after.analyze.release);
        ui.set_release_text(SharedString::from(format!("{:.2}", after.analyze.release)));
    }
    if before.analyze.db_floor != after.analyze.db_floor {
        ui.set_db_floor_value(after.analyze.db_floor);
        ui.set_db_floor_text(SharedString::from(format!(
            "{:.0} dB",
            after.analyze.db_floor
        )));
    }
    if before.analyze.fft_size != after.analyze.fft_size {
        ui.set_fft_index_value(fft_index(after.analyze.fft_size) as f32);
        ui.set_fft_text(SharedString::from(after.analyze.fft_size.to_string()));
    }
    if before.behavior.silence_dbfs != after.behavior.silence_dbfs {
        ui.set_silence_dbfs_value(after.behavior.silence_dbfs);
        ui.set_silence_text(SharedString::from(format!(
            "{:.0} dB",
            after.behavior.silence_dbfs
        )));
    }
    if before.behavior.hide_on_fullscreen != after.behavior.hide_on_fullscreen {
        ui.set_hide_on_fullscreen(after.behavior.hide_on_fullscreen);
    }
    if before.behavior.silence_timeout_ms != after.behavior.silence_timeout_ms {
        ui.set_silence_timeout_value(after.behavior.silence_timeout_ms as f32);
        ui.set_timeout_text(SharedString::from(format!(
            "{} ms",
            after.behavior.silence_timeout_ms
        )));
    }
    if before.visual.peak_cap != after.visual.peak_cap {
        ui.set_peak_cap(after.visual.peak_cap);
    }
    if before.analyze.peak_hold_ms != after.analyze.peak_hold_ms {
        ui.set_peak_hold_value(after.analyze.peak_hold_ms as f32);
        ui.set_peak_hold_text(SharedString::from(format!(
            "{} ms",
            after.analyze.peak_hold_ms
        )));
    }
    if before.analyze.peak_fall != after.analyze.peak_fall {
        ui.set_peak_fall_value(after.analyze.peak_fall);
        ui.set_peak_fall_text(SharedString::from(format!(
            "{:.3}",
            after.analyze.peak_fall
        )));
    }
    if before.analyze.peak_gravity != after.analyze.peak_gravity {
        ui.set_peak_gravity(after.analyze.peak_gravity);
    }
}

/// `#RRGGBB` → Slint 颜色；非法值回退到默认青。
fn color_of(hex: &str) -> slint::Color {
    let bytes = hex.as_bytes();
    if bytes.len() == 7
        && bytes[0] == b'#'
        && let Ok(v) = u32::from_str_radix(&hex[1..], 16)
    {
        return slint::Color::from_rgb_u8(
            ((v >> 16) & 0xFF) as u8,
            ((v >> 8) & 0xFF) as u8,
            (v & 0xFF) as u8,
        );
    }
    slint::Color::from_rgb_u8(0x00, 0xC8, 0xFF)
}

/// FFT 点数 → 滑块下标。
fn fft_index(fft_size: usize) -> usize {
    FFT_SIZES.iter().position(|s| *s == fft_size).unwrap_or(2)
}

/// 滑块下标 → FFT 点数。
fn fft_from_index(index: i32) -> usize {
    let idx = usize::try_from(index).unwrap_or(2);
    FFT_SIZES.get(idx).copied().unwrap_or(4096)
}

/// 设置界面单实例锁。
///
/// 用 Unix socket 占位：`AddrInUse` 时再 `connect` 一次区分「真有人在跑」
/// 和「上次崩溃留下的僵尸文件」。
struct GuiLock {
    path: PathBuf,
    /// 只为持有监听；Drop 时自动关闭。
    _listener: std::os::unix::net::UnixListener,
}

impl GuiLock {
    fn acquire() -> Result<Self, String> {
        use std::os::unix::net::{UnixListener, UnixStream};

        let path = gui_lock_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("创建设置界面锁目录失败：{e}"))?;
        }
        let listener = match UnixListener::bind(&path) {
            Ok(listener) => listener,
            Err(err) if err.kind() == std::io::ErrorKind::AddrInUse => {
                if UnixStream::connect(&path).is_ok() {
                    return Err("设置界面已在运行".to_owned());
                }
                let _ = std::fs::remove_file(&path);
                UnixListener::bind(&path).map_err(|e| format!("重建设置界面锁失败：{e}"))?
            }
            Err(err) => return Err(format!("创建设置界面锁失败：{err}")),
        };
        Ok(Self {
            path,
            _listener: listener,
        })
    }
}

impl Drop for GuiLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn gui_lock_path() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .map(|home| PathBuf::from(home).join(".cache").join("deepin-spectrum"))
        })
        .unwrap_or_else(std::env::temp_dir)
        .join("deepin-spectrum-settings.lock")
}

#[cfg(test)]
mod tests {
    use super::{color_of, fft_from_index, fft_index};

    #[test]
    fn hex_to_color() {
        let c = color_of("#FF8000");
        // Slint 1.18 的 Color::red()/green()/blue() 返回 u8（不是 f32）
        assert_eq!((c.red(), c.green(), c.blue()), (255, 128, 0));
    }

    #[test]
    fn invalid_hex_falls_back() {
        let fallback = color_of("#00C8FF");
        assert_eq!(color_of("nonsense"), fallback);
        assert_eq!(color_of("#FFF"), fallback);
        assert_eq!(color_of(""), fallback);
    }

    #[test]
    fn fft_index_round_trips() {
        for size in [1024usize, 2048, 4096, 8192] {
            let idx = i32::try_from(fft_index(size)).unwrap_or(2);
            assert_eq!(fft_from_index(idx), size);
        }
        // 非法下标落到默认档
        assert_eq!(fft_from_index(99), 4096);
        assert_eq!(fft_from_index(-1), 4096);
        // 非标准点数落到 4096 档
        assert_eq!(fft_index(3000), 2);
    }
}
