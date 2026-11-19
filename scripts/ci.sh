#!/usr/bin/env bash
# 全量 CI 门：默认 / 全 feature / feature 子集 / GUI。
#
# 用法：
#     bash scripts/ci.sh
#     CI_OFFLINE=0 bash scripts/ci.sh   # 允许访问网络（默认离线）
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

# 有隔离工具链时优先用项目自带的，避免污染系统环境。
if [[ -f "$ROOT/scripts/activate-dev-env.sh" && -d "$ROOT/.dev-env" ]]; then
    # shellcheck disable=SC1091
    source "$ROOT/scripts/activate-dev-env.sh" >/dev/null
fi

OFFLINE=()
if [[ "${CI_OFFLINE:-1}" == "1" ]]; then
    OFFLINE=(--offline)
fi

run() {
    echo "==> $*"
    "$@"
}

run cargo fmt --all --check
run cargo test --workspace "${OFFLINE[@]}"
run cargo test --workspace --all-features "${OFFLINE[@]}"
run cargo clippy --workspace --all-features --all-targets -- -D warnings
RUSTDOCFLAGS="-D warnings" run cargo doc --workspace --no-deps --all-features

# feature 子集：单独 feature 最容易漏（x11 曾经单独编译失败）。
for features in x11 dbus wayland pulse tray fft-realfft; do
    run cargo check -q -p spectrum-daemon "${OFFLINE[@]}" --features "$features"
done

# GUI 是独立 workspace，必须单独跑。
(
    cd crates/spectrum-settings-gui
    run cargo fmt --check
    run cargo clippy --all-targets "${OFFLINE[@]}" -- -D warnings
    run cargo test "${OFFLINE[@]}"
)

echo "CI: PASS"
