#!/usr/bin/env bash
# scripts/install-system-deps.sh — §S0 系统依赖安装（幂等、默认 dry-run）
#
# 用法:
#   ./scripts/install-system-deps.sh               # 只预览，不改系统
#   ./scripts/install-system-deps.sh --apply      # 真正安装（需 root，需用户显式授权）
#
# ⚠️ 这是唯一会修改系统状态的脚本。deepin 25 是不可变系统（ostree），
#    在 AI 会话/容器内 apt 常报 "failed to remount /usr: operation not permitted"，
#    此时请在**真实终端**以 root 执行本脚本（必要时先开启开发者模式/解锁 ostree）。
#
# 已落实文档修正：
#   F41  xephyr -> xserver-xephyr；删 mangohud（仓库无候选）、sox、systemd、
#        libegl1-mesa-dev / libgles2-mesa-dev（khronos-egl 用 dynamic 运行时 dlopen）
#   F118 补齐 4 个 x11rb feature 的 -dev（shm/xfixes/render/composite）
#        + Slint 字体的 libfontconfig1-dev / libfreetype-dev
#   F118 正确包名是 libxcb-present-dev（**没有 0**）
#   F105 xxd 不装（用系统已有的 od / hexdump）
#   F47  perf/heaptrack 不装（linux-perf 6.5.10 与内核 6.18.56 错配）
#   F30  Xvfb 必须用 x32，不用 x24
#   实测补漏 2026-11-14：xvfb-run 依赖 xauth，但 §S0 清单未列它；
#        缺 xauth 时第 2 层直接报 "xvfb-run: error: xauth command not found" → 整层不可跑。
#        xtrace(F97) 仍为可选独立包，F4 方法 A 需要时单独装：apt install xtrace
set -euo pipefail

APPLY=0
for arg in "$@"; do
    case "$arg" in
        --apply) APPLY=1 ;;
        -h|--help) echo "用法: $0 [--apply]"; exit 0 ;;
        *) echo "未知参数: $arg" >&2; exit 2 ;;
    esac
done

BUILD_DEPS=(
    build-essential pkg-config
    libx11-dev libxcb1-dev libxcb-shape0-dev libxcb-randr0-dev
    libxcb-present-dev
    libxcb-shm0-dev libxcb-xfixes0-dev libxcb-render0-dev libxcb-composite0-dev
    libpulse-dev libwayland-dev libxkbcommon-dev
    libfontconfig1-dev libfreetype-dev
)
VERIFY_DEPS=(
    x11-utils x11-xserver-utils xdotool
    xvfb xserver-xephyr xauth
    mesa-utils
    wayland-utils weston
    pulseaudio-utils alsa-utils
    imagemagick valgrind
)
# 明确不装：mangohud（无候选）、sox（非必需）、systemd（核心包）、
# mesa-utils-extra（会把 9.0.0 降级到 8.4.0）、xxd（用 od）、
# libegl1-mesa-dev / libgles2-mesa-dev（dlopen 不需要）、linux-perf（内核错配）、
# grim（仅 Treeland 视觉验证时才单独装）

echo "=== deepin-spectrum 系统依赖 (§S0 / F41 / F118) ==="
echo "编译依赖 (${#BUILD_DEPS[@]}):"
printf '  %s\n' "${BUILD_DEPS[@]}"
echo "验证工具 (${#VERIFY_DEPS[@]}):"
printf '  %s\n' "${VERIFY_DEPS[@]}"

if [[ "$APPLY" -eq 0 ]]; then
    echo
    echo "[DRY-RUN] 未修改系统。确认无误后执行: $0 --apply"
    exit 0
fi

if [[ "$(id -u)" -ne 0 ]]; then
    echo "错误：需要 root 权限（或在真实终端用 sudo）" >&2
    exit 1
fi

echo "[预检] apt 是否可写系统 ..."
if ! apt-get -s install -y libxcb1-dev >/dev/null 2>&1; then
    echo "错误：apt 当前不可用（不可变系统 / ostree / 沙箱限制）。" >&2
    echo "请在真实终端以 root 重试；必要时先启用 deepin 开发者模式。" >&2
    exit 3
fi

echo "[安装] 正在安装 ..."
DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
    "${BUILD_DEPS[@]}" "${VERIFY_DEPS[@]}"
echo "[完成] 请运行: source scripts/activate-dev-env.sh && python scripts/check_dev_env.py"
