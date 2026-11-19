#!/usr/bin/env python3
"""设计方案的断言式自检（零依赖，可直接复跑）。

验证六件事，任何一条不过就 exit(1)：
  1. presets.json 结构完整、ID 不重复、20 套齐全；
  2. key_scope 的键都登记在 schema.rs 里且为 Live / LiveRebuild；
     pending_keys 的键**确实还没**登记（反向门：落地后必须回来更新本脚本）；
  3. core / enhance / motion 的值全在 Config::validate / AnalyzerConfig::validate 合法区间；
  4. HTML 原型内嵌的 JSON 数据块与 presets.json **深度相等**（单一数据源，不再有第二份副本）；
  5. 动画 / 峰值滑块 / 我的方案相关控件与 JS 引用的 id 全部存在；
  6. 能力说明与存储规格齐备。

用法：python3 docs/design/verify_design.py
"""

from __future__ import annotations

import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
DESIGN = ROOT / "docs" / "design"

failures: list[str] = []
checks = 0


def check(cond: bool, msg: str) -> None:
    global checks
    checks += 1
    if not cond:
        failures.append(msg)


# ---------------------------------------------------------------- 1. 结构
data = json.loads((DESIGN / "presets.json").read_text(encoding="utf-8"))
presets = data["presets"]
families = {f["id"] for f in data["families"]}

check(len(presets) == 30, f"预置数量应为 30，实际 {len(presets)}")
check(len(families) == 6, f"情绪分类应为 6，实际 {len(families)}")
from collections import Counter
family_counts = Counter(p["family"] for p in presets)
for fam in families:
    check(family_counts[fam] == 5, f"分类 {fam} 应有 5 套，实际 {family_counts[fam]}")
for f in data["families"]:
    for field in ("id", "name", "en", "name_en", "mood", "mood_en"):
        check(field in f, f"分类 {f.get('id','?')} 缺字段 {field}")
check(len({p["id"] for p in presets}) == len(presets), "预置 id 有重复")
check(all(p["family"] in families for p in presets), "存在不属于任何风格族的预置")
check(sum(1 for p in presets if p.get("default")) == 1, "必须且只能有 1 套默认预置")
for p in presets:
    for field in ("id", "family", "name", "en", "tagline", "best_on", "core", "enhance", "motion"):
        check(field in p, f"{p.get('id','?')} 缺字段 {field}")
    check(p.get("best_on") in ("dark", "light", "both"), f"{p['id']} best_on 非法")
for field in ("capabilities", "storage", "key_scope"):
    check(field in data, f"顶层缺 {field}")
for name in ("animated_preview", "peak_slider", "user_presets", "implementation"):
    check(name in data.get("capabilities", {}), f"capabilities 缺 {name}")
check(data.get("storage", {}).get("max_user_presets") == 30, "storage 上限应为 30")

# ---------------------------------------------------------------- 2. 键的生效范围
schema = (ROOT / "crates" / "spectrum-settings" / "src" / "schema.rs").read_text(encoding="utf-8")
scopes = {
    f"{m.group(1)}.{m.group(2)}": m.group(3)
    for m in re.finditer(r'k\(\s*"([^"]+)",\s*"([^"]+)",\s*Scope::(\w+)', schema, re.S)
}
check(len(scopes) >= 40, f"schema 键表解析异常，只解析到 {len(scopes)} 个键")

declared = set(data["key_scope"])
check("pending_keys" not in data, "P1 已落地，pending_keys 应该已迁进 key_scope")
for key in declared:
    check(key in scopes, f"{key} 不在 schema.rs 的 KEYS 中")
    check(
        scopes.get(key) in ("Live", "LiveRebuild"),
        f"{key} 的 scope 是 {scopes.get(key)}，不是热生效键（预置不能依赖它）",
    )
# 两个新键必须真的落进内核、差分、schema 与 GUI，缺一处就是「改了没反应」
for key in ("analyze.peak_fall", "analyze.peak_gravity"):
    check(key in scopes, f"{key} 未登记进 schema.rs")
