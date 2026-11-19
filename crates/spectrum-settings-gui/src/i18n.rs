//! 界面文案与语言偏好。
//!
//! 刻意**不用** Slint 的 `@tr!` / bundled translation：那需要编译期 `.pot` 与
//! build.rs 配置，运行时切换还得重建 bundle。这里用一张静态字符串表 + 一个
//! `in-out property <Strings>`，Rust 侧切换时一次性回填，零外部依赖、可单测、
//! 且语言偏好能落到独立文件（不碰主配置、不动 `CONFIG_VERSION`）。

use std::path::PathBuf;

use spectrum_settings::store;
use spectrum_settings::toml::parse;

/// 界面语言。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Lang {
    /// 简体中文（默认）。
    #[default]
    Zh,
    /// English.
    En,
}

impl Lang {
    /// 持久化用的短代码。
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Zh => "zh",
            Self::En => "en",
        }
    }

    /// 从短代码解析；未知值一律回退中文，不猜测用户意图。
    #[must_use]
    pub fn from_code(s: &str) -> Self {
        match s {
            "en" => Self::En,
            _ => Self::Zh,
        }
    }

    /// 切换到另一种语言。
    #[must_use]
    pub const fn other(self) -> Self {
        match self {
            Self::Zh => Self::En,
            Self::En => Self::Zh,
        }
    }
}

/// 界面上所有可翻译文案。
///
/// Slint 侧是 `in-out property <Strings> s`，字段名一一对应（`s.nav_presets` 等）。
/// 字段注释统一省略：它们本身就是可读的英文标识，逐条写 `///` 只会制造噪音。
#[allow(missing_docs)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Strings {
    pub app_name: &'static str,
    pub contents: &'static str,
    pub mast_title: &'static str,
    pub custom_label: &'static str,
    pub nav_presets: &'static str,
    pub nav_tune: &'static str,
    pub nav_motion: &'static str,
    pub nav_runtime: &'static str,
    pub mast_subtitle: &'static str,
    pub theme_paper: &'static str,
    pub theme_ink: &'static str,
    pub stage_kicker: &'static str,
    pub dock_dark: &'static str,
    pub dock_light: &'static str,
    pub slow_on: &'static str,
    pub slow_off: &'static str,
    pub play: &'static str,
    pub pause: &'static str,
    pub presets_title: &'static str,
    pub presets_desc: &'static str,
    pub group_hint: &'static str,
    pub save_bar_title: &'static str,
    pub save_bar_desc: &'static str,
    pub palette_title: &'static str,
    pub palette_desc: &'static str,
    pub palette_mode: &'static str,
    pub palette_stops: &'static str,
    pub pm_off: &'static str,
    pub pm_across: &'static str,
    pub pm_per_bar: &'static str,
    pub pm_vertical: &'static str,
    pub runtime_mode_label: &'static str,
    pub mode_full: &'static str,
    pub mode_above: &'static str,
    pub mine_title: &'static str,
    pub mine_desc: &'static str,
    pub save_hint: &'static str,
    pub save_btn: &'static str,
    pub delete_btn: &'static str,
    pub use_btn: &'static str,
    pub now_label: &'static str,
    pub best_dark: &'static str,
    pub best_light: &'static str,
    pub best_both: &'static str,
    pub tune_title: &'static str,
    pub opacity: &'static str,
    pub bands: &'static str,
    pub gap: &'static str,
    pub color_start: &'static str,
    pub color_end: &'static str,
    pub fx_title: &'static str,
    pub fx_desc: &'static str,
    pub fx_enable: &'static str,
    pub fx_hold: &'static str,
    pub fx_fall: &'static str,
    pub fx_gravity: &'static str,
    pub motion_title: &'static str,
    pub sensitivity: &'static str,
    pub attack: &'static str,
    pub release: &'static str,
    pub db_floor: &'static str,
    pub fft: &'static str,
    pub runtime_title: &'static str,
    pub start: &'static str,
    pub stop: &'static str,
    pub autostart: &'static str,
    pub tray: &'static str,
    pub fullscreen: &'static str,
    pub silence_dbfs: &'static str,
    pub silence_timeout: &'static str,
    pub footer_reset: &'static str,
    pub footer_hint: &'static str,
    pub status_saved: &'static str,
    pub status_applied: &'static str,
    pub status_custom: &'static str,
    pub lang_label: &'static str,
    pub nav_about: &'static str,
    pub about_title: &'static str,
    pub about_kicker: &'static str,
    pub about_tagline: &'static str,
    pub about_author: &'static str,
    pub about_email: &'static str,
    pub about_license: &'static str,
    pub about_version: &'static str,
    pub about_stack: &'static str,
    pub about_license_note: &'static str,
    pub about_license_file: &'static str,
    pub about_thanks: &'static str,
}

