# 任务栏音频频谱插件 · 全新 UI 设计方案

> 设计交付 **v1.6** · 2026-11-15 · UI Designer
> 配套文件：`spectrum-ui-prototype.html`（可点击原型）· `presets.json`（20 套预置数据）· `verify_design.py`（断言式自检）
> v1.1 新增：预览定为 **B 档动画** · 新选项 **「峰值滑块 · 悬浮缓落」** · **命名保存「我的方案」**
> v1.0 / v1.1 只新增设计资产；**v1.2：P1 → v0.3.0**（§11）；**v1.3：P1.5 → v0.4.0**（§12）；**v1.4：界面反馈修复 + 情绪分类 + 多色标 → v0.5.0**（§13）。

---

## 0. 结论先行

| 项 | 结论 |
|---|---|
| 现状 | 现有设置界面是 660×800 单页长滚动，约 20 个控件平铺，无导航、无预览、无风格选择（截图 `docs/settings-gui.png`） |
| 新方案 | **4 分区目录式布局 + 刊头 + 任务栏实时预览 + 预置色卡库**；风格为「大胆编辑风 · 杂志排版」（用户选定） |
| 预置 | **20 套 / 5 个风格族**（霓虹赛博、单色克制、暖色复古、自然冷色、实验款），比原计划 8~10 套翻倍 |
| **v1.1 新增** | ① 预览定为 **B 档动画**（46.9fps + 慢放 0.35×）；② 新选项 **「峰值滑块 · 悬浮缓落」**——顶飞 → 悬停 → 与柱顶分离后缓慢回落，可开关、调悬停/回落、切重力；③ **命名保存「我的方案」**，外观 + 动态整套留存、可选用/删除 |
| 工程代价（**修正**） | 20 套预置主体仍**零渲染改动**（10 个 `core` + 2 个 `enhance` 全为已实现热生效键）；但**缓落效果需新增 2 个配置键并改 `analyzer.rs` 的峰值衰减分支**（§6.5），属 P1 小范围内核改动、需授权。自定义方案走独立文件，**不动 `CONFIG_VERSION`** |
| 可立即使用 | 原型内点色卡即改变**动画预览**；可开关/调峰值滑块并慢放观察；命名保存后立即出现在「我的方案」；「复制 TOML」可粘贴进真机配置 |
| 验证 | `verify_design.py` **848 条断言全绿**：HTML 内嵌 JSON 与 `presets.json` 深度相等（单一数据源）、12 键热生效、2 个待落地键反向门检查、动态值域与滑块步长对齐 |
| P1 状态 | ✅ **已落地 v0.3.0**：内核 / 配置层 / CLI / GUI 功能接入、20 套预置、峰值滑块、我的方案（§11）。视觉重构与动画预览留 P1.5 |

---

## 1. 现状诊断（为什么需要重做）

| # | 问题 | 证据 | 影响 |
|---|---|---|---|
| 1 | 单页长滚动，20 个控件无层级 | `slint/gui.slint` L126~L575 一个 `ScrollView` 套全部 | 首次安装找不到「启动频谱」，运行控制混在外观参数里；用户装完播放音乐没反应（历史真机反馈） |
| 2 | 无实时预览 | 全文件无预览组件 | 调颜色只能凭想象，改完还要切到任务栏看；配色是两个 `#RRGGBB` 输入框，认知成本高 |
| 3 | 无「一键换风格」 | Grep `preset` 全仓库 0 命中 | 想换个好看的效果要手动连调 8~10 个滑块，且不知道什么组合好看 |
| 4 | 视觉无记忆点 | 截图为系统默认控件平铺 | 与 deepin 原生设置难以区分，缺少产品性格 |
| 5 | 参数与行为混杂 | `频谱运行 / 外观 / 分析 / 行为` 四段等权重 | 低频操作（自启、托盘）与高频操作（配色）争夺同一屏 |

**设计目标**：把「选一套好看的」变成首屏一次点击；把「调参数」变成有需要的用户才进入的二级动作；把「有没有在跑」变成永远可见的状态。

---

## 2. 信息架构

```
刊头（永远可见）
  ├─ 期号 / 版本 · 主题切换                     ← 品牌层
  ├─ 巨型标题「频谱」+ 描边水印 SPECTRUM        ← 记忆点
  └─ 动画预览：模拟 deepin 任务栏 + 频谱叠加    ← 视觉主角（B 档）
       ├─ 深/浅任务栏 · 播放/暂停 · 慢放 0.35×
       ├─ 柱体 + 可分离的峰值滑块（顶飞 / 悬停 / 缓落）
       └─ 当前方案名 / 描述 / 参数标签 / 导出 TOML

01 预置风格  PRESETS · 20        ← 默认页，主任务
   筛选 chips（全部/5 族） + 20 张编辑式色卡
   └─ 我的方案：命名保存 + 选用 / 删除           ← 用户自定义
02 微调      FINE TUNE           ← 外观参数 + 峰值滑块控制卡
03 动态      MOTION              ← 起音/衰减/FFT（热生效·重建）
04 运行      RUNTIME             ← 启停/自启/托盘/全屏/静音

页脚：状态行 + 恢复默认 + 生效说明
```

**渐进披露**：首屏只要求一个动作「点一张色卡」。20 套按 5 族分组、默认「全部」，4 列网格一屏可扫完；筛选 chips 负责收窄，不新增页面。

---

## 3. 设计系统

### 3.1 设计 Token

| 类别 | 纸白（默认） | 墨黑 | 用途 |
|---|---|---|---|
| 背景 `--paper` | `#F5F3EE` | `#14161A` | 页面底 |
| 卡片底 `--surface` | `#FFFFFF` | `#1C1F25` | 选中态 / 注记块 |
| 主文字 `--ink` | `#16181D` | `#F2EFE8` | 标题、正文、2px 粗规则线 |
| 次级文字 `--muted` | `#6E6B64` | `#93908A` | 说明、编号、kicker |
| 规则线 `--rule` | `#D8D3C8` | `#373B42` | 1px 分隔 |
| 强调 `--accent` | `#C8302A` | `#FF6A5E` | 朱红：选中、编号、焦点、印章 |
| 任务栏模拟 `--dock` | `#1B1E24` / 可切 `#D6DAE1` | — | 预览底 |

**色彩策略**：界面本体是中性纸墨 + 单色朱红；所有高饱和色只存在于预览与色卡。这样既满足「炫酷」，又不让 UI 变成霓虹堆砌。刻意避开通用 AI 审美的紫蓝渐变 + 发光。

