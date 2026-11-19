//! 频谱预置方案：20 套内置风格 + 用户命名保存的自定义方案。
//!
//! ## 为什么预置只是「参数组合」
//!
//! 每套预置都是 [`spectrum_core::config::Config`] 里**已实现且热生效**的一组取值，
//! 不是运行时状态。切换 = 一次性展开成已有键 → 落盘 → daemon 250ms 内热应用。
//! 因此不需要新增配置键、不需要改 `CONFIG_VERSION`、不需要重建窗口。
//!
//! 数据来源为 `docs/design/presets.json`，由 `docs/design/verify_design.py` 守门；
//! 本文件的常量表由该 JSON 生成，不要手工改数值。
//!
//! ## 用户方案为什么用独立文件
//!
//! `~/.config/deepin-spectrum/presets/<id>.toml`，每个方案一个文件。
//! 往主 `config.toml` 里塞方案列表会让「设置界面回写整个文件」擦掉用户手写的键，
//! 也会逼着 `CONFIG_VERSION` 升级。独立文件复用现有零依赖 [`crate::toml`] 解析器，
//! 段名固定、id 白名单化，天然避开 TOML 段名注入。

use std::path::PathBuf;

use spectrum_core::config::Config;

use crate::store;
use crate::toml::{parse, quote};

/// 一套内置预置的完整参数。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Preset {
    /// 稳定标识（配置与 CLI 用它引用）。
    pub id: &'static str,
    /// 中文名。
    pub name: &'static str,
    /// 英文名。
    pub en: &'static str,
    /// 风格族标识。
    pub family: &'static str,
    /// 一句话描述。
    pub tagline: &'static str,
    /// 最适配的任务栏底色：`dark` / `light` / `both`。
    pub best_on: &'static str,
    /// 柱底颜色 `#RRGGBB`。
    pub color_start: &'static str,
    /// 柱顶颜色 `#RRGGBB`。
    pub color_end: &'static str,
    /// 多色标色板；空字符串 = 双色渐变。
    pub palette: &'static str,
    /// 铺色方式 `off` / `across` / `per_bar` / `vertical`。
    pub palette_mode: &'static str,
    /// 整体不透明度。
    pub opacity: f32,
    /// 柱数。
    pub bands: usize,
    /// 柱间隙比例。
    pub bar_gap_ratio: f32,
    /// 灵敏度。
    pub sensitivity: f32,
    /// 起音系数。
    pub attack: f32,
    /// 衰减系数。
    pub release: f32,
    /// 底噪门限（dB）。
    pub db_floor: f32,
    /// FFT 点数。
    pub fft_size: usize,
    /// 是否绘制峰值滑块。
    pub peak_cap: bool,
    /// 峰值滑块悬停时长（毫秒）。
    pub peak_hold_ms: u32,
    /// 峰值滑块每帧回落量。
    pub peak_fall: f32,
    /// 是否使用重力加速缓落。
    pub peak_gravity: bool,
}

impl Preset {
    /// 把这套预置的**配色风格**展开到 `cfg` 上（调用方随后需执行 `cfg.validate()`）。
    ///
    /// 只覆盖视觉外观字段（配色、多色标、不透明度、柱数、间隙）。刻意**不动**峰值滑块
    /// 与分析参数（`peak_cap` / `peak_hold_ms` / `peak_fall` / `peak_gravity` /
    /// `sensitivity` / `attack` / `release` / `db_floor` / `fft_size`）：这些属于用户在
    /// 「02 微调」「03 动态」页的独立调整，切换配色风格不应把它们重置掉。
    /// `Preset` 结构里仍保留这些字段的完整取值，供 CLI / 数据核对使用。
    pub fn apply_to(&self, cfg: &mut Config) {
        self.color_start.clone_into(&mut cfg.visual.color_start);
        self.color_end.clone_into(&mut cfg.visual.color_end);
        self.palette.clone_into(&mut cfg.visual.palette);
        self.palette_mode.clone_into(&mut cfg.visual.palette_mode);
        cfg.visual.opacity = self.opacity;
        cfg.visual.bands = self.bands;
        cfg.visual.bar_gap_ratio = self.bar_gap_ratio;
    }
}