/// 简体中文文案。
pub const ZH: Strings = Strings {
    app_name: "频谱工坊",
    contents: "目录",
    mast_title: "频谱",
    custom_label: "自定义",
    nav_presets: "预置风格",
    nav_tune: "微调",
    nav_motion: "动态",
    nav_runtime: "运行",
    mast_subtitle: "让任务栏跟着声音呼吸。选一套风格，或者把它拧成你自己的样子。",
    theme_paper: "纸白",
    theme_ink: "墨黑",
    stage_kicker: "动画预览 · FullDock",
    dock_dark: "深色任务栏",
    dock_light: "浅色任务栏",
    slow_on: "慢放 0.35×",
    slow_off: "恢复正常速度",
    play: "播放动画",
    pause: "暂停动画",
    presets_title: "预置风格",
    presets_desc: "30 套方案按情绪分成 6 类：欢快、平静、温暖、忧伤、严肃、炫酷。每套都是一组已实现且热生效的参数，点一下就整套换掉，不需要理解任何滑块。",
    group_hint: "按情绪分成 6 类，每类 5 套。点一张色卡，配色、动态和峰值滑块会整套套用。",
    save_bar_title: "存成我的方案",
    save_bar_desc: "调好了？给这套参数起个名字，配色、柱数、动态和峰值滑块会一起存下来。以后在「01 预置风格」底部随时选回。",
    palette_title: "多色标",
    palette_desc: "带多组色标的方案可以换铺色方式：横向彩虹渐变、每柱一色，或柱内多段。",
    palette_mode: "铺色方式",
    palette_stops: "当前色标",
    pm_off: "双色渐变（关闭多色）",
    pm_across: "横向彩虹渐变",
    pm_per_bar: "每柱一色",
    pm_vertical: "柱内多段",
    runtime_mode_label: "显示位置",
    mode_full: "铺满任务栏",
    mode_above: "浮在任务栏上方",
    mine_title: "我的方案",
    mine_desc: "在「02 微调」或「03 动态」调好后命名保存，整套参数就会留在这里，随时选用或删除。同名保存会更新。",
    save_hint: "给方案起个名字",
    save_btn: "命名保存",
    delete_btn: "删除",
    use_btn: "选用",
    now_label: "当前",
    best_dark: "深底更佳",
    best_light: "浅底更佳",
    best_both: "深浅皆宜",
    tune_title: "微调",
    opacity: "不透明度",
    bands: "柱数",
    gap: "柱间隙",
    color_start: "柱底颜色",
    color_end: "柱顶颜色",
    fx_title: "峰值滑块 · 悬浮缓落",
    fx_desc: "声音一冲上来，柱顶的小滑块被顶飞，短暂悬停后与柱体分离、缓慢落回。",
    fx_enable: "启用峰值滑块",
    fx_hold: "悬停时长",
    fx_fall: "回落速度",
    fx_gravity: "重力加速（越落越快）",
    motion_title: "动态",
    sensitivity: "灵敏度",
    attack: "起音",
    release: "衰减",
    db_floor: "底噪门限",
    fft: "FFT 档位",
    runtime_title: "运行",
    start: "启动频谱",
    stop: "停止频谱",
    autostart: "登录时自动启动频谱",
    tray: "在系统托盘显示控制图标",
    fullscreen: "全屏播放视频时隐藏频谱",
    silence_dbfs: "静音门限",
    silence_timeout: "静音超时",
    footer_reset: "恢复默认",
    footer_hint: "改动自动保存并立即生效（≤250ms），不需要重启频谱。界面上不出现本版本未实现的参数。",
    status_saved: "已保存 · 立即生效",
    status_applied: "已应用「",
    status_custom: "自定义参数 · 已保存 · 立即生效",
    lang_label: "语言",
    nav_about: "关于",
    about_title: "关于本软件",
    about_kicker: "关于 / 致谢",
    about_tagline: "让任务栏跟着声音呼吸，不改动系统外壳一行代码。",
    about_author: "作者",
    about_email: "邮箱",
    about_license: "开源协议",
    about_version: "版本",
    about_stack: "技术",
    about_license_note: "本软件按 GNU 通用公共许可证第 3 版（GPL-3.0-only）发布：可以自由使用、修改和再分发，衍生作品须以同一协议开源。因设置界面使用 Slint，本项目不提供「或更新版本」选项。",
    about_license_file: "许可证全文见项目根目录 LICENSE，安装后位于 /usr/share/doc/deepin-spectrum/。",
    about_thanks: "感谢 Rust、Slint、PulseAudio 与 Deepin 社区。",
};