core_src = (ROOT / "crates/spectrum-core/src/analyzer.rs").read_text(encoding="utf-8")
check("pub peak_fall: f32" in core_src, "AnalyzerConfig 缺 peak_fall 字段")
check("pub peak_gravity: bool" in core_src, "AnalyzerConfig 缺 peak_gravity 字段")
check("GRAVITY_FRAMES" in core_src, "analyzer.rs 缺重力积分分支")
apply_src = (ROOT / "crates/spectrum-settings/src/apply.rs").read_text(encoding="utf-8")
check('"analyze.peak_fall"' in apply_src and '"analyze.peak_gravity"' in apply_src,
      "apply.rs 的 diff 漏了新键，热重载会静默失效")
schema_src = (ROOT / "crates/spectrum-settings/src/schema.rs").read_text(encoding="utf-8")
check("peak_gravity" in schema_src, "schema.rs 未序列化 peak_gravity")
gui_src = (ROOT / "crates/spectrum-settings-gui/slint/gui.slint").read_text(encoding="utf-8")
gui_lib = (ROOT / "crates/spectrum-settings-gui/src/lib.rs").read_text(encoding="utf-8")
check("peak_fall_value" in gui_src, "gui.slint 缺峰值滑块控件")
check("get_peak_fall" in gui_lib and "on_save_preset" in gui_lib, "GUI 回调未接线")

# P1.5：视觉重构 / 动画预览 / 中英文切换必须真的在产品代码里
# v0.5.0：GridLayout + repeater 的 row/col 实测不生效（卡片被压成一列、文字糊掉），
# 已改为「分类组件 + HorizontalBox 一行 5 卡」，断言随之更新。
check("for card in root.cards" in gui_src and "height: 148px" in gui_src,
      "分类色卡未用 for 生成或卡片高度不足")
check("GridLayout {" not in gui_src, "不应再用会压糊卡片的 GridLayout + repeater")
check("preview_image" in gui_src and "image-fit: fill" in gui_src, "动画预览控件未落地")
check("language_changed" in gui_src and "lang_en" in gui_src, "语言切换控件未落地")
check("on_language_changed" in gui_lib and "Duration::from_millis(200)" in gui_lib,
      "i18n 回调或 200ms 预览 Timer 未接线")
check("set_preview_image" in gui_lib and "refresh_cards" in gui_lib,
      "预览帧或色卡模型未接线")
i18n_src = (ROOT / "crates/spectrum-settings-gui/src/i18n.rs").read_text(encoding="utf-8")
check("pub const ZH" in i18n_src and "pub const EN" in i18n_src, "缺中英文双表")
check("ui.toml" in i18n_src and "fn load" in i18n_src, "语言偏好未持久化")
check("both_languages_have_same_shape" in i18n_src, "缺双表一致性单测")
preview_src = (ROOT / "crates/spectrum-settings-gui/src/preview.rs").read_text(encoding="utf-8")
check("Image::from_rgba8" in preview_src and "SharedPixelBuffer" in preview_src,
      "预览未接 Slint 像素图")
check("frame_draws_visible_pixels" in preview_src, "缺预览像素级单测")
check("sample_palette" in preview_src, "动画预览未同步多色标")

# v0.5.0：多色渲染、情绪分类、界面修复必须真的在产品代码里。
render_src = (ROOT / "crates/spectrum-render/src/lib.rs").read_text(encoding="utf-8")
check("pub enum PaletteMode" in render_src and "MAX_PALETTE" in render_src,
      "渲染层缺多色标类型")
check("palette_off_is_pixel_identical_to_legacy_gradient" in render_src,
      "缺「多色关闭时逐像素等价」回归门")
config_src = (ROOT / "crates/spectrum-core/src/config.rs").read_text(encoding="utf-8")
check("pub fn parse_palette" in config_src and "is_palette_mode" in config_src,
      "config.rs 缺多色标解析/校验")
check('"palette_mode"' in schema_src and '"palette"' in schema_src,
      "schema.rs 缺多色标两个键")
check('"visual.palette"' in apply_src, "apply.rs 的 diff 漏了 palette 键")
check("component PresetSection" in gui_src and "component SaveBar" in gui_src,
      "缺按分类分区的色卡组件或调参页保存条")
check("runtime_mode_label" in gui_src and "mode_labels" in gui_src,
      "显示位置未接语言或 ComboBox 未收窄")
check("width: 220px" in gui_src and "height: 34px" in gui_src,
      "ComboBox 尺寸未收窄")
check("preset_selected(string)" in gui_src, "色卡回调未改为按 id")
check("FAMILY_META" in gui_lib and "set_group0" in gui_lib, "6 个分类未接线")
check("refresh_palette_ui" in gui_lib and "get_palette_value" in gui_lib,
      "多色标未在 GUI 回填/回写（会静默失效）")