| 类别 | 规格 |
|---|---|
| 字体 | 展示：`Noto Serif CJK SC / Georgia`（衬线，强对比）；正文：`Noto Sans CJK SC`（系统无衬线）；数值/编号：等宽字体**仅用于对齐**（不作"科技感"装饰） |
| 字阶 | 9 / 10 / 11 / 12 / 13 / 17 / 22 / 28 / clamp(58~104) px，模块化 ≈1.3 |
| 间距 | 4 / 8 / 12 / 16 / 24 / 32 / 48 / 64 px（4px 基准，8pt 节奏） |
| 圆角 | 色卡与容器 **0**（印刷品感）；按钮/输入框 0~2px。刻意不用大圆角+通用阴影 |
| 分层 | 靠 1px / 2px 规则线与留白，不用投影 |
| 动效 | 180ms `cubic-bezier(.22,1,.36,1)`（ease-out-quart），只动画颜色/透明度/位移，不动画布局属性；无回弹 |

### 3.2 组件库与状态

| 组件 | 规格 | 状态 |
|---|---|---|
| 目录项 | 224px 左栏 · 30px 编号列 · 17px 衬线名 · 9px 英文副名 | default / hover（背景+4px 缩进）/ current（3px 朱红左条） |
| 预置色卡 | 4 列 · 52px 渐变条（**底部=柱底色→顶部=柱顶色**）· 14 根迷你柱 · 编号/名称/英文/描述/参数行 | default / hover / **selected（3px 左条 + 名称变朱红 + 「● 使用中」文字）** |
| 筛选 chip | 文字 + 2px 下边框 | pressed = 朱红下划线 + 加粗 |
| 滑块 | 2px 轨道 · 13px 朱红圆钮 · 右侧等宽数值 | hover / focus-visible / disabled |
| 颜色输入 | 30px 色块 + 原生取色器 + `#RRGGBB` 文本（原型用原生控件演示，Slint 落地板见 §9 R5） | 非法值回退默认色并**显示回退结果** |
| 状态行 | 20px 衬线主句 + 11px 解释 | 成功 / 自定义 / 错误（朱红） |
| 按钮 | 实心墨块 / 描边幽灵 | hover 上移 1px / focus 环 / disabled |

**选中态不只靠颜色**：朱红竖条 + 文字「● 使用中」+ 名称变色三重编码，色盲用户可辨。

---

## 4. 关键交互

| 流程 | 行为 | 设计理由 |
|---|---|---|
| 点色卡 | 10 个 `core` 键一次性写入 → 预览立即改变 → 状态行「已应用「X」· 立即生效」 | 一次点击完成过去 8~10 次滑块操作 |
| 手动改任一参数 | 立即取消预置选中，标题改为「自定义」，状态行说明「已脱离预置」 | **防静默失效**：绝不假装用户还在用某套预置（沿用项目 F33 原则） |
| 恢复默认 | 回到「深空青紫」并清除自定义标记 | 单一出口，不做二次确认（可撤销） |
| 导出 TOML | 生成当前 11 个键的片段；浮点写成 `1.0` 形式；剪贴板不可用时展开文本框手动复制，**不假装成功** | 让设计资产在真机上立刻可用，不依赖 GUI 落地 |
| 深/浅任务栏切换 | 预览底色切换，同时切换混合模拟（深底 `lighter` ≈ additive，浅底 `source-over`） | 高亮色预置在浅色任务栏上可能看不清，选之前就能验 |
| 键盘 | Tab 遍历；色卡上方向键 ±1（上下按列）、Home/End 跳首尾、Enter/Space 选用 | 全功能无鼠标可达 |

**空/错状态**：颜色填错 → 沿用现有 `sync_back()` 把回退后的值显示出来；daemon 未运行 → 预览照常（模拟帧），运行页状态卡明确「已停止」并给出启动按钮；预览动画在系统「减弱动画」开启时默认暂停并画静态帧。

---

## 5. 可访问性（WCAG AA 实测）

对比度按 WCAG 2.1 相对亮度公式实算：

| 组合 | 对比度 | 判定 |
|---|---|---|
| 纸白底 / 正文墨 `#16181D` on `#F5F3EE` | **16.01:1** | AA ✓ |
| 纸白底 / 次级字 `#6E6B64` | **4.79:1** | AA ✓（含 10px kicker） |
| 纸白底 / 朱红强调 `#C8302A` | **4.84:1** | AA ✓ |
| 墨黑底 / 正文 `#F2EFE8` on `#14161A` | **15.77:1** | AA ✓ |
| 墨黑底 / 次级字 `#93908A` | **5.69:1** | AA ✓ |
| 墨黑底 / 朱红强调 `#FF6A5E` | **6.45:1** | AA ✓ |

其余：焦点环 2px 朱红 + 3px offset；色卡点击区 ≥ 160×150px、导航项 ≥ 44px 高；`prefers-reduced-motion` 下动效与滚动全部禁用；`aria-pressed` / `aria-current` / `aria-label` 标注全部交互态；预览画布有文字替代说明。

---

## 6. 20 套频谱预置

### 6.1 一条必须知道的渲染约束

`crates/spectrum-render/src/lib.rs` L269~L283 的 `row_color()` 按**画布绝对 y 位置**插值：`from_bottom` 越大越接近 `color_end`。也就是说：

> **画布底部 = `color_start`（安静时最常看到的主色）；画布顶部 = `color_end`（只有大音量的高柱才带出来的高潮色）。**

所有 20 套都按这个规律配色：主色放柱底，点睛色放柱顶。原型的预览、色卡渐变、TOML 导出全部遵循此约定，所见即所得。

### 6.2 预置总表

