#!/usr/bin/env bash
# deepin-spectrum 隔离开发环境激活脚本
# 用法（必须 source，不能 ./ 执行）：
#     source scripts/activate-dev-env.sh
# 退出：deactivate 仅对 PATH 生效；RUSTUP_HOME/CARGO_HOME 需关闭 shell 或手动 unset。
#
# 隔离原则：rustup / cargo / Python venv 全部放在项目 .dev-env/ 下，
# 不写 ~/.cargo、不改 ~/.profile、不污染系统 Python（binary_context 运行时隔离要求）。

if [[ "${BASH_SOURCE[0]}" == "${0}" ]]; then
    echo "错误：请通过 source 执行：source scripts/activate-dev-env.sh" >&2
    exit 1
fi

SPECTRUM_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export SPECTRUM_ROOT
export RUSTUP_HOME="$SPECTRUM_ROOT/.dev-env/rustup"
export CARGO_HOME="$SPECTRUM_ROOT/.dev-env/cargo"
export VIRTUAL_ENV="$SPECTRUM_ROOT/.dev-env/venv"
export PATH="$VIRTUAL_ENV/bin:$CARGO_HOME/bin:$PATH"
unset PYTHONHOME

# F42：英文 grep 断言需要 LC_ALL=C，但【绝不能 export 到交互 shell】。
# 旧版本在此处 export LC_ALL=C，会让终端提示符里的中文路径显示成
# M-dM-;M-;M-eM-!... 之类的字节转义（用户 2026-11-14 实测反馈）。
# 所有验证脚本（check_dev_env.py / probe_dock_evidence.py / spike_d2_*.py）
# 都已在内部自行设置 ENV["LC_ALL"]="C"；bootstrap-real-terminal.sh 也只在
# 自己的子 shell 里设置。因此激活脚本无需、也不应该改动终端 locale。
#
# 自愈：若当前 shell 仍残留旧版设置的 LC_ALL=C，而 LANG 仍是 UTF-8，撤销它。
if [[ "${LC_ALL:-}" == "C" && "${LANG:-}" == *UTF-8* ]]; then
    unset LC_ALL LC_CTYPE
    echo "  locale      : 已撤销旧版遗留的 LC_ALL=C（中文显示恢复）"
fi
# 手工跑需要英文输出的命令时，用 dslc 只对这一条命令加 C locale：
#     dslc pactl list short sinks | grep RUNNING
dslc() { LC_ALL=C "$@"; }
export -f dslc 2>/dev/null || true
export RUST_BACKTRACE=1

echo "deepin-spectrum 开发环境已激活"
echo "  root        : $SPECTRUM_ROOT"
echo "  RUSTUP_HOME : $RUSTUP_HOME"
echo "  CARGO_HOME  : $CARGO_HOME"
echo "  VIRTUAL_ENV : $VIRTUAL_ENV"
if command -v rustc >/dev/null 2>&1; then
    echo "  rustc       : $(rustc --version)"
else
    echo "  rustc       : 未安装（运行 scripts/check_dev_env.py 查看缺口）"
fi
if command -v python >/dev/null 2>&1; then
    echo "  python      : $(python --version 2>&1) @ $(command -v python)"
fi
echo "  自检        : python scripts/check_dev_env.py"
echo "  验证命令    : dslc <cmd>（仅该命令用 LC_ALL=C，不污染终端中文）"