check("runtime_mode_label" in i18n_src and "mode_full" in i18n_src,
      "i18n 缺显示位置文案")
check("SaveBar" in gui_src and gui_src.count("SaveBar {") >= 2,
      "微调/动态页都应放保存条")

# v0.5.1：关于页、Logo 与 GPL-3.0 许可合规
check("nav_about" in gui_src and "panel_index == 4" in gui_src,
      "缺 05 关于页或左栏导航")
check('@image-url("logo.png")' in gui_src, "关于页未内嵌 Logo")
check((ROOT / "crates/spectrum-settings-gui/slint/logo.png").is_file(),
      "Logo 资源文件缺失")
check("read-only: true" in gui_src, "邮箱未设为只读可复制")
check("GPL-3.0-only" in gui_src, "关于页未写明协议")
check("about_version" in gui_lib and "CARGO_PKG_VERSION" in gui_lib,
      "版本号未取自 Cargo 元数据")
check("Feelup" in gui_lib and "testangel@163.com" in gui_lib,
      "作者 / 邮箱未注入界面")
check("nav_about" in i18n_src and "about_license_note" in i18n_src,
      "i18n 缺关于页双语字段")
license_path = ROOT / "LICENSE"
check(license_path.is_file()
      and "GNU GENERAL PUBLIC LICENSE" in license_path.read_text(encoding="utf-8", errors="replace")
      and "Version 3, 29 June 2007" in license_path.read_text(encoding="utf-8", errors="replace"),
      "根目录缺 GPL-3 全文")
check('license = "GPL-3.0-only"' in (ROOT / "Cargo.toml").read_text(encoding="utf-8"),
      "主 workspace 未声明 license")
check('license = "GPL-3.0-only"'
      in (ROOT / "crates/spectrum-settings-gui/Cargo.toml").read_text(encoding="utf-8"),
      "GUI workspace 未声明 license")
for crate in ("spectrum-core", "spectrum-render", "spectrum-platform",
              "spectrum-daemon", "spectrum-settings"):
    check("license.workspace = true"
          in (ROOT / "crates" / crate / "Cargo.toml").read_text(encoding="utf-8"),
          f"{crate} 未继承 workspace license")
copyright_text = (ROOT / "debian/copyright").read_text(encoding="utf-8")
check("Upstream-Contact: Feelup <testangel@163.com>" in copyright_text,
      "copyright 缺作者联系方式")
check("Files: LICENSE" in copyright_text, "copyright 缺 LICENSE 段落")
check('"/usr/share/doc/deepin-spectrum/LICENSE.GPL-3"' in
      (ROOT / "scripts/build-deb.sh").read_text(encoding="utf-8"),
      "打包脚本未安装 GPL 全文")

# v0.5.2：品牌名称统一、左上角 Logo、商店文案字数
desktop = (ROOT / "data/deepin-spectrum.desktop").read_text(encoding="utf-8")
check("Name=频谱工坊" in desktop, "启动器中文名未统一")
check("Name[en]=Spectrum Atelier" in desktop, "启动器英文名未统一")
check("任务栏频谱设置" not in desktop, "启动器仍有旧名称")
trayctl = (ROOT / "crates/spectrum-settings-gui/src/trayctl.rs").read_text(encoding="utf-8")
check("任务栏频谱托盘" not in trayctl, "托盘自启动项仍有旧名称")
check('text: "频"' not in gui_src, "左上角仍是红底「频」字占位图标")
check(gui_src.count('@image-url("logo.png")') >= 2,
      "左上角与关于页应复用同一份 Logo 资源")
man = (ROOT / "data/deepin-spectrum.1").read_text(encoding="utf-8")
workspace_version = re.search(r'^version = "([0-9.]+)"',
                              (ROOT / "Cargo.toml").read_text(encoding="utf-8"), re.M)
check(workspace_version is not None
      and f"deepin-spectrum {workspace_version.group(1)}" in man,
      "man page 版本号与主工程不一致")