| # | 名称 / EN | 族 | 柱底 → 柱顶 | 柱数 | 间隙 | 不透明 | 起音/衰减 | FFT | 帽 | 适配 |
|---|---|---|---|---|---|---|---|---|---|---|
| 01 | 深空青紫 DEEP FIELD ★默认 | 霓虹 | `#00C8FF → #7B61FF` | 64 | 2px | 65% | .55/.12 | 4096 | 开 | 深浅 |
| 02 | 赛博霓虹 NEON RUN | 霓虹 | `#FF2BD6 → #00E5FF` | 80 | 2px | 85% | .78/.10 | 4096 | 开 | 深 |
| 03 | 极光脉冲 AURORA PULSE | 霓虹 | `#00FFA3 → #8B5CF6` | 96 | 2px | 78% | .72/.11 | 8192 | 开 | 深 |
| 04 | 熔岩电音 LAVA CLUB | 霓虹 | `#FF3B30 → #FFD60A` | 72 | 2px | 90% | .85/.09 | 4096 | 开 | 深浅 |
| 05 | 石墨 GRAPHITE | 单色 | `#3A3F47 → #9099A3` | 48 | 3px | 55% | .42/.22 | 4096 | 关 | 浅 |
| 06 | 雾白 PAPER WHITE | 单色 | `#A8B6C4 → #F2F6FA` | 40 | 4px | 70% | .38/.26 | 4096 | 关 | 深 |
| 07 | 蓝图 BLUEPRINT | 单色 | `#1E6FFF → #7FB0FF` | 56 | 3px | 60% | .48/.20 | 4096 | 开 | 深浅 |
| 08 | 水墨 INK WASH | 单色 | `#16181D → #5B6B73` | 32 | 4px | 62% | .35/.28 | 8192 | 关 | 浅 |
| 09 | VU 表 VU METER | 复古 | `#34C759 → #FF3B30` | 36 | 4px | 78% | .55/.22 | 4096 | 开 | 深浅 |
| 10 | 黑胶 VINYL | 复古 | `#FF6B35 → #FFC371` | 44 | 4px | 72% | .50/.24 | 4096 | 开 | 深 |
| 11 | 钨丝 TUNGSTEN | 复古 | `#C8791A → #FFE0A3` | 32 | 5px | 58% | .40/.30 | 8192 | 关 | 深 |
| 12 | 日落磁带 SUNSET TAPE | 复古 | `#E8574A → #F7B267` | 52 | 3px | 80% | .60/.18 | 4096 | 开 | 深浅 |
| 13 | 深海 DEEP SEA | 自然 | `#0A4D68 → #05C3DD` | 64 | 3px | 68% | .32/.30 | 8192 | 开 | 深 |
| 14 | 冰川 GLACIER | 自然 | `#7FD8E8 → #E8F7FA` | 48 | 3px | 62% | .30/.32 | 4096 | 关 | 深 |
| 15 | 薄雾紫 MIST LILAC | 自然 | `#A78BFA → #E9D5FF` | 56 | 3px | 60% | .30/.30 | 4096 | 关 | 深 |
| 16 | 林地 FOREST | 自然 | `#1B7F5A → #8FD694` | 60 | 3px | 66% | .34/.28 | 8192 | 开 | 深浅 |
| 17 | 朱印 VERMILION | 实验 | `#B4261D → #FF6B5E` | 64 | 2px | 82% | .68/.14 | 4096 | 开 | 深浅 |
| 18 | 热成像 THERMAL | 实验 | `#1B3A8F → #FF8A00` | 88 | 2px | 88% | .75/.12 | 4096 | 开 | 深 |
| 19 | 薄荷静电 MINT STATIC | 实验 | `#8AD9BE → #137A5B` | 72 | 2px | 70% | .62/.16 | 4096 | 开 | 浅 |
| 20 | 鎏金 GOLD LEAF | 实验 | `#6B4E16 → #FFD700` | 48 | 4px | 85% | .58/.20 | 8192 | 开 | 深 |

> 间隙列为 `round(bar_gap_ratio × 10)` 的渲染值；完整精确值（含 `db_floor`、`sensitivity`）见 `presets.json`。

### 6.3 性格分层

| 族 | 起音 / 衰减 | 视觉性格 | 适合 |
|---|---|---|---|
| 霓虹赛博 | 快 / 极快（.72~.85 / .09~.11） | 脆、炸、跟鼓点 | 电音、游戏、想要「炫酷」 |
| 单色克制 | 慢 / 中（.35~.48 / .20~.28） | 稳、不抢图标 | 长时间办公、浅色主题 |
| 暖色复古 | 中 / 慢（.40~.60 / .18~.30） | 像指针一样摆动 | 爵士、老歌、深夜 |
| 自然冷色 | 极慢 / 极慢（.30~.34 / .28~.32） | 呼吸感、不刺眼 | 背景音乐、专注工作 |
| 实验款 | 偏快 | 强记忆点 | 想换心情的时候 |

### 6.4 数据结构（v1.1 三层分组）

`presets.json` 是**唯一数据源**；原型用 `<script type="application/json" id="presetData">` 内嵌同一份 JSON，运行时解析，不再维护第二份手写数组（`verify_design.py` 会做深度相等断言）。每套分三组：

| 组 | 键数 | 含义 | 落地代价 |
|---|---|---|---|
| `core` | 10 | 外观 + 分析参数，现有 GUI 的 `fill()` / `collect()` 已在读写（`lib.rs` L423~L498） | **零改动即生效** |
| `enhance` | 2 | `visual.peak_cap`（Live）+ `analyze.peak_hold_ms`（LiveRebuild），schema 已实现、GUI 未暴露 | GUI 补 2 个控件 |
| `motion` | 2 | `analyze.peak_fall` / `analyze.peak_gravity`，**待新增** | 需改内核，见 §6.5 |

- 12 个 `key_scope` 键全为 `Live / LiveRebuild`；2 个 `pending_keys` 当前**不在** `schema.rs`。`verify_design.py` 有一条**反向门**：若将来键真的落地了却没更新脚本，会主动 FAIL 提醒迁移，而不是默默通过。
- **刻意不做**：不往 `config.toml` 加 `preset = "xxx"`。预置是「一次性展开成已有键」，不是运行时状态；自定义方案另存独立文件。这样 `CONFIG_VERSION=2`、`TEMPLATE`、版本迁移与相关单测都不用动。
- **如何再加一套**：只改 `presets.json`（原型自动读内嵌的同份数据），跑 `verify_design.py` 检查值域与滑块步长。

### 6.5 新选项：峰值滑块 · 悬浮缓落

**用户描述**：「有声音时频谱柱子顶部有个滑块，被顶飞起来又缓慢落下来，会与柱顶分离然后回落」。

**关键发现：这不是从零做。** `crates/spectrum-core/src/analyzer.rs` L219~225 已经维护 `peaks[]` + `peak_time_ms[]`：

```rust
if self.levels[i] >= self.peaks[i] {
    self.peaks[i] = self.levels[i];                 // ← 「顶飞」
    self.peak_time_ms[i] = now_ms;
} else if now_ms - self.peak_time_ms[i] > peak_hold_ms {
    self.peaks[i] = (self.peaks[i] - 0.02).max(self.levels[i]);  // ← 「悬停后缓落」+ 防穿底
}
```

`spectrum-render/src/lib.rs` L219~237 也已把 `peaks` 画成柱顶上方的横条。缺的只是：回落量 `0.02` 硬编码、没有重力曲线、没有开关、GUI 没暴露。

**交互规格**（原型「02 微调」页的控制卡）：

| 控件 | 键 | 范围 / 默认 | 效果 |
|---|---|---|---|
| 启用峰值滑块 | `visual.peak_cap` | 开 / 关 | 关掉后柱顶干净，无任何悬浮块 |
| 悬停时长 | `analyze.peak_hold_ms` | 0~3000ms，步长 20 | 顶飞后停留多久才开始落 |
| 回落速度 | `analyze.peak_fall` ★新增 | 0.002~0.05 /帧 | 越小落得越慢 |
| 重力加速 | `analyze.peak_gravity` ★新增 | 开 / 关 | 开 = 从静止起加速、越落越快；关 = 匀速（VU 表感） |

**原型物理**（与 Rust 语义对齐）：`caps[i]` 被 `levels[i]` 顶起时上冲 `+0.025` 形成「飞」，重置悬停计时；超时后按匀速或重力积分下落；每帧 `caps[i] = max(caps[i], levels[i])` 防穿底。柱体落下去而滑块还悬着，**空隙自然出现** —— 正是用户要的「分离」。滑块用与柱体不同的亮/暗色绘制，进一步强化分离观感。预览默认 46.9fps，可切 **慢放 0.35×** 看清三段过程。