/// English copy.
pub const EN: Strings = Strings {
    app_name: "Spectrum Atelier",
    contents: "Contents",
    mast_title: "SPECTRUM",
    custom_label: "Custom",
    nav_presets: "Presets",
    nav_tune: "Fine tune",
    nav_motion: "Motion",
    nav_runtime: "Runtime",
    mast_subtitle: "Let the dock breathe with your audio. Pick a style, or make it your own.",
    theme_paper: "Paper",
    theme_ink: "Ink",
    stage_kicker: "Animated preview · FullDock",
    dock_dark: "Dark dock",
    dock_light: "Light dock",
    slow_on: "Slow 0.35x",
    slow_off: "Normal speed",
    play: "Play",
    pause: "Pause",
    presets_title: "Preset library",
    presets_desc: "30 presets in six mood families: cheerful, calm, warm, melancholy, serious, radical. Each is a set of live-reloadable parameters; one click applies the whole look.",
    group_hint: "Six mood families, five presets each. Click a card to apply colors, motion and peak slider together.",
    save_bar_title: "Save as my preset",
    save_bar_desc: "Happy with it? Give these settings a name; colors, bars, motion and the peak slider are saved together. Find them at the bottom of 01 Presets.",
    palette_title: "Multi-color palette",
    palette_desc: "Presets with color stops can be laid out as a horizontal rainbow, one color per bar, or vertical segments.",
    palette_mode: "Palette mode",
    palette_stops: "Color stops",
    pm_off: "Two-color gradient (off)",
    pm_across: "Horizontal rainbow",
    pm_per_bar: "One color per bar",
    pm_vertical: "Vertical segments",
    runtime_mode_label: "Display mode",
    mode_full: "Full dock",
    mode_above: "Above dock",
    mine_title: "My presets",
    mine_desc: "Named after tuning in 02 Fine tune or 03 Motion. Saved presets live here for reuse or deletion; saving the same name updates it.",
    save_hint: "Name this preset",
    save_btn: "Save",
    delete_btn: "Delete",
    use_btn: "Use",
    now_label: "Now",
    best_dark: "Best on dark dock",
    best_light: "Best on light dock",
    best_both: "Any dock",
    tune_title: "Fine tune",
    opacity: "Opacity",
    bands: "Bars",
    gap: "Bar gap",
    color_start: "Bottom color",
    color_end: "Top color",
    fx_title: "Peak slider · float & fall",
    fx_desc: "A hit kicks the little slider above the bar; it hovers, separates from the bar, then slowly falls back.",
    fx_enable: "Enable peak slider",
    fx_hold: "Hold time",
    fx_fall: "Fall speed",
    fx_gravity: "Gravity (accelerating fall)",
    motion_title: "Motion",
    sensitivity: "Sensitivity",
    attack: "Attack",
    release: "Release",
    db_floor: "Noise floor",
    fft: "FFT size",
    runtime_title: "Runtime",
    start: "Start",
    stop: "Stop",
    autostart: "Start spectrum on login",
    tray: "Show tray control icon",
    fullscreen: "Hide spectrum in fullscreen",
    silence_dbfs: "Silence threshold",
    silence_timeout: "Silence timeout",
    footer_reset: "Reset defaults",
    footer_hint: "Changes save and apply within 250ms; no restart needed. Unimplemented options never appear here.",
    status_saved: "Saved · applied live",
    status_applied: "Applied ",
    status_custom: "Custom parameters · saved · applied live",
    lang_label: "Language",
    nav_about: "About",
    about_title: "About this app",
    about_kicker: "ABOUT / CREDITS",
    about_tagline: "Let the dock breathe with your audio - without patching the shell.",
    about_author: "Author",
    about_email: "Email",
    about_license: "License",
    about_version: "Version",
    about_stack: "Built with",
    about_license_note: "Released under the GNU General Public License version 3 (GPL-3.0-only). You may use, modify and redistribute it, but derivatives must stay under the same license. Because the settings UI uses Slint, no 'or later' option is offered.",
    about_license_file: "Full text: LICENSE in the source tree, or /usr/share/doc/deepin-spectrum/ after install.",
    about_thanks: "Thanks to the Rust, Slint, PulseAudio and Deepin communities.",
};