store_path = ROOT / "docs/store-listing.md"
check(store_path.is_file(), "缺应用商店上架文案")
if store_path.is_file():
    store = store_path.read_text(encoding="utf-8")
    blocks = re.findall(r"```\n(.*?)\n```", store, re.S)
    check(len(blocks) == 4, f"商店文案应有 4 段可复制内容，实际 {len(blocks)}")
    if len(blocks) == 4:
        for i, (text, limit) in enumerate(zip(blocks, (100, 1000, 100, 1000)), 1):
            check(len(text) <= limit,
                  f"商店文案第 {i} 段 {len(text)} 字符，超过 {limit} 上限")

# 打包脚本：架构探测必须多源降级 + 白名单，绝不能把空值写进 control。
# 受限沙箱里 `dpkg --print-architecture` 因无法 remount /usr 而失败并返回空串。
build_sh = (ROOT / "scripts/build-deb.sh").read_text(encoding="utf-8")
check("detect_arch()" in build_sh and "uname -m" in build_sh,
      "打包脚本缺架构多源降级探测")
check("dpkg-architecture" in build_sh, "打包脚本缺 dpkg-architecture 中间层")
check("无法探测 Debian 架构" in build_sh, "打包脚本缺探测失败时拒绝打包的守卫")
check("PKG_ARCH" in build_sh and "Architecture: $PKG_ARCH" in build_sh,
      "control 未使用探测到的架构值")

for p in presets:
    check(set(p["core"]) <= declared, f"{p['id']} 的 core 用了未声明的键")
    check(set(p["enhance"]) <= declared, f"{p['id']} 的 enhance 用了未声明的键")
    check(set(p["motion"]) <= declared, f"{p['id']} 的 motion 用了未声明的键")

# ---------------------------------------------------------------- 3. 值域
analyzer = (ROOT / "crates" / "spectrum-core" / "src" / "analyzer.rs").read_text(encoding="utf-8")
max_bands = int(re.search(r"MAX_BANDS\s*:\s*usize\s*=\s*(\d+)", analyzer).group(1))
max_fft = int(re.search(r"MAX_FFT_SIZE\s*:\s*usize\s*=\s*(\d+)", analyzer).group(1))
check(max_bands == 128, f"MAX_BANDS 变了（{max_bands}），需复核值域断言")
check(max_fft == 8192, f"MAX_FFT_SIZE 变了（{max_fft}），需复核值域断言")

RANGES = {
    "visual.opacity": (0.05, 1.0),
    "visual.bands": (16, max_bands),
    "visual.bar_gap_ratio": (0.0, 0.9),
    "analyze.sensitivity": (0.2, 3.0),
    "analyze.attack": (0.0, 1.0),
    "analyze.release": (0.0, 1.0),
    "analyze.db_floor": (-120.0, -1.0),
    "analyze.fft_size": (1024, max_fft),
}
HEX = re.compile(r"^#[0-9A-Fa-f]{6}$")

for p in presets:
    core = p["core"]
    for key, (lo, hi) in RANGES.items():
        v = core[key]
        check(isinstance(v, (int, float)), f"{p['id']} 的 {key} 不是数字")
        check(lo <= v <= hi, f"{p['id']} 的 {key}={v} 越界（合法 {lo}~{hi}）")
    check(HEX.match(core["visual.color_start"]) is not None, f"{p['id']} 柱底色不是 #RRGGBB")
    check(HEX.match(core["visual.color_end"]) is not None, f"{p['id']} 柱顶色不是 #RRGGBB")
    # v0.5.0 多色标：off 必须留空，其余模式必须有 3~8 个合法色标。
    mode = core["visual.palette_mode"]
    check(mode in {"off", "across", "per_bar", "vertical"},
          f"{p['id']} palette_mode={mode} 非法")
    pal = core["visual.palette"]
    if mode == "off":
        check(pal == "", f"{p['id']} 关闭多色时 palette 必须为空")
    else:
        stops = pal.split(",")
        check(3 <= len(stops) <= 8, f"{p['id']} 色标数 {len(stops)} 越界")
        check(all(HEX.match(x.strip()) for x in stops), f"{p['id']} 有非法色标")
    fft = core["analyze.fft_size"]
    check(fft & (fft - 1) == 0, f"{p['id']} 的 fft_size={fft} 不是 2 的幂")
    check(fft != 2048, f"{p['id']} 用了 2048 档（F56：低频段会塌缩）")

    enh = p["enhance"]
    check(isinstance(enh["visual.peak_cap"], bool), f"{p['id']} peak_cap 不是布尔")
    hold = enh["analyze.peak_hold_ms"]
    check(0 <= hold <= 3000, f"{p['id']} 悬停时长 {hold}ms 越界")
    check(hold % 20 == 0, f"{p['id']} 悬停时长 {hold}ms 不是滑块步长 20 的倍数（界面会静默夹紧）")

    fx = p["motion"]
    fall = fx["analyze.peak_fall"]
    check(0.002 <= fall <= 0.05, f"{p['id']} 回落速度 {fall} 越界")
    check(abs(round(fall / 0.002) * 0.002 - fall) < 1e-9, f"{p['id']} 回落速度 {fall} 不是步长 0.002 的倍数")
    check(isinstance(fx["analyze.peak_gravity"], bool), f"{p['id']} peak_gravity 不是布尔")