/// 全部内置预置（顺序与 `presets.json` 一致）。
pub const PRESETS: &[Preset] = &[
    Preset {
        id: "neon-run",
        name: "赛博霓虹",
        en: "NEON RUN",
        family: "cheer",
        tagline: "一撞就亮的品红青，节拍越密越热闹",
        best_on: "dark",
        color_start: "#FF2BD6",
        color_end: "#00E5FF",
        opacity: 0.85,
        bands: 80,
        bar_gap_ratio: 0.18,
        palette: "",
        palette_mode: "off",
        sensitivity: 1.25,
        attack: 0.78,
        release: 0.1,
        db_floor: -74.0,
        fft_size: 4096,
        peak_cap: true,
        peak_hold_ms: 420,
        peak_fall: 0.01,
        peak_gravity: true,
    },
    Preset {
        id: "lava-club",
        name: "熔岩电音",
        en: "LAVA CLUB",
        family: "cheer",
        tagline: "熔岩红滚进金黄，像舞池地面在发烫",
        best_on: "both",
        color_start: "#FF3B30",
        color_end: "#FFD60A",
        opacity: 0.9,
        bands: 72,
        bar_gap_ratio: 0.2,
        palette: "",
        palette_mode: "off",
        sensitivity: 1.35,
        attack: 0.85,
        release: 0.09,
        db_floor: -70.0,
        fft_size: 4096,
        peak_cap: true,
        peak_hold_ms: 420,
        peak_fall: 0.01,
        peak_gravity: true,
    },
    Preset {
        id: "rainbow-candy",
        name: "彩虹糖",
        en: "RAINBOW CANDY",
        family: "cheer",
        tagline: "五段色标沿整条频谱平滑铺开，从左到右一路甜到底",
        best_on: "dark",
        color_start: "#FF4D6D",
        color_end: "#7B61FF",
        opacity: 0.82,
        bands: 64,
        bar_gap_ratio: 0.2,
        palette: "#FF4D6D,#FFB703,#8AFF6B,#00E5FF,#7B61FF",
        palette_mode: "across",
        sensitivity: 1.05,
        attack: 0.62,
        release: 0.14,
        db_floor: -80.0,
        fft_size: 4096,
        peak_cap: true,
        peak_hold_ms: 420,
        peak_fall: 0.01,
        peak_gravity: true,
    },
    Preset {
        id: "confetti",
        name: "彩纸蹦迪",
        en: "CONFETTI",
        family: "cheer",
        tagline: "一根柱一个颜色，鼓点一落像撒了一把彩纸",
        best_on: "dark",
        color_start: "#FF2BD6",
        color_end: "#00E5FF",
        opacity: 0.88,
        bands: 48,
        bar_gap_ratio: 0.28,
        palette: "#FF2BD6,#00E5FF,#FFE66D,#7CFF6B,#FF7A45",
        palette_mode: "per_bar",
        sensitivity: 1.15,
        attack: 0.72,
        release: 0.18,
        db_floor: -80.0,
        fft_size: 4096,
        peak_cap: true,
        peak_hold_ms: 420,
        peak_fall: 0.01,
        peak_gravity: true,
    },
    Preset {
        id: "citrus-pop",
        name: "柑橘汽水",
        en: "CITRUS POP",
        family: "cheer",
        tagline: "柠檬黄冒泡到西柚粉，清爽不腻",
        best_on: "both",
        color_start: "#FFD166",
        color_end: "#FF6B6B",
        opacity: 0.78,
        bands: 56,
        bar_gap_ratio: 0.22,
        palette: "",
        palette_mode: "off",
        sensitivity: 1.0,
        attack: 0.6,
        release: 0.15,
        db_floor: -80.0,
        fft_size: 4096,
        peak_cap: true,
        peak_hold_ms: 420,
        peak_fall: 0.01,
        peak_gravity: true,
    },
    Preset {
        id: "deep-sea",
        name: "深海",
        en: "DEEP SEA",
        family: "calm",
        tagline: "深海底的一束青光，安静但一直在",
        best_on: "dark",
        color_start: "#0A4D68",
        color_end: "#05C3DD",
        opacity: 0.68,
        bands: 64,
        bar_gap_ratio: 0.28,
        palette: "",
        palette_mode: "off",
        sensitivity: 1.0,
        attack: 0.32,
        release: 0.3,
        db_floor: -86.0,
        fft_size: 8192,
        peak_cap: true,
        peak_hold_ms: 1400,
        peak_fall: 0.004,
        peak_gravity: false,
    },
    Preset {
        id: "glacier",
        name: "冰川",
        en: "GLACIER",
        family: "calm",
        tagline: "几乎透明的冰蓝，长时间开着也不累",
        best_on: "dark",
        color_start: "#7FD8E8",
        color_end: "#E8F7FA",
        opacity: 0.62,
        bands: 48,
        bar_gap_ratio: 0.32,
        palette: "",
        palette_mode: "off",
        sensitivity: 0.95,
        attack: 0.3,
        release: 0.32,
        db_floor: -90.0,
        fft_size: 4096,
        peak_cap: false,
        peak_hold_ms: 1400,
        peak_fall: 0.004,
        peak_gravity: false,
    },
    Preset {
        id: "forest",
        name: "林地",
        en: "FOREST",
        family: "calm",
        tagline: "林间散射的绿，起伏都很轻",
        best_on: "both",
        color_start: "#1B7F5A",
        color_end: "#8FD694",
        opacity: 0.66,
        bands: 60,
        bar_gap_ratio: 0.28,
        palette: "",
        palette_mode: "off",
        sensitivity: 1.0,
        attack: 0.34,
        release: 0.28,
        db_floor: -84.0,
        fft_size: 8192,
        peak_cap: true,
        peak_hold_ms: 1400,
        peak_fall: 0.004,
        peak_gravity: false,
    },
    Preset {
        id: "paper-white",
        name: "雾白",
        en: "PAPER WHITE",
        family: "calm",
        tagline: "浅任务栏上的一层雾白，几乎不抢图标",
        best_on: "dark",
        color_start: "#A8B6C4",
        color_end: "#F2F6FA",
        opacity: 0.7,
        bands: 40,
        bar_gap_ratio: 0.35,
        palette: "",
        palette_mode: "off",
        sensitivity: 1.0,
        attack: 0.38,
        release: 0.26,
        db_floor: -88.0,
        fft_size: 4096,
        peak_cap: false,
        peak_hold_ms: 1400,
        peak_fall: 0.004,
        peak_gravity: false,
    },
    Preset {
        id: "still-water",
        name: "静水",
        en: "STILL WATER",
        family: "calm",
        tagline: "青灰到浅荷的四段冷色，横向铺开像水面反光",
        best_on: "both",
        color_start: "#4A7C8C",
        color_end: "#A8D5C8",
        opacity: 0.62,
        bands: 40,
        bar_gap_ratio: 0.24,
        palette: "#3E6B7A,#5E9AA3,#8FC2B4,#C7E2D4",
        palette_mode: "across",
        sensitivity: 0.9,
        attack: 0.38,
        release: 0.09,
        db_floor: -80.0,
        fft_size: 4096,
        peak_cap: true,
        peak_hold_ms: 1400,
        peak_fall: 0.004,
        peak_gravity: false,
    },
    Preset {
        id: "vinyl",
        name: "黑胶",
        en: "VINYL",
        family: "warm",
        tagline: "黑胶封套的橙，慢悠悠地转",
        best_on: "dark",
        color_start: "#FF6B35",
        color_end: "#FFC371",
        opacity: 0.72,
        bands: 44,
        bar_gap_ratio: 0.38,
        palette: "",
        palette_mode: "off",
        sensitivity: 1.05,
        attack: 0.5,
        release: 0.24,
        db_floor: -82.0,
        fft_size: 4096,
        peak_cap: true,
        peak_hold_ms: 1100,
        peak_fall: 0.006,
        peak_gravity: false,
    },
    Preset {
        id: "tungsten",
        name: "钨丝",
        en: "TUNGSTEN",
        family: "warm",
        tagline: "钨丝灯从暗橙烧到暖白",
        best_on: "dark",
        color_start: "#C8791A",
        color_end: "#FFE0A3",
        opacity: 0.58,
        bands: 32,
        bar_gap_ratio: 0.45,
        palette: "",
        palette_mode: "off",
        sensitivity: 0.95,
        attack: 0.4,
        release: 0.3,
        db_floor: -88.0,
        fft_size: 8192,
        peak_cap: false,
        peak_hold_ms: 1100,
        peak_fall: 0.006,
        peak_gravity: false,
    },
    Preset {
        id: "sunset-tape",
        name: "日落磁带",
        en: "SUNSET TAPE",
        family: "warm",
        tagline: "旧磁带里的落日，边缘有点糊",
        best_on: "both",
        color_start: "#E8574A",
        color_end: "#F7B267",
        opacity: 0.8,
        bands: 52,
        bar_gap_ratio: 0.3,
        palette: "",
        palette_mode: "off",
        sensitivity: 1.1,
        attack: 0.6,
        release: 0.18,
        db_floor: -76.0,
        fft_size: 4096,
        peak_cap: true,
        peak_hold_ms: 1100,
        peak_fall: 0.006,
        peak_gravity: false,
    },
    Preset {
        id: "gold-leaf",
        name: "鎏金",
        en: "GOLD LEAF",
        family: "warm",
        tagline: "暗金到亮金，像烫在纸上的字",
        best_on: "dark",
        color_start: "#6B4E16",
        color_end: "#FFD700",
        opacity: 0.85,
        bands: 48,
        bar_gap_ratio: 0.36,
        palette: "",
        palette_mode: "off",
        sensitivity: 1.08,
        attack: 0.58,
        release: 0.2,
        db_floor: -78.0,
        fft_size: 8192,
        peak_cap: true,
        peak_hold_ms: 1100,
        peak_fall: 0.006,
        peak_gravity: false,
    },
    Preset {
        id: "amber-radio",
        name: "琥珀收音机",
        en: "AMBER RADIO",
        family: "warm",
        tagline: "柱内从暗茶爬到暖白，像老收音机的刻度灯",
        best_on: "dark",
        color_start: "#7A3E12",
        color_end: "#FFE0A3",
        opacity: 0.8,
        bands: 36,
        bar_gap_ratio: 0.26,
        palette: "#6B3410,#B4661B,#E8A94B,#FFE0A3",
        palette_mode: "vertical",
        sensitivity: 0.95,
        attack: 0.44,
        release: 0.1,
        db_floor: -80.0,
        fft_size: 4096,
        peak_cap: true,
        peak_hold_ms: 1100,
        peak_fall: 0.006,
        peak_gravity: false,
    },
    Preset {
        id: "ink-wash",
        name: "水墨",
        en: "INK WASH",
        family: "melancholy",
        tagline: "墨在水里散开，只留一点灰青",
        best_on: "light",
        color_start: "#16181D",
        color_end: "#5B6B73",
        opacity: 0.62,
        bands: 32,
        bar_gap_ratio: 0.4,
        palette: "",
        palette_mode: "off",
        sensitivity: 0.95,
        attack: 0.35,
        release: 0.28,
        db_floor: -90.0,
        fft_size: 8192,
        peak_cap: false,
        peak_hold_ms: 1600,
        peak_fall: 0.004,
        peak_gravity: false,
    },
    Preset {
        id: "graphite",
        name: "石墨",
        en: "GRAPHITE",
        family: "melancholy",
        tagline: "石墨灰，雨天窗玻璃的那种反光",
        best_on: "light",
        color_start: "#3A3F47",
        color_end: "#9099A3",
        opacity: 0.55,
        bands: 48,
        bar_gap_ratio: 0.3,
        palette: "",
        palette_mode: "off",
        sensitivity: 1.0,
        attack: 0.42,
        release: 0.22,
        db_floor: -86.0,
        fft_size: 4096,
        peak_cap: false,
        peak_hold_ms: 1600,
        peak_fall: 0.004,
        peak_gravity: false,
    },
    Preset {
        id: "mist-lilac",
        name: "薄雾紫",
        en: "MIST LILAC",
        family: "melancholy",
        tagline: "褪了色的紫，落得比别的都慢",
        best_on: "dark",
        color_start: "#A78BFA",
        color_end: "#E9D5FF",
        opacity: 0.6,
        bands: 56,
        bar_gap_ratio: 0.3,
        palette: "",
        palette_mode: "off",
        sensitivity: 0.95,
        attack: 0.3,
        release: 0.3,
        db_floor: -88.0,
        fft_size: 4096,
        peak_cap: false,
        peak_hold_ms: 1600,
        peak_fall: 0.004,
        peak_gravity: false,
    },
    Preset {
        id: "rain-night",
        name: "雨夜蓝",
        en: "RAIN NIGHT",
        family: "melancholy",
        tagline: "午夜蓝到雾灰，滑块落得最慢，像雨停在窗上",
        best_on: "dark",
        color_start: "#2A3A5C",
        color_end: "#8C99B5",
        opacity: 0.7,
        bands: 44,
        bar_gap_ratio: 0.24,
        palette: "",
        palette_mode: "off",
        sensitivity: 0.85,
        attack: 0.3,
        release: 0.07,
        db_floor: -80.0,
        fft_size: 4096,
        peak_cap: true,
        peak_hold_ms: 1600,
        peak_fall: 0.004,
        peak_gravity: false,
    },
    Preset {
        id: "faded-rainbow",
        name: "褪色彩虹",
        en: "FADED RAINBOW",
        family: "melancholy",
        tagline: "低饱和的虹，颜色还在，情绪已经淡了",
        best_on: "both",
        color_start: "#6E7BA6",
        color_end: "#B5A08F",
        opacity: 0.66,
        bands: 52,
        bar_gap_ratio: 0.22,
        palette: "#6E7BA6,#8A7FA8,#A88FA0,#B5A08F",
        palette_mode: "across",
        sensitivity: 0.88,
        attack: 0.32,
        release: 0.07,
        db_floor: -80.0,
        fft_size: 4096,
        peak_cap: true,
        peak_hold_ms: 1600,
        peak_fall: 0.004,
        peak_gravity: false,
    },
    Preset {
        id: "vu-meter",
        name: "VU 表",
        en: "VU METER",
        family: "serious",
        tagline: "绿到红的 VU 表，读数比好看重要",
        best_on: "both",
        color_start: "#34C759",
        color_end: "#FF3B30",
        opacity: 0.78,
        bands: 36,
        bar_gap_ratio: 0.42,
        palette: "",
        palette_mode: "off",
        sensitivity: 1.0,
        attack: 0.55,
        release: 0.22,
        db_floor: -78.0,
        fft_size: 4096,
        peak_cap: true,
        peak_hold_ms: 1000,
        peak_fall: 0.006,
        peak_gravity: false,
    },
    Preset {
        id: "thermal",
        name: "热成像",
        en: "THERMAL",
        family: "serious",
        tagline: "冷蓝到橙红的热像，层次分明",
        best_on: "dark",
        color_start: "#1B3A8F",
        color_end: "#FF8A00",
        opacity: 0.88,
        bands: 88,
        bar_gap_ratio: 0.16,
        palette: "",
        palette_mode: "off",
        sensitivity: 1.2,
        attack: 0.75,
        release: 0.12,
        db_floor: -72.0,
        fft_size: 4096,
        peak_cap: true,
        peak_hold_ms: 1000,
        peak_fall: 0.006,
        peak_gravity: false,
    },
    Preset {
        id: "blueprint",
        name: "蓝图",
        en: "BLUEPRINT",
        family: "serious",
        tagline: "工程蓝图蓝，规整、没有多余情绪",
        best_on: "both",
        color_start: "#1E6FFF",
        color_end: "#7FB0FF",
        opacity: 0.6,
        bands: 56,
        bar_gap_ratio: 0.25,
        palette: "",
        palette_mode: "off",
        sensitivity: 1.05,
        attack: 0.48,
        release: 0.2,
        db_floor: -82.0,
        fft_size: 4096,
        peak_cap: true,
        peak_hold_ms: 1000,
        peak_fall: 0.006,
        peak_gravity: false,
    },
    Preset {
        id: "oscilloscope",
        name: "示波绿",
        en: "OSCILLOSCOPE",
        family: "serious",
        tagline: "柱内三段绿阶，像仪器屏幕上的读数条",
        best_on: "dark",
        color_start: "#124A3A",
        color_end: "#8FE3C8",
        opacity: 0.86,
        bands: 64,
        bar_gap_ratio: 0.16,
        palette: "#124A3A,#3FBFA0,#8FE3C8",
        palette_mode: "vertical",
        sensitivity: 1.0,
        attack: 0.55,
        release: 0.12,
        db_floor: -80.0,
        fft_size: 4096,
        peak_cap: false,
        peak_hold_ms: 1000,
        peak_fall: 0.006,
        peak_gravity: false,
    },
    Preset {
        id: "monochrome-grid",
        name: "网格灰",
        en: "MONOCHROME GRID",
        family: "serious",
        tagline: "每根柱一档灰，像坐标纸，严谨到没有情绪",
        best_on: "both",
        color_start: "#3A3F47",
        color_end: "#B8C0C8",
        opacity: 0.74,
        bands: 72,
        bar_gap_ratio: 0.14,
        palette: "#3A3F47,#6B737D,#9AA2AC,#D2D8DE",
        palette_mode: "per_bar",
        sensitivity: 1.0,
        attack: 0.5,
        release: 0.11,
        db_floor: -80.0,
        fft_size: 4096,
        peak_cap: false,
        peak_hold_ms: 1000,
        peak_fall: 0.006,
        peak_gravity: false,
    },
    Preset {
        id: "deep-field",
        name: "深空青紫",
        en: "DEEP FIELD",
        family: "rad",
        tagline: "青紫深空，最百搭的默认款",
        best_on: "both",
        color_start: "#00C8FF",
        color_end: "#7B61FF",
        opacity: 0.65,
        bands: 64,
        bar_gap_ratio: 0.2,
        palette: "",
        palette_mode: "off",
        sensitivity: 1.0,
        attack: 0.55,
        release: 0.12,
        db_floor: -80.0,
        fft_size: 4096,
        peak_cap: true,
        peak_hold_ms: 600,
        peak_fall: 0.014,
        peak_gravity: true,
    },
    Preset {
        id: "aurora-pulse",
        name: "极光脉冲",
        en: "AURORA PULSE",
        family: "rad",
        tagline: "极光绿撞紫，96 柱密到发光",
        best_on: "dark",
        color_start: "#00FFA3",
        color_end: "#8B5CF6",
        opacity: 0.78,
        bands: 96,
        bar_gap_ratio: 0.15,
        palette: "",
        palette_mode: "off",
        sensitivity: 1.15,
        attack: 0.72,
        release: 0.11,
        db_floor: -76.0,
        fft_size: 8192,
        peak_cap: true,
        peak_hold_ms: 600,
        peak_fall: 0.014,
        peak_gravity: true,
    },
    Preset {
        id: "vermilion",
        name: "朱印",
        en: "VERMILION",
        family: "rad",
        tagline: "朱砂印泥，一记就很重",
        best_on: "both",
        color_start: "#B4261D",
        color_end: "#FF6B5E",
        opacity: 0.82,
        bands: 64,
        bar_gap_ratio: 0.22,
        palette: "",
        palette_mode: "off",
        sensitivity: 1.15,
        attack: 0.68,
        release: 0.14,
        db_floor: -76.0,
        fft_size: 4096,
        peak_cap: true,
        peak_hold_ms: 600,
        peak_fall: 0.014,
        peak_gravity: true,
    },
    Preset {
        id: "mint-static",
        name: "薄荷静电",
        en: "MINT STATIC",
        family: "rad",
        tagline: "薄荷绿里带点电流噪",
        best_on: "light",
        color_start: "#8AD9BE",
        color_end: "#137A5B",
        opacity: 0.7,
        bands: 72,
        bar_gap_ratio: 0.2,
        palette: "",
        palette_mode: "off",
        sensitivity: 1.1,
        attack: 0.62,
        release: 0.16,
        db_floor: -80.0,
        fft_size: 4096,
        peak_cap: true,
        peak_hold_ms: 600,
        peak_fall: 0.014,
        peak_gravity: true,
    },
    Preset {
        id: "glitch-spectrum",
        name: "光谱故障",
        en: "GLITCH SPECTRUM",
        family: "rad",
        tagline: "五段高饱和色按柱跳变，配合重力滑块最野",
        best_on: "dark",
        color_start: "#00E5FF",
        color_end: "#FF2BD6",
        opacity: 0.9,
        bands: 88,
        bar_gap_ratio: 0.18,
        palette: "#00E5FF,#7B61FF,#FF2BD6,#FF3B30,#FFE66D",
        palette_mode: "per_bar",
        sensitivity: 1.25,
        attack: 0.8,
        release: 0.2,
        db_floor: -80.0,
        fft_size: 8192,
        peak_cap: true,
        peak_hold_ms: 600,
        peak_fall: 0.014,
        peak_gravity: true,
    },
];