**20 套的动态性格**：霓虹 420ms / 0.010 / 重力（弹）；单色 1000ms / 0.006 / 匀速；复古 1100ms / 0.006 / 匀速；自然 1400ms / 0.004 / 匀速（落得最慢）；实验 600ms / 0.014 / 重力。

**★ 新增 2 键的落地范围**（比 v1.0 扩大的部分，逐文件清单见 §8）：`AnalyzerConfig` 加 2 字段并校验值域；`schema.rs` 的 `KEYS` / `TEMPLATE` / `encode` 各加 2 行；`analyzer.rs` L225 把 `0.02` 换为参数并加重力分支；补单测。**不改 `CONFIG_VERSION`**（新增键有默认值，旧配置照常读，走 `Extra` 保留机制）。

### 6.6 新功能：命名保存「我的方案」

**保存范围**：外观 10 键 + 动态/滑块 4 值 = **14 个值**，一次存整套，不出现「配色回来了但跳动没回来」。

| 环节 | 原型 | 产品落地 |
|---|---|---|
| 存储 | `localStorage` 键 `deepin-spectrum.user-presets.v1` | 独立文件 `~/.config/deepin-spectrum/user-presets.toml`，复用现有手写 TOML 编解码与 `encode_key` 引号规则 |
| 命名 | ≤24 字、非空、拒尖括号/引号/控制字符，同名**更新而非新增** | 同左；方案名作段名时一律经 `encode_key` 转义 |
| 上限 | 30 套，超出明确提示 | 同左，不静默丢弃 |
| 列表 | 色带 + 名称 + 参数摘要 + 选用 / 删除 | 同左 |
| 失败 | 本地存储不可用时**回滚数组并明确报错**，不留「看起来成功」的假状态 | 写盘失败同样回滚内存状态并提示 |

**交互原则**：
- 保存后该方案立即成为「使用中」，但**原参数不变**；之后手动改任一参数 → 标记「自定义」，已存方案不受影响。
- 删除正在使用的方案不会偷偷换回默认预置，只把当前状态标记为「自定义」。
- 空状态给明确引导语，不是一片空白。
- 自定义方案与 20 套内置预置**分区展示**，不混进同一网格，避免编号与语义混乱。

---

## 7. 验证结果

```
$ python3 docs/design/verify_design.py
断言总数：848
预置方案：20 套 / 5 个风格族
热生效键：12 个 · 待落地键：2 个（反向门已检查）
动态性格：悬停 [420, 600, 1000, 1100, 1400] ms · 回落 [0.004, 0.006, 0.01, 0.014] /帧
柱数范围：32 ~ 96        （合法 16~128）
FFT 档位：[4096, 8192]   （避开 F56 的 2048）
PASS：设计方案、单一数据源、动态参数与代码约束全部一致
```

校验项：① JSON 结构 / ID 唯一 / 默认唯一；② `key_scope` 的键能在 `schema.rs` 查到且为热生效，`pending_keys` 的键**确实还没**落地（反向门）；③ 值域对照 `MAX_BANDS=128` / `MAX_FFT_SIZE=8192` / `Config::validate`，颜色正则、FFT 幂次、**悬停与回落值必须是滑块步长的整数倍**；④ HTML 内嵌 JSON 与 `presets.json` 深度相等，且无残留手写数组；⑤ 动画 / 滑块 / 我的方案相关控件与 JS 引用 id 全部存在；⑥ 能力说明与存储规格齐备。另跑 `node --check`（JS 语法）与 HTML 标签闭合检查，均 PASS。

**方法论记录（沿用项目「验证脚本会假 PASS/假 FAIL」的教训）**：
- v1.0 修掉两处脚本自身缺陷：内联对象 `id` 带 JS 引号没剥离 → 20 套全被误判「缺失」；颜色字段被 `float()` 比较 → `ValueError`。
- v1.1 直接**消除了那类问题**：改成单一 JSON 数据源，用深度相等断言替代逐字段解析，不再有第二份副本可 drift。
- v1.1 新增断言抓到一个真实隐患：霓虹族悬停 420ms 不是滑块步长 50 的倍数，界面会静默夹紧成 400 或 450，与配置值不一致 → 步长改为 20 并加断言锁死。

---

## 8. 逐文件影响分析（落地时）

| 文件 | 改动要点 | 风险 | 阻塞 |
|---|---|---|---|
| `crates/spectrum-settings-gui/slint/gui.slint` | 左导航 + 刊头 + **动画预览区（B 档）** + 4 个 `panel`；新增 `preset_index` / `custom` / `peak_cap` / `peak_hold_ms` / `peak_fall` / `peak_gravity` / 预览 `Image` / 我的方案列表属性 | Slint 1.18 API 不能凭记忆写（M69~M73）；`Window` 不能同时给 `height` 与 `min-height` | 是 |
| `crates/spectrum-settings-gui/src/lib.rs` | `apply_preset()`、`custom` 标记、6 个新属性的 fill/collect/sync_back、**动画帧生成**（`SharedPixelBuffer` → `Image::from_rgba8` + Timer）、我的方案 CRUD、TOML 片段导出 | R1~R3、R8、R10 | 是 |
| 新增 `crates/spectrum-settings/src/presets.rs` | 20 套 Rust 常量表 + 用户方案读写；放 `spectrum-settings` 便于 `settings --list-presets` 复用，**保持零外部依赖** | 与 JSON 分叉 → `verify_design.py` 守门 | 是 |
| **`crates/spectrum-core/src/analyzer.rs`** | **L225 的 `- 0.02` 换成 `cfg.peak_fall`，并加重力积分分支**；`AnalyzerConfig` 加 2 字段 + `validate` 值域；补「重力先慢后快」「关闭滑块不产峰」单测 | **唯一的内核行为改动**，见 R9 | 是 |
| `crates/spectrum-settings/src/schema.rs` | `KEYS` / `TEMPLATE` / `encode` / `apply_key` 各加 2 个键；`--explain` 补说明 | 漏一处即静默失效 | 是 |
| `crates/spectrum-core/src/config.rs` | **不改**：不动 `CONFIG_VERSION=2`、不动 `TEMPLATE` 迁移逻辑 | — | 否 |
| `crates/spectrum-render/**` | **不改**：滑块绘制通道 `render_with_peaks()` 已存在，只是输入值受新参数影响 | — | 否 |
| `spectrum-platform/**`、`spectrum-daemon/**` | **不改**：热重载链路完全复用 | — | 否 |
| 新增 `~/.config/deepin-spectrum/user-presets.toml` | 用户方案的独立存储；复用 `toml.rs` / `encode_key` | 不写进主配置，避免擦掉用户手写键 | 是 |
| `README.md` / `debian/changelog` / 版本号 | bump 至 **v0.3.0** | 版本不同步 | 否 |