/// 取某个语言的文案表。
#[must_use]
pub const fn strings(lang: Lang) -> &'static Strings {
    match lang {
        Lang::Zh => &ZH,
        Lang::En => &EN,
    }
}

/// 语言偏好的持久化（独立文件，不写进 `config.toml`）。
#[derive(Debug, Clone)]
pub struct UiPrefs {
    path: PathBuf,
}

impl UiPrefs {
    /// 默认路径：`~/.config/deepin-spectrum/ui.toml`。
    #[must_use]
    pub fn open() -> Self {
        let path = store::default_path()
            .parent()
            .map_or_else(|| PathBuf::from("."), PathBuf::from)
            .join("ui.toml");
        Self { path }
    }

    /// 指定路径（测试用）。
    #[must_use]
    pub const fn with_path(path: PathBuf) -> Self {
        Self { path }
    }

    /// 读取语言；文件不存在 / 损坏 / 值未知时回退中文。
    #[must_use]
    pub fn load(&self) -> Lang {
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return Lang::Zh;
        };
        let Ok(doc) = parse(&text) else {
            return Lang::Zh;
        };
        doc.entries()
            .iter()
            .find(|e| e.section.is_none() && e.key == "lang")
            .and_then(|e| e.value.as_str())
            .map_or(Lang::Zh, Lang::from_code)
    }

    /// 原子写入语言偏好。
    ///
    /// # Errors
    ///
    /// 目录创建或写盘失败。
    pub fn save(&self, lang: Lang) -> Result<(), String> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("创建偏好目录失败：{e}"))?;
        }
        let text = format!(
            "# deepin-spectrum UI language\nlang = \"{}\"\n",
            lang.code()
        );
        store::write_atomic_text(&self.path, &text).map_err(|e| format!("写入语言偏好失败：{e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::{EN, Lang, Strings, UiPrefs, ZH, strings};

    /// 两套文案的字段数必须一致：用一个字段列表做结构性比对。
    fn fields(s: &Strings) -> Vec<&'static str> {
        vec![
            s.app_name,
            s.contents,
            s.mast_title,
            s.custom_label,
            s.nav_presets,
            s.nav_tune,
            s.nav_motion,
            s.nav_runtime,
            s.mast_subtitle,
            s.theme_paper,
            s.theme_ink,
            s.stage_kicker,
            s.dock_dark,
            s.dock_light,
            s.slow_on,
            s.slow_off,
            s.play,
            s.pause,
            s.presets_title,
            s.presets_desc,
            s.group_hint,
            s.save_bar_title,
            s.save_bar_desc,
            s.palette_title,
            s.palette_desc,
            s.palette_mode,
            s.palette_stops,
            s.pm_off,
            s.pm_across,
            s.pm_per_bar,
            s.pm_vertical,
            s.runtime_mode_label,
            s.mode_full,
            s.mode_above,
            s.mine_title,
            s.mine_desc,
            s.save_hint,
            s.save_btn,
            s.delete_btn,
            s.use_btn,
            s.now_label,
            s.best_dark,
            s.best_light,
            s.best_both,
            s.tune_title,
            s.opacity,
            s.bands,
            s.gap,
            s.color_start,
            s.color_end,
            s.fx_title,
            s.fx_desc,
            s.fx_enable,
            s.fx_hold,
            s.fx_fall,
            s.fx_gravity,
            s.motion_title,
            s.sensitivity,
            s.attack,
            s.release,
            s.db_floor,
            s.fft,
            s.runtime_title,
            s.start,
            s.stop,
            s.autostart,
            s.tray,
            s.fullscreen,
            s.silence_dbfs,
            s.silence_timeout,
            s.footer_reset,
            s.footer_hint,
            s.status_saved,
            s.status_applied,
            s.status_custom,
            s.lang_label,
            s.nav_about,
            s.about_title,
            s.about_kicker,
            s.about_tagline,
            s.about_author,
            s.about_email,
            s.about_license,
            s.about_version,
            s.about_stack,
            s.about_license_note,
            s.about_license_file,
            s.about_thanks,
        ]
    }

    #[test]
    fn both_languages_have_same_shape_and_no_empty_text() {
        let zh = fields(&ZH);
        let en = fields(&EN);
        assert_eq!(zh.len(), en.len());
        assert!(zh.iter().all(|s| !s.is_empty()), "中文表有空文案");
        assert!(en.iter().all(|s| !s.is_empty()), "英文表有空文案");
        // 英文必须真的翻译过（只允许少量专有名词相同）
        let same = zh.iter().zip(&en).filter(|(a, b)| a == b).count();
        assert!(same <= 3, "英文表里还有 {same} 条未翻译");
    }

    #[test]
    fn lang_code_round_trips() {
        for lang in [Lang::Zh, Lang::En] {
            assert_eq!(Lang::from_code(lang.code()), lang);
            assert_ne!(lang.other(), lang);
        }
        assert_eq!(Lang::from_code("fr"), Lang::Zh, "未知语言回退中文");
        assert_eq!(strings(Lang::En).app_name, "Spectrum Atelier");
    }

    #[test]
    fn prefs_round_trip_and_bad_file_falls_back() {
        let dir = std::env::temp_dir().join(format!(
            "ds-i18n-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_millis())
        ));
        let path = dir.join("ui.toml");
        let prefs = UiPrefs::with_path(path.clone());
        assert_eq!(prefs.load(), Lang::Zh, "文件不存在应回退中文");
        prefs.save(Lang::En).expect("保存失败");
        assert_eq!(prefs.load(), Lang::En);
        std::fs::write(&path, "lang = [broken\n").expect("写坏文件");
        assert_eq!(prefs.load(), Lang::Zh, "坏文件应回退中文");
        let _ = std::fs::remove_dir_all(dir);
    }
}