/// 首次运行 / 恢复默认时使用的预置 id。
pub const DEFAULT_PRESET: &str = "deep-field";

/// 按 id 查预置。
#[must_use]
pub fn by_id(id: &str) -> Option<&'static Preset> {
    PRESETS.iter().find(|p| p.id == id)
}

/// 把指定预置展开到配置上并做一次校验。
///
/// # Errors
///
/// `id` 不存在时返回错误，配置保持不变。
pub fn apply_preset(cfg: &mut Config, id: &str) -> Result<(), String> {
    let preset = by_id(id).ok_or_else(|| format!("未知预置 {id}"))?;
    preset.apply_to(cfg);
    let _ = cfg.validate();
    Ok(())
}

/// 用户命名保存的一套方案。
#[derive(Debug, Clone, PartialEq)]
pub struct UserPreset {
    /// 安全化的文件标识（只允许 ASCII 字母数字、`-`、`_`）。
    pub id: String,
    /// 用户起的名字。
    pub name: String,
    /// 创建时间（Unix 毫秒）。
    pub created_ms: u64,
    /// 保存的整套参数。
    pub cfg: Config,
}

impl UserPreset {
    /// 把这套方案还原到 `cfg`。
    pub fn apply_to(&self, cfg: &mut Config) {
        *cfg = self.cfg.clone();
        let _ = cfg.validate();
    }
}

