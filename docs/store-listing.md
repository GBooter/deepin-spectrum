# deepin 应用商店上架文案 · 频谱工坊 / Spectrum Atelier

> 版本 v0.5.2 · 2026-11-15 · 供应用商店后台直接复制
> 字数按**字符数（含标点、空格与换行）**统计，已用脚本逐条核验：

| 条目 | 实际 | 上限 |
|---|---:|---:|
| 中文一句话介绍 | 64 | 100 |
| 中文应用介绍 | 848 | 1000 |
| English tagline | 92 | 100 |
| English description | 987 | 1000 |

产品名称：**频谱工坊 / Spectrum Atelier**
开源协议：GPL-3.0-only · 作者：Feelup · 联系：testangel@163.com

---

## 中文 · 一句话介绍（64 / 100）

```
让 Deepin 任务栏跟着声音呼吸：实时音频频谱、30 套情绪化配色与七彩渐变、峰值滑块悬浮缓落，改完即生效，不改动系统外壳。
```

## 中文 · 应用介绍（848 / 1000）

```
频谱工坊是一款为 deepin / UOS 25 桌面打造的任务栏音频频谱插件。它让任务栏随着正在播放的音乐、视频或游戏声音实时起伏，不需要更换主题、不修改 dde-shell 源码，也不改变系统默认音频输出。

【看得见的节奏】
软件以独立常驻进程读取 PulseAudio / PipeWire 的 monitor 输出，经 FFT 分析后把频谱柱叠加到任务栏的空输入区。叠加层使用 XShape 输入穿透，鼠标点击、悬停与窗口操作完全不受影响；X11 与 Wayland（Treeland）会话均可使用，并适配 175% 分数缩放。

【30 套风格，按情绪挑】
内置 30 套预置，按欢快、平静、温暖、忧伤、严肃、炫酷 6 个情绪分类排列，每类 5 套。除经典双色渐变外，还支持多色标：横向彩虹渐变、一根柱一个颜色、柱内多段渐变三种铺法，8 套多色方案开箱即用。每套连起音、衰减和峰值滑块的「跳动性格」都调好了，点一张色卡整套生效。

【峰值滑块 · 悬浮缓落】
声音一冲上来，柱顶的小滑块被顶飞，短暂停留后与柱体分离、缓慢落回。悬停时长、回落速度、是否重力加速都可以调，慢放预览能看清全过程。

【拧成自己的样子】
微调页可改不透明度、柱数、柱间隙与配色；动态页可调灵敏度、起音、衰减、底噪门限与 FFT 档位。调好后直接在页面底部命名保存为「我的方案」，整套参数（含多色标）一次留存，随时选回、同名更新。改动写入配置后约 250ms 热生效，无需重启。

【安静、克制、不打扰】
设置界面提供动画预览、深色/浅色任务栏切换与中英文切换；支持开机自启（systemd 用户单元）、托盘控制、全屏播放视频时自动隐藏。软件渲染让 CPU 占用长期控制在 1.5% 以下。

【开源】
GPL-3.0-only，作者 Feelup，联系 testangel@163.com。配置位于 ~/.config/deepin-spectrum/config.toml，不过全部设置都能在图形界面完成。
```

---

## English · Tagline (92 / 100)

```
Live audio spectrum on the Deepin dock: 30 mood presets, rainbow gradients, zero shell mods.
```

## English · Description (987 / 1000)

```
Spectrum Atelier puts a live audio spectrum on the deepin / UOS 25 dock - no theme changes, no shell patches. A small process reads the PulseAudio / PipeWire monitor, runs an FFT and paints bars into an empty dock region; XShape pass-through keeps clicks intact. X11 and Wayland supported.

30 PRESETS, SIX MOODS
Cheerful, Calm, Warm, Melancholy, Serious, Radical. Multi-stop palettes offer a horizontal rainbow, one colour per bar, or vertical segments; eight multicolour presets included.

PEAK SLIDER
A hit kicks a slider above the bar; it hovers, separates, then slowly falls back. Hold, fall speed and gravity are adjustable, with slow-motion preview.

MAKE IT YOURS
Tune opacity, bars, gap, colours, sensitivity, attack, release, noise floor and FFT size; save it all as a named preset. Changes apply in ~250ms. Also: animated preview, dark/light dock, Chinese/English UI, autostart, tray control, fullscreen auto-hide. CPU under 1.5%.

GPL-3.0-only, by Feelup (testangel@163.com).
```

---

## 上架辅助信息（供后台填写参考）

| 项 | 建议 |
|---|---|
| 分类 | 实用工具 / 影音（Utility;AudioVideo，与 desktop 文件一致） |
| 中文关键词 | 频谱、音频可视化、任务栏、桌面美化、deepin、七彩、均衡器 |
| English keywords | spectrum, audio visualizer, dock, taskbar, deepin, rainbow, equalizer |
| 主页 / 源码 | 请填写实际仓库地址（未提供，故未写入 `debian/copyright` 的 Source） |
| 支持架构 | amd64（本包）；如需 arm64 请在对应机器重新执行 `scripts/build-deb.sh` |

### 截图顺序建议（商店通常 3~5 张）

1. `docs/design/presets-panel.png` — 30 套情绪分类色卡，第一印象
2. `docs/settings-gui.png` — 默认界面 + 动画预览，展示整体版式
3. `docs/design/runtime-panel.png` — 中英文切换与显示位置
4. `docs/design/about-panel.png` — Logo、作者与 GPL-3.0-only 协议
5. 建议补一张「微调页多色标 + 保存条」截图，突出可调性与命名保存

### 文案取舍说明

- 一句话介绍只讲「是什么 + 最抓眼的两点（30 套情绪配色 / 七彩渐变）+ 不改系统」，不塞技术参数。
- 应用介绍按「是什么 → 原理与兼容性 → 预置 → 峰值滑块 → 自定义 → 性能与运行 → 开源」递进，
  把用户最关心的「会不会破坏系统」放在第一段就回答。
- 英文版不是逐字直译：压缩到 1000 字符内，保留全部关键卖点，去掉中文里的重复修饰。
- 未写「100% 安全」「零风险」等无法验证的承诺；CPU <1.5% 与 250ms 热生效均有项目实测证据。