### 风险登记

| # | 风险 | 缓解 |
|---|---|---|
| R1 | 展开 10 个键会连续触发 `changed` → `config_changed` | 现有 `ConfigWriter` 已有 300ms 合并窗口，一次点击只落盘一次；需加断言验证只写一次 |
| R2 | `fill()` 会触发 `changed`，打开界面就写盘 | 必须复用现有 `suppress`（`lib.rs` L84）+ 400ms 单次 Timer（`L407~415`），预置应用前后都要抑制/放行 |
| **R3** | 新增 `peak_cap` 属性但 `collect()` 忘了写 → 预置里的峰值帽被旧值覆盖，**静默失效** | 把 `peak_cap` 纳入 fill/collect/sync_back 三处并补单测；未接入 enhance 时，界面必须不显示该选项而不是显示一个无效的 |
| R4 | 20 套数据在 Rust 常量与 JSON 之间分叉 | `verify_design.py` 纳入 CI；建议由 JSON 生成 Rust 常量，避免手写 |
| R5 | Slint 1.18 无内置取色器 / Canvas | 预览用静态 `Rectangle` `for` 循环（P0 方案，零 CPU）；颜色沿用现有「色块 + `#RRGGBB` 文本」折中（`gui.slint` L11~12），原型里的原生取色器仅演示 |
| R6 | `fft_size=8192` 会重建分析器、延迟略增 | 真机量测；在预置卡片标注「更细腻」，不承诺零延迟 |
| R7 | 高亮色（雾白/冰川/薄雾紫）在浅色任务栏上可能看不清 | 数据已有 `best_on` 字段；落地时按系统深浅主题把不适配的卡片排序靠后并加提示，不禁用 |
| **R8** | B 档动画预览持续占 CPU，可能冲击项目 <1.5% 的承诺 | 预览**只在设置窗口打开时**跑；用 200ms（5fps）Timer 或按窗口可见性暂停；真机量测窗口打开时的 CPU 增量，超阈值自动降帧 |
| **R9** | 改 `analyzer.rs` 峰值衰减会影响**所有运行中的频谱**，不只预览 | 新参数默认值取 `peak_fall=0.02` / `peak_gravity=false`，保证旧配置逐帧行为与现版本一致；补回归单测对比旧公式输出 |
| **R10** | 2 个新键漏在 `apply_key` 或 `encode` → 改了没反应，或被回写悄悄擦掉 | 复用 `every_key_in_registry_decodes_without_warning` 的反向门逐键断言；`verify_design.py` 的 pending 反向门会在落地后主动提醒迁移 |
| **R11** | 用户方案名含特殊字符导致 TOML 段名注入 / 界面 XSS | 段名一律经 `encode_key` quote；原型侧已拒尖括号、引号、控制字符并做 HTML 转义 |

### 预览实现的三档方案（B 档已定）

| 档 | 做法 | CPU | 状态 |
|---|---|---|---|
| A | Slint 静态 `Rectangle` 示意柱，随预置变色/变柱数，不动画 | ≈0 | 未采用 |
| **B** | Rust 侧 `SharedPixelBuffer` → `Image::from_rgba8`，Timer 刷新模拟帧（原型即其 canvas 版，46.9fps） | 低 | ✅ **用户已拍板采用** |
| C | 接 daemon 真实电平（D-Bus 新增信号） | 中 | P2 可选 |

> B 档的 CPU 预算：预览是**模拟帧**、不接真实音频，且只在设置窗口可见时运行；`slint::Timer` 建议 200ms 刷新（5fps）即可表达「跳动 + 滑块缓落」，比原型的 46.9fps 更省。原型保留 46.9fps 与慢放开关是为了让你看清物理，不代表产品必须跑满帧。

---

## 9. 分阶段落地与授权点

| 阶段 | 内容 | 状态 |
|---|---|---|
| **P0（v1.0）** | 设计上下文 `.impeccable.md`、方案、20 套 `presets.json`、可交互原型、断言自检 | ✅ 已交付 |
| **P0.1（v1.1 本轮）** | 动画预览（B 档）、峰值滑块选项与物理、命名保存「我的方案」、单一数据源重构、自检升级 | ✅ **已交付，未改产品代码** |
| **P1** | 内核 + 配置层 + 20 套预置 + 峰值滑块 2 键 + 我的方案独立存储 + CLI + GUI 功能接入 + v0.3.0 | ✅ **已落地（§11）** |
| **P1.5** | 杂志排版视觉重构 + B 档动画预览 + 中英文切换 + 色卡网格 | ✅ **已落地 v0.4.0（§12）** |
| **P2（可选）** | 真实电平预览、按主题智能推荐、方案导入导出、i18n 英文文案 | ⏳ 未排期 |

**P1 验收门**：
1. `verify_design.py` PASS；
2. `cargo fmt --all --check` / `clippy -D warnings` / `test` / `RUSTDOCFLAGS=-D warnings cargo doc` 全绿，GUI 独立 workspace 同样全绿；
3. 真机点任意预置 → ≤250ms 任务栏变化，**窗口 id 不变、采集不中断**；
4. 一次点击只落盘一次；用户手写的未知键回写后仍保留（`encode_with_extras` 行为不回归）；
5. 手动改参数后「自定义」标记正确，界面不出现未实现的键；
6. 深浅主题与 175% 分数缩放无错位；键盘可达全部功能；
7. **峰值滑块**：慢放下可分辨「顶飞 → 悬停 → 分离缓落」三段；关闭后无悬浮块；`peak_fall` / `peak_gravity` 改完热生效；
8. **回归**：默认参数下峰值逐帧结果与旧 `-0.02` 公式一致（单测断言），不破坏既有频谱观感；
9. **我的方案**：命名保存 → 重开设置窗口仍在 → 选用后 14 个值全部还原 → 删除后不留假状态；写盘失败会回滚并提示；
10. **动画预览**：设置窗口打开时的 CPU 增量实测不越过预算（R8）。

P1（§11）与 P1.5（§12）均已完成。**仍需真机观感验证**：杂志排版在 175% 分数缩放下的对齐、深色主题对比度、动画预览打开时的 CPU 增量；这些只能靠截图与实测，沙箱内无法替代。

---

## 10. 交付文件

| 文件 | 说明 |
|---|---|
| `docs/design/spectrum-ui-prototype.html` | 单文件离线原型：20 套预置实时切换、**46.9fps 动画预览 + 慢放 0.35×**、峰值滑块顶飞缓落的开关与参数、4 分区导航、深浅双主题、**命名保存我的方案（选用/删除）**、TOML 导出 |
| `docs/design/presets.json` | 20 套精确参数（`core` 10 / `enhance` 2 / `motion` 2 三层）+ 能力说明 + 存储规格；**也是原型内嵌的唯一数据源** |
| `docs/design/UI设计方案.md` | 本文档 v1.1 |
| `docs/design/verify_design.py` | 断言式自检（**848 条**），含待落地键反向门，可纳入 CI |
| `.impeccable.md` | 项目设计上下文，供后续设计工作复用 |