/// 用户方案的本地存储（一个方案一个 TOML 文件）。
#[derive(Debug, Clone)]
pub struct UserPresetStore {
    dir: PathBuf,
}

impl UserPresetStore {
    /// 默认存储目录：`~/.config/deepin-spectrum/presets/`。
    #[must_use]
    pub fn open() -> Self {
        let dir = store::default_path()
            .parent()
            .map_or_else(|| PathBuf::from("."), PathBuf::from)
            .join("presets");
        Self { dir }
    }

    /// 指定目录（测试与 `--config` 场景使用）。
    #[must_use]
    pub const fn with_dir(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// 列出全部用户方案；单个坏文件会被跳过，不影响其它方案。
    #[must_use]
    pub fn list(&self) -> Vec<UserPreset> {
        let Ok(rd) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for entry in rd.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("toml") {
                continue;
            }
            let Some(id) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if !safe_id(id) {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            if let Some(p) = decode_user(&text, id) {
                out.push(p);
            }
        }
        out.sort_by(|a, b| a.created_ms.cmp(&b.created_ms));
        out
    }

    /// 按 id 查用户方案。
    #[must_use]
    pub fn get(&self, id: &str) -> Option<UserPreset> {
        if !safe_id(id) {
            return None;
        }
        let text = std::fs::read_to_string(self.dir.join(format!("{id}.toml"))).ok()?;
        decode_user(&text, id)
    }

    /// 命名保存当前配置；同名会更新已有方案，而不是新增一条。
    ///
    /// # Errors
    ///
    /// 名称非法、方案数达上限、写盘失败。
    pub fn save(&self, name: &str, cfg: &Config) -> Result<UserPreset, String> {
        validate_name(name)?;
        let name = name.trim();
        let existing = self.list().into_iter().find(|u| u.name == name);
        if existing.is_none() && self.list().len() >= 30 {
            return Err("最多保存 30 套方案，请先删除不用的".to_owned());
        }
        let preset = UserPreset {
            id: existing.as_ref().map_or_else(new_id, |u| u.id.clone()),
            name: name.to_owned(),
            created_ms: existing.as_ref().map_or_else(now_ms, |u| u.created_ms),
            cfg: cfg.clone(),
        };
        std::fs::create_dir_all(&self.dir).map_err(|e| format!("创建方案目录失败：{e}"))?;
        let path = self.dir.join(format!("{}.toml", preset.id));
        let text = encode_user(&preset);
        store::write_atomic_text(&path, &text).map_err(|e| format!("保存方案失败：{e}"))?;
        Ok(preset)
    }

    /// 删除一套方案。
    ///
    /// # Errors
    ///
    /// id 非法（防路径遍历）或文件不存在/不可删。
    pub fn delete(&self, id: &str) -> Result<(), String> {
        if !safe_id(id) {
            return Err("非法的方案 id".to_owned());
        }
        let path = self.dir.join(format!("{id}.toml"));
        if !path.exists() {
            return Err(format!("方案 {id} 不存在"));
        }
        std::fs::remove_file(&path).map_err(|e| format!("删除方案失败：{e}"))
    }
}

/// 名称校验：非空、≤24 字符、不含控制字符与 TOML/路径敏感字符。
///
/// # Errors
///
/// 名称不符合规则时给出可直接展示给用户的原因。
pub fn validate_name(name: &str) -> Result<(), String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err("名称不能为空".to_owned());
    }
    if trimmed.chars().count() > 24 {
        return Err("名称最多 24 个字".to_owned());
    }
    if trimmed
        .chars()
        .any(|c| c.is_control() || matches!(c, '<' | '>' | '"' | '\'' | '/' | '\\'))
    {
        return Err("名称不能包含尖括号、引号、斜杠或控制字符".to_owned());
    }
    Ok(())
}