# 各族的动态性格必须真的有区分度
holds = {p["family"]: p["enhance"]["analyze.peak_hold_ms"] for p in presets}
falls = {p["family"]: p["motion"]["analyze.peak_fall"] for p in presets}
check(len(set(holds.values())) >= 4, f"各族悬停时长区分度不足：{holds}")
check(len(set(falls.values())) >= 3, f"各族回落速度区分度不足：{falls}")

# 三种铺色方式都要有实际预置覆盖，且至少 6 套多色方案。
modes = {p["core"]["visual.palette_mode"] for p in presets if p["core"]["visual.palette"]}
check(modes == {"across", "per_bar", "vertical"}, f"铺色方式覆盖不全：{modes}")
check(sum(1 for p in presets if p["core"]["visual.palette"]) >= 6, "多色预置太少")

# ---------------------------------------------------------------- 4. 单一数据源
html = (DESIGN / "spectrum-ui-prototype.html").read_text(encoding="utf-8")
m = re.search(r'<script type="application/json" id="presetData">(.*?)</script>', html, re.S)
check(m is not None, "HTML 里找不到内嵌的 presetData JSON 块")
if m:
    embedded = json.loads(m.group(1))
    check(embedded == data, "HTML 内嵌 JSON 与 presets.json 不一致（不要再维护第二份数据副本）")
check("const PRESETS = [" not in html, "HTML 里仍残留手写的 PRESETS 数组副本")

# ---------------------------------------------------------------- 5. 控件与引用
ids_in_html = set(re.findall(r'id="([^"]+)"', html))
js_refs = set(re.findall(r"\$\('#([A-Za-z][\w-]*)'\)", html))
missing = sorted(js_refs - ids_in_html)
check(not missing, f"JS 引用了 HTML 中不存在的 id：{missing}")
for required in (
    "presetData", "slowBtn", "playBtn", "peakCap", "capGravity",
    "saveName", "saveBtn", "userList", "presetGrid", "spectrum", "tomlView",
):
    check(required in ids_in_html, f"HTML 缺少必需控件 id：{required}")
for required in ('data-k="hold"', 'data-k="fall"', "ANIMATED PREVIEW", "我的方案", "峰值滑块"):
    check(required in html, f"HTML 缺少新增功能标记：{required}")
check("step(dt * state.speed)" in html, "慢放没有接到动画时间轴上")
check("caps[i] = Math.max(caps[i], levels[i])" in html, "峰值滑块缺少防穿底约束")
check("localStorage.setItem(LS_KEY" in html, "自定义方案缺少持久化")

# ---------------------------------------------------------------- 结论
print(f"断言总数：{checks}")
print(f"预置方案：{len(presets)} 套 / {len(families)} 个情绪分类")
print(f"多色方案：{sum(1 for p in presets if p['core']['visual.palette'])} 套 · 铺法 {sorted(modes)}")
print(f"热生效键：{len(declared)} 个 · 全部已落地（含 v0.5.0 多色标 2 键）")
print(f"动态性格：悬停 {sorted(set(holds.values()))} ms · 回落 {sorted(set(falls.values()))} /帧")
print(f"柱数范围：{min(p['core']['visual.bands'] for p in presets)} ~ {max(p['core']['visual.bands'] for p in presets)}")
print(f"FFT 档位：{sorted({p['core']['analyze.fft_size'] for p in presets})}")
if failures:
    print(f"\nFAIL：{len(failures)} 条")
    for f in failures[:30]:
        print("  - " + f)
    sys.exit(1)
print("\nPASS：设计方案、单一数据源、多色标、关于页、许可与品牌实现全部一致")