---

## 11. P1 实施记录（v0.3.0 · 2026-11-15）

### 11.1 逐文件改动摘要

| 文件 | 改动要点 |
|---|---|
| `crates/spectrum-core/src/lib.rs` | 新增 `CoreError::InvalidPeakFall(f32)` 与文案 |
| `crates/spectrum-core/src/analyzer.rs` | `AnalyzerConfig` 加 `peak_fall` / `peak_gravity`；`validate()` 校验 `0.002..=0.05`；`Analyzer` 加 `peak_vel[]`；把硬编码 `- 0.02` 换为参数并加重力积分分支（47 帧 ≈ 1 秒）；`reset()` 清速度；补 5 条单测（含默认值与旧行为等价） |
| `crates/spectrum-core/src/config.rs` | `Config::validate()` 先单独钳制 `peak_fall`，避免一个键越界把整段 analyze 回退默认；补回归单测 |
| `crates/spectrum-settings/src/schema.rs` | `KEYS` / `TEMPLATE` / `apply_key` / `encode` 各补 2 键；`--explain` 自动覆盖 |
| `crates/spectrum-settings/src/apply.rs` | `diff()` 补 2 键（**关键**：漏了会热重载静默失效），加反向断言 |
| `crates/spectrum-settings/src/presets.rs`（新增） | 20 套 `Preset` 常量（由 `presets.json` 生成）+ `apply_preset()`；`UserPreset` / `UserPresetStore`（list / get / save / delete，独立文件、原子写、名称校验、id 白名单防路径遍历）；6 条单测 |
| `crates/spectrum-settings/src/cli.rs` | `--list-presets` / `--apply-preset` / `--save-preset` / `--delete-preset`；应用时经 `encode_with_extras` 保留用户手写未知键；2 条新单测 |
| `crates/spectrum-settings-gui/slint/gui.slint` | 顶部「预置方案」下拉 + 命名保存/删除；外观区「峰值滑块 · 悬浮缓落」4 个控件；对应属性与 `changed` 绑定 |
| `crates/spectrum-settings-gui/src/presets_ui.rs`（新增） | 20 套 + 我的方案拍平成 ComboBox 模型（★ 前缀区分）；3 条单测 |
| `crates/spectrum-settings-gui/src/lib.rs` | `fill` / `collect` / `sync_back` 补齐 4 个新字段（**R3 关键**）；预置选用、命名保存、删除三个回调；复用现有 `suppress` 防止一次选择写两次盘 |
| `Cargo.toml` × 7 / `debian/changelog` / `README.md` | 版本 bump 至 **0.3.0**，补 CLI 与功能说明、已知限制 |
| `docs/design/*` | `presets.json` 把 2 个 `pending_keys` 迁进 `key_scope`；原型内嵌 JSON 同步；`verify_design.py` 反向门转为正向落地断言 |

### 11.2 实测结果

```
spectrum-core      68 tests PASS
spectrum-settings  84+ tests PASS（含 20 套逐套应用无告警）
GUI workspace      15 tests PASS / clippy -D warnings PASS
CLI 端到端         --list-presets 列出 20 套；--apply-preset neon-run 落盘
                   color_start/bands/peak_cap/peak_hold_ms/peak_fall/peak_gravity；
                   --save-preset 生成独立文件；--apply-preset user-xxx 还原整套
verify_design.py   859 条断言 PASS（含新键已进入内核/差分/schema/GUI）
```

端到端用隔离的 `HOME` / `XDG_CONFIG_HOME` 跑，**未创建或改动真实用户配置目录**。

### 11.3 刻意留到 P1.5 的部分

| 项 | 为什么本轮不做 |
|---|---|
| 杂志排版视觉重构（4 分区、刊头、色卡网格） | 需要重写 500+ 行 Slint 布局，且版式必须靠真机截图迭代；硬塞进本轮容易出现"编译过了但界面错乱" |
| B 档动画预览（Slint 自绘） | 需要 `SharedPixelBuffer` → `Image::from_rgba8` + Timer，并实测设置窗口打开时的 CPU 增量（R8）；建议 200ms/5fps，仅在窗口可见时运行 |
| 预置色卡网格替代下拉框 | 下拉框已能完整表达 20 套 + 我的方案，功能无缺口；网格是视觉层优化 |

**界面的功能目标已全部达成**：用户可以一键选风格、调峰值滑块、命名保存并随时选回。

---

## 12. P1.5 实施记录（v0.4.0 · 2026-11-15）

### 12.1 三项交付

| 项 | 落地方式 |
|---|---|
| 杂志排版视觉重构 | `gui.slint` 重写：左栏编号目录（01 预置 / 02 微调 / 03 动态 / 04 运行）+ 刊头巨型标题 + 粗规则线 + 4 分区条件渲染；20 张色卡用 `GridLayout` + `for card[idx]` 生成 4 列网格，点击回调传 index |
| B 档动画预览 | 新增 `preview.rs`：`PreviewStyle::from_config()` → 与 `analyzer.rs` 同构的模拟物理 → `SharedPixelBuffer<Rgba8Pixel>` + `Image::from_rgba8()` → `in-out property <image>`；`slint::Timer` **200ms / 5fps**，`preview_playing` 为假时直接跳过；支持深/浅底色、暂停、慢放 0.35× |
| 中英文切换 | 新增 `i18n.rs`：`Strings` 静态双表（约 70 条）+ `Lang` 枚举 + `UiPrefs`（`~/.config/deepin-spectrum/ui.toml`，原子写、坏文件回退中文）；Slint 侧 `in-out property <Strings> s`，切换时 Rust 一次回填所有文案并重建色卡（英文模式用英文名 + 族名） |

### 12.2 逐文件改动摘要

| 文件 | 改动要点 |
|---|---|
| `slint/gui.slint` | 全量重写；新增 `Strings` / `PresetCard` / `UserCard` 三个结构；所有可翻译文案改为 `root.s.*` 绑定；导航、筛选、语言、预览控制回调 |
| `src/lib.rs` | 重写接线层：`UiState`（语言/筛选/当前方案/自定义标记）、`to_slint_strings`、`refresh_cards` / `refresh_users` / `update_now` / `render_preview`；保留原 daemon 控制与 `fill/collect/sync_back`；新增语言、色卡、我的方案、预览 Timer 回调 |
| `src/i18n.rs`（新增） | 中英双表、`Lang`、`UiPrefs`；3 条单测（表结构一致且无空文案 / 英文确实翻译过 / 偏好往返与坏文件回退） |
| `src/preview.rs`（新增） | `PreviewStyle`、`Preview`（物理 + 像素绘制）、`frame()`；5 条单测（非空帧 / 渐变方向 / 滑块分离且防穿底 / 重力前段更慢 / 关闭滑块无白像素） |
| `Cargo.toml` × 7 / changelog / README | 版本 **0.4.0**，补语言与视觉说明、更新已知限制 |
| `docs/design/verify_design.py` | 新增 P1.5 落地断言（色卡网格、Image 预览、语言回调、i18n 双表、200ms Timer） |