fn safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

fn new_id() -> String {
    format!("user-{}", now_ms())
}

fn fnum(v: f32) -> String {
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

fn encode_user(u: &UserPreset) -> String {
    let c = &u.cfg;
    format!(
        "# deepin-spectrum 我的方案（自动生成）\n\
         # 改动会自动保存；改名请删除后重新保存\n\
         version = 1\n\n\
         [meta]\n\
         name = {name}\n\
         created_ms = {created}\n\n\
         [visual]\n\
         color_start = {cs}\n\
         color_end = {ce}\n\
         palette = {pal}\n\
         palette_mode = {pm}\n\
         opacity = {op}\n\
         bands = {bands}\n\
         bar_gap_ratio = {gap}\n\
         peak_cap = {cap}\n\n\
         [analyze]\n\
         sensitivity = {sens}\n\
         attack = {atk}\n\
         release = {rel}\n\
         db_floor = {floor}\n\
         fft_size = {fft}\n\
         peak_hold_ms = {hold}\n\
         peak_fall = {fall}\n\
         peak_gravity = {grav}\n",
        name = quote(&u.name),
        created = u.created_ms,
        cs = quote(&c.visual.color_start),
        ce = quote(&c.visual.color_end),
        pal = quote(&c.visual.palette),
        pm = quote(&c.visual.palette_mode),
        op = fnum(c.visual.opacity),
        bands = c.visual.bands,
        gap = fnum(c.visual.bar_gap_ratio),
        cap = c.visual.peak_cap,
        sens = fnum(c.analyze.sensitivity),
        atk = fnum(c.analyze.attack),
        rel = fnum(c.analyze.release),
        floor = fnum(c.analyze.db_floor),
        fft = c.analyze.fft_size,
        hold = c.analyze.peak_hold_ms,
        fall = fnum(c.analyze.peak_fall),
        grav = c.analyze.peak_gravity,
    )
}

fn decode_user(text: &str, id: &str) -> Option<UserPreset> {
    let doc = parse(text).ok()?;
    let mut cfg = Config::default();
    let mut name: Option<String> = None;
    let mut created_ms = 0_u64;
    for e in doc.entries() {
        let section = e.section.as_deref().unwrap_or("");
        match (section, e.key.as_str()) {
            ("meta", "name") => name = e.value.as_str().map(str::to_owned),
            ("meta", "created_ms") => created_ms = e.value.as_int().unwrap_or(0).max(0) as u64,
            ("visual", "color_start") => {
                e.value.as_str()?.clone_into(&mut cfg.visual.color_start);
            }
            ("visual", "color_end") => {
                e.value.as_str()?.clone_into(&mut cfg.visual.color_end);
            }
            ("visual", "palette") => {
                e.value.as_str()?.clone_into(&mut cfg.visual.palette);
            }
            ("visual", "palette_mode") => {
                e.value.as_str()?.clone_into(&mut cfg.visual.palette_mode);
            }
            ("visual", "opacity") => cfg.visual.opacity = e.value.as_float()? as f32,
            ("visual", "bands") => cfg.visual.bands = e.value.as_int()?.max(0) as usize,
            ("visual", "bar_gap_ratio") => {
                cfg.visual.bar_gap_ratio = e.value.as_float()? as f32;
            }
            ("visual", "peak_cap") => cfg.visual.peak_cap = e.value.as_bool()?,
            ("analyze", "sensitivity") => {
                cfg.analyze.sensitivity = e.value.as_float()? as f32;
            }
            ("analyze", "attack") => cfg.analyze.attack = e.value.as_float()? as f32,
            ("analyze", "release") => cfg.analyze.release = e.value.as_float()? as f32,
            ("analyze", "db_floor") => cfg.analyze.db_floor = e.value.as_float()? as f32,
            ("analyze", "fft_size") => cfg.analyze.fft_size = e.value.as_int()?.max(0) as usize,
            ("analyze", "peak_hold_ms") => {
                cfg.analyze.peak_hold_ms = e.value.as_int()?.max(0) as u32;
            }
            ("analyze", "peak_fall") => cfg.analyze.peak_fall = e.value.as_float()? as f32,
            ("analyze", "peak_gravity") => cfg.analyze.peak_gravity = e.value.as_bool()?,
            _ => {}
        }
    }
    let _ = cfg.validate();
    Some(UserPreset {
        id: id.to_owned(),
        name: name?,
        created_ms,
        cfg,
    })
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_PRESET, PRESETS, UserPresetStore, apply_preset, by_id, validate_name};
    use spectrum_core::config::Config;
    use std::path::PathBuf;

    #[test]
    fn ids_are_unique_and_default_exists() {
        assert_eq!(PRESETS.len(), 30);
        let mut ids: Vec<&str> = PRESETS.iter().map(|p| p.id).collect();
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), before, "预置 id 不能重复");
        assert!(by_id(DEFAULT_PRESET).is_some());
        assert!(by_id("nope").is_none());
    }

    #[test]
    fn every_preset_applies_without_warnings() {
        // 反向门：任何一套写进配置后被钳制/回退，都说明数据越界了。
        for p in PRESETS {
            let mut cfg = Config::default();
            apply_preset(&mut cfg, p.id).expect("应用预置失败");
            assert_eq!(cfg.visual.color_start, p.color_start, "{} 配色没生效", p.id);
            assert_eq!(cfg.visual.bands, p.bands, "{} 柱数没生效", p.id);
            assert_eq!(cfg.analyze.bands, p.bands, "{} 分析器柱数未同步", p.id);
            assert_eq!(cfg.visual.palette, p.palette, "{} 多色标没生效", p.id);
            assert_eq!(
                cfg.visual.palette_mode, p.palette_mode,
                "{} 铺色方式没生效",
                p.id
            );
            let warnings = cfg.validate();
            assert!(warnings.is_empty(), "{} 产生了告警：{warnings:?}", p.id);
        }
    }

    #[test]
    fn unknown_preset_leaves_config_untouched() {
        let mut cfg = Config::default();
        let before = cfg.clone();
        assert!(apply_preset(&mut cfg, "does-not-exist").is_err());
        assert_eq!(cfg, before);
    }

    #[test]
    fn preset_switch_keeps_tune_and_motion_fields() {
        // 回归门：用户在「02 微调」「03 动态」调好的值，切换配色预置后必须原样保留，
        // 否则就是用户报告的"选项自动关闭 / 灵敏度被重置"。
        let mut cfg = Config::default();
        cfg.visual.peak_cap = true;
        cfg.analyze.peak_hold_ms = 2500;
        cfg.analyze.peak_fall = 0.041;
        cfg.analyze.peak_gravity = true;
        cfg.analyze.sensitivity = 2.3;
        cfg.analyze.attack = 0.9;
        cfg.analyze.release = 0.33;
        cfg.analyze.db_floor = -105.0;
        cfg.analyze.fft_size = 8192;
        let tune_motion = (
            cfg.visual.peak_cap,
            cfg.analyze.peak_hold_ms,
            cfg.analyze.peak_fall,
            cfg.analyze.peak_gravity,
            cfg.analyze.sensitivity,
            cfg.analyze.attack,
            cfg.analyze.release,
            cfg.analyze.db_floor,
            cfg.analyze.fft_size,
        );

        for p in PRESETS {
            apply_preset(&mut cfg, p.id).expect("应用预置失败");
            assert_eq!(cfg.visual.color_start, p.color_start, "{} 配色没生效", p.id);
            assert_eq!(
                (
                    cfg.visual.peak_cap,
                    cfg.analyze.peak_hold_ms,
                    cfg.analyze.peak_fall,
                    cfg.analyze.peak_gravity,
                    cfg.analyze.sensitivity,
                    cfg.analyze.attack,
                    cfg.analyze.release,
                    cfg.analyze.db_floor,
                    cfg.analyze.fft_size,
                ),
                tune_motion,
                "{} 覆盖了微调/动态调整",
                p.id
            );
        }
    }

    #[test]
    fn name_validation() {
        assert!(validate_name("深夜写代码").is_ok());
        assert!(validate_name("  ").is_err());
        assert!(validate_name("x".repeat(25).as_str()).is_err());
        assert!(validate_name("a/b").is_err());
        assert!(validate_name("a\"b").is_err());
        assert!(validate_name("a\nb").is_err());
    }

    fn temp_store() -> (UserPresetStore, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "ds-presets-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_millis())
        ));
        (UserPresetStore::with_dir(dir.clone()), dir)
    }

    #[test]
    fn user_preset_roundtrip_and_update() {
        let (store, dir) = temp_store();
        let mut cfg = Config::default();
        apply_preset(&mut cfg, "neon-run").expect("应用预置");
        let saved = store.save("深夜写代码", &cfg).expect("保存失败");
        assert_eq!(store.list().len(), 1);

        let loaded = store.get(&saved.id).expect("读回失败");
        assert_eq!(loaded.name, "深夜写代码");
        assert_eq!(loaded.cfg, cfg, "整套参数必须原样还原");

        let mut restored = Config::default();
        loaded.apply_to(&mut restored);
        assert_eq!(restored, cfg);

        // 同名保存更新而不是新增
        let mut other = Config::default();
        apply_preset(&mut other, "vu-meter").expect("应用预置");
        let updated = store.save("深夜写代码", &other).expect("更新失败");
        assert_eq!(updated.id, saved.id);
        assert_eq!(store.list().len(), 1);
        assert_eq!(store.get(&saved.id).expect("读回").cfg, other);

        assert!(store.delete(&saved.id).is_ok());
        assert!(store.list().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn delete_rejects_path_traversal() {
        let (store, dir) = temp_store();
        assert!(store.delete("../config").is_err());
        assert!(store.get("..").is_none());
        let _ = std::fs::remove_dir_all(dir);
    }
}