### 12.3 实测

```
GUI workspace   23 tests PASS / clippy -D warnings PASS / fmt PASS
主 workspace    test + test --all-features + clippy --all-features -D warnings
                + rustdoc -D warnings + fmt 全 PASS
verify_design   PASS（含 P1.5 / 打包断言）
deb 打包        v0.4.0 amd64 产出成功；spike-deb.json 11 项全 PASS
                （doctor 逐项核对 feature、关键文件清单、GPL 正文、无 maintainer 脚本）
```

### 12.4 仍需真机验证

- 175% 分数缩放下左栏与色卡网格的对齐、深色系统主题的对比度；
- 动画预览打开时设置窗口的 CPU 增量（设计预算：5fps、900×96 RGBA）；
- 中英文切换后各分区文案是否有截断（英文普遍比中文长）。

这三项依赖截图与实测，沙箱内只能保证「编译通过 + 逻辑正确」，不能替代真机观感。


---

## 13. v0.5.0：真机反馈修复、情绪分类与多色标（2026-11-15）

用户用两张真机截图反馈了 4 个问题，本轮全部落地并用**真实窗口截图**复核。

### 13.1 四项反馈与处理

| 反馈 | 处理 | 实证 |
|---|---|---|
| 中文下「选择频谱的位置」仍显示英文；选择框非常大 | `visual.mode` 标签改绑 `runtime_mode_label`，`FullDock/AboveDock` 改由 Rust 按语言回填 `mode_labels`；ComboBox 从撑满整行改为 `220×34px` | `docs/design/runtime-panel.png`：显示「显示位置 / 铺满任务栏」，框明显收窄 |
| 预置方案名称全糊掉 | 根因是 **`GridLayout` + repeater 的 `row/col` 在 Slint 1.18.1 下不生效**，卡片被压成一列且高度不足；改为 `PresetSection` 组件（标题 + `HorizontalBox` 一行 5 卡），卡片固定 148px | `docs/design/presets-panel.png`：名称清晰、一行 5 张 |
| 「我的方案」应与微调/动态相关，调完要能方便保存 | 保存条抽成 `SaveBar` 组件，放到 02 微调与 03 动态页底部；01 页保留列表并加引导语 | 两个页面共用 `save_name` 与 `save_preset()`，切换页不会丢输入 |
| 预置风格太少、想要七彩/渐变、按情绪分类 | 20 套 → **30 套 / 6 个情绪分类**；新增多色标能力（3 种铺法）、8 套多色预置；去掉只显示单族的筛选 chips，改为 6 个分区并列 | `presets.json` 单一数据源；`verify_design.py` 1394 条断言 |

### 13.2 多色标设计

原渲染器只按**高度**在 `color_bottom → color_top` 之间插值，所有柱子同高同色。v0.5.0 在 `RenderStyle` 上加 `[u32; 8]` 色板 + `PaletteMode`，取色改为 `bar_color(usable, from_bottom, index, count)`：

- `Across`：按柱索引 `index/(count-1)` 在色板上插值，柱内叠一层 72%→100% 明度，保留横向渐变的连贯；
- `PerBar`：`index` 映射到离散色标段，柱内纯色 + 明度；
- `Vertical`：按 `from_bottom/usable` 在色板上插值，柱内跨多段；
- `Off`（默认）：走原公式，**逐像素等价**，有回归单测守护。

配置用两个 Live 键：`visual.palette`（逗号分隔 3~8 个 `#RRGGBB`）与 `visual.palette_mode`。`spectrum-core` 提供无渲染依赖的 `parse_palette()`；`apply.rs` 映射到渲染枚举；`preview.rs` 用同构算法画模拟帧，所见即所得。GUI 的 `fill/collect/sync_back` 都接了这两个键——否则换预置后会被旧值覆盖，正是本项目零容忍的静默失效。

### 13.3 情绪分类

| 分类 | 气质 | 代表方案 |
|---|---|---|
| 欢快明快 | 高饱和、跳跃 | 彩虹糖（横向彩虹）、彩纸蹦迪（每柱一色）、赛博霓虹 |
| 平静专注 | 低对比、慢呼吸 | 静水、深海、冰川、林地 |
| 温暖怀旧 | 琥珀、黑胶 | 琥珀收音机（柱内暖阶）、黑胶、钨丝 |
| 忧伤低回 | 低饱和冷调、落得最慢 | 褪色彩虹、雨夜蓝、水墨 |
| 严肃仪表 | 克制、精确 | 示波绿、网格灰、VU 表、热成像 |
| 炫酷实验 | 强对比、快起音 | 光谱故障、极光脉冲、鎏金 |

各分类的滑块性格也不同：悬停 420~1600ms、回落 0.004~0.014、重力开关按族区分。

### 13.4 逐文件改动摘要

| 文件 | 要点 |
|---|---|
| `spectrum-render/src/lib.rs` | `MAX_PALETTE` / `PaletteMode` / `RenderStyle` 3 字段；`bar_color` 按柱取色；`sample_palette` 整数插值；6 条新单测含逐像素回归门 |
| `spectrum-core/src/config.rs` | `Visual` 加 2 字段 + 校验；`parse_palette()` / `is_palette_mode()`；非法色板整段回退双色 |
| `spectrum-settings/src/{schema,apply,presets,cli}.rs` | 键表/TEMPLATE/编解码/diff/plan/常量表/用户方案读写/CLI 分类全部贯通；常量表由 JSON 重生成 30 套 |
| `spectrum-settings-gui/slint/gui.slint` | 全量重写：`PresetSection` / `SaveBar` 组件、6 分区、多色控件、双语 ComboBox |
| `spectrum-settings-gui/src/{lib,i18n,preview}.rs` | 分类模型、双语回填、色标取样、多色属性 fill/collect/sync_back、预览同构取色 |
| `docs/design/*` | `presets.json` 30 套单一数据源；原型内嵌 JSON 同步；`verify_design.py` 1394 断言；两张真机截图 |
| 7 个 `Cargo.toml` / changelog / README | 0.4.0 → **0.5.0** |

### 13.5 实测

```
主 workspace   fmt / test --all-features / clippy -D warnings / rustdoc -D warnings  全 PASS
GUI workspace  22 tests / clippy -D warnings / fmt  全 PASS
verify_design  1394 条断言 PASS
GUI 冒烟       G1~G4 PASS（窗口 1960×1470，仅打开不写配置）
真机截图       01 预置页 + 04 运行页，均已人工复核
```


---

## 14. v0.5.1：「05 关于」页与许可合规（2026-11-15）

### 14.1 版式设计

延续杂志排版，避免做成「一个塞满文字的弹窗」：

```
05 / 关于本软件
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
┌────────┐   频谱工坊                 ← 34px 衬线主名
│  Logo  │   关于 / 致谢              ← 10px kicker
│ 112px  │   [ v0.5.1 ]              ← 朱红版本徽标
└────────┘   让任务栏跟着声音呼吸…
────────────────────────────────────────────
 作者      Feelup
 邮箱      testangel@163.com  ← 只读输入框，可选中复制
 开源协议  GPL-3.0-only       ← 朱红强调
 版本      0.5.1
 技术      Rust 1.92 · Slint 1.18 · PulseAudio · FFT · X11 / Wayland
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
 协议说明 / 全文位置 / 致谢          ← 10~11px 次级字
```

设计取舍：
- **Logo 放名片左侧而不是页顶居中**：与刊头「编号 + 巨型标题」的左对齐节奏一致，
  右侧留给名称与版本，视线从图 → 名 → 版本 → 简介自然下移。
- **信息表用 130px 标签列**：与 02/03 页的滑块标签列同宽，跨页对齐。
- **邮箱用只读 `LineEdit`** 而非纯文本：Slint 1.18 的 LineEdit 支持 `read-only`，
  用户能选中后 Ctrl+C，不引入剪贴板依赖或外部命令。
- **协议名用朱红强调**：它是这一页唯一需要被记住的「行动信息」。
- 作者与邮箱是专有名词，由 Rust 注入不翻译；标签全部走 `Strings` 中英双表。
- 版本号取 `env!("CARGO_PKG_VERSION")`，不手抄，避免版本漂移。

### 14.2 GPL-3.0 适用性核查

| 检查项 | 结果 |
|---|---|
| 依赖许可 | 29 种唯一值，全部为 MIT / Apache-2.0 / BSD-2,3 / Zlib / ISC / BSL-1.0 / Unicode-3.0 / CC0 / Unlicense / 0BSD / LLVM-exception / LGPL-2.1-or-later(仅作为多选之一)，**均与 GPLv3 兼容** |
| 明显不兼容项 | 无 GPL-2.0-only、LGPL-2.0-only、MPL、AGPL-1、SSPL、Commons-Clause |
| 关键约束 | Slint 1.18.1 = `GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0`，禁止 "or later" |
| 结论 | **适用，且必须写 `GPL-3.0-only`**（不能写 `GPL-3.0-or-later` 或笼统的 `GPL-3.0`） |

落地补齐了此前缺失的四处：7 个 `Cargo.toml` 的 `license` 字段、根目录
`LICENSE`（GPL-3 全文 674 行）、`debian/copyright` 的 `Upstream-Contact`
与逐文件版权、以及安装包内的 `LICENSE.GPL-3`（新增 D6 断言）。
`debian/copyright` 里 2026-11-14 的决策说明保留，作为「为什么不是 or later」的依据。

### 14.3 逐文件改动摘要

| 文件 | 要点 |
|---|---|
| `slint/gui.slint` | `Strings` 加 12 个关于字段；左栏加 05 导航；`panel_index == 4` 分区；`@image-url("logo.png")` 内嵌 Logo |
| `src/i18n.rs` | 中英双表各加 12 条，`fields()` 同步（双表一致性单测守护） |
| `src/lib.rs` | `to_slint_strings` 同步；启动注入作者 / 邮箱 / `CARGO_PKG_VERSION` |
| `slint/logo.png` | 由 `data/deepin-spectrum.png` 复制，编译进二进制 |
| `Cargo.toml` × 7 | `license = "GPL-3.0-only"`（`workspace.package` + 5 个 `license.workspace = true` + GUI 独立声明）；版本 0.5.0 → **0.5.1** |
| `LICENSE` / `debian/copyright` | GPL-3 全文、逐文件版权、作者与邮箱 |
| `scripts/build-deb.sh` | 安装 LICENSE 全文；`spike-deb.json` 新增 D6 |
| changelog / README / 本文档 | 0.5.1 条目、关于与许可章节、§14 |


---

## 15. v0.5.2：品牌统一与商店上架文案（2026-11-15）

### 15.1 名称一致性审计

用户发现「窗口叫频谱工坊，启动器却叫任务栏频谱设置」。审计后把**产品名（Name）**
与**功能描述（Comment）**分开，四处统一：

| 位置 | 改前 | 改后 |
|---|---|---|
| `data/deepin-spectrum.desktop` `Name` | 任务栏频谱设置 | **频谱工坊** |
| 同一文件 `Name[en]` | Taskbar Spectrum Settings | **Spectrum Atelier** |
| 同一文件 `Comment` / `Comment[en]` | 配置任务栏音频频谱 | 让任务栏跟着声音呼吸 · 实时音频频谱 / Live audio spectrum for the Deepin dock |
| `trayctl.rs` 自启动项 | 任务栏频谱托盘 | 频谱工坊（托盘） / Spectrum Atelier (Tray) |
| `build-deb.sh` control `Description` | Deepin 任务栏音频频谱 | 频谱工坊 - Deepin 任务栏音频频谱 |
| `data/deepin-spectrum.1` NAME | Deepin 任务栏音频频谱 | 频谱工坊，Deepin 任务栏实时音频频谱 |

顺手修正 man page 里陈旧的版本号 `0.2.6 → 0.5.2`，并加自检断言防止再次漂移。
窗口标题 `root.s.app_name` 原本就是「频谱工坊」，保持不变。

### 15.2 左上角标识

原来是 30px 朱红方块 + 白色「频」字（一个临时占位标识）。换成**与「05 关于」名片
完全相同的 `logo.png`**：34px 白底卡片 + 1px `Palette.border` 细边框，内嵌 28px
Logo，`image-fit: contain`。

设计取舍：
- 品牌资产不做反色处理，白底在纸白与墨黑主题下都由细边框提供清晰边界；
- 与关于页共用**同一份资源**，避免两处图形分叉（单一来源原则）；
- 尺寸从 30px 微调到 34px，与左栏 15px 应用名的视觉重量更平衡。

### 15.3 商店文案

`docs/store-listing.md` 提供中英双语、可直接复制的四段文案，字数经脚本核验：

| 条目 | 字符数 | 上限 |
|---|---:|---:|
| 中文一句话介绍 | 64 | 100 |
| 中文应用介绍 | 848 | 1000 |
| English tagline | 92 | 100 |
| English description | 987 | 1000 |

文案策略：第一段就回答「会不会破坏系统」；按「是什么 → 原理 → 预置 → 峰值滑块 →
自定义 → 性能 → 开源」递进；英文版为压缩到 1000 字符而重写，不是逐字直译；
不写无法验证的「零风险」承诺。文档另附分类、关键词、截图顺序与文案取舍说明。

`verify_design.py` 会重新解析文档的 4 个代码块并逐条核验字数，防止后续编辑超限。
