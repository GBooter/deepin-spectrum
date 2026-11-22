#!/usr/bin/env bash
# 打 UOS 20 / Debian 10 兼容的 deb 包（glibc 2.28）。
#
# ## 为什么需要它
#
# 在 deepin 25（glibc 2.38）上直接编译出来的二进制，动态符号会带上
# GLIBC_2.29 ~ GLIBC_2.35（pthread_* / __libc_start_main / stat64 / hypotf 等），
# 而 UOS 20 的 glibc 只有 2.28，安装后运行必然报：
#
#     /lib/x86_64-linux-gnu/libc.so.6: version `GLIBC_2.34' not found
#
# 二进制无法"降级"老 glibc 符号，唯一可靠的做法是**在 glibc ≤ 2.28 的环境里编译**。
# 本脚本用 Debian 10（buster，glibc 2.28，与 UOS 20 同源）容器做构建，
# 复用 scripts/build-deb.sh 完成打包与自检，最后再核对产物符号上限。
#
# ## 用法
#
#     bash scripts/build-deb-uos20.sh
#
# 前置条件：
#   - 已安装 podman（deepin 25 默认可用；buildah 亦可，但本脚本用 podman）
#   - 已存在 .dev-env/rustup 与 .dev-env/cargo（即项目离线开发环境，见
#     scripts/activate-dev-env.sh / scripts/install-system-deps.sh）
#   - 首次运行会拉取 debian:buster-slim 镜像（约 27MB）
#
# 产物：
#     dist/uos20/deepin-spectrum-uos20_<版本>_amd64.deb
#     dist/uos20/deepin-spectrum-uos20_<版本>_amd64.deb.sha256
#     docs/spike-deb-uos20.json          结构化证据
#
# 只支持在 x86_64 宿主上打 amd64 包（与开发机一致），不做交叉编译。

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

CONTAINER="deepin-spectrum-uos20-build"
# 国内直连 Docker Hub 常超时，优先用镜像站；可用 IMAGE=... 覆盖。
IMAGE="${IMAGE:-docker.m.daocloud.io/library/debian:buster-slim}"
FALLBACK_IMAGE="docker.io/library/debian:buster-slim"
# 装好编译依赖后会 commit 成这个镜像，后续构建跳过 apt（省几分钟）。
BUILD_IMAGE="localhost/deepin-spectrum-build:glibc2.28"
MAX_GLIBC="2.28"
DIST_DIR="$ROOT/dist/uos20"
SPIKE_JSON="$ROOT/docs/spike-deb-uos20.json"
TOOLCHAIN="$ROOT/.dev-env/rustup/toolchains/1.92.0-x86_64-unknown-linux-gnu/bin"
CARGO_REGISTRY="$ROOT/.dev-env/cargo/registry"

die() { echo "错误：$*" >&2; exit 1; }

echo "=== deepin-spectrum UOS 20 兼容包（glibc $MAX_GLIBC） ==="

# ---- 前置检查 ---------------------------------------------------------------
[[ "$(uname -m)" == "x86_64" ]] || die "本脚本只支持 x86_64 宿主（当前 $(uname -m)）"
command -v podman >/dev/null 2>&1 || die "未找到 podman，请先安装（deepin: sudo apt install podman）"
[[ -x "$TOOLCHAIN/rustc" ]] || die "缺少离线工具链：$TOOLCHAIN（见 scripts/activate-dev-env.sh）"
[[ -d "$CARGO_REGISTRY" ]] || die "缺少离线依赖缓存：$CARGO_REGISTRY（见 scripts/activate-dev-env.sh）"

cleanup() { podman rm -f "$CONTAINER" >/dev/null 2>&1 || true; }
trap cleanup EXIT

# ---- 准备镜像 ---------------------------------------------------------------
if podman image exists "$BUILD_IMAGE" >/dev/null 2>&1; then
    RUN_IMAGE="$BUILD_IMAGE"
    echo "  构建镜像: $RUN_IMAGE（已缓存，跳过 apt）"
else
    if ! podman image exists "$IMAGE" >/dev/null 2>&1; then
        echo "== 拉取基础镜像 $IMAGE =="
        podman pull "$IMAGE" >/dev/null 2>&1 \
            || podman pull "$FALLBACK_IMAGE" >/dev/null 2>&1 \
            || die "镜像拉取失败，请检查网络或用 IMAGE=... 指定可用的 debian:buster-slim 镜像"
        podman image exists "$IMAGE" >/dev/null 2>&1 || IMAGE="$FALLBACK_IMAGE"
    fi
    RUN_IMAGE="$IMAGE"
    echo "  基础镜像: $RUN_IMAGE"
fi

# ---- 启动容器 ---------------------------------------------------------------
cleanup
podman run -d --name "$CONTAINER" --network host \
    -v "$ROOT:/src:rw" -w /src "$RUN_IMAGE" sleep infinity >/dev/null \
    || die "容器启动失败"

if [[ "$RUN_IMAGE" != "$BUILD_IMAGE" ]]; then
    # buster 已进归档，官方源在国内很慢；按华为云 → 阿里云 → archive.debian.org 依次尝试。
    # 用 http：buster-slim 初始没有 ca-certificates，https 会直接失败。
    podman exec "$CONTAINER" sh -c '
set -e
for base in http://mirrors.huaweicloud.com/debian-archive \
            http://mirrors.aliyun.com/debian-archive \
            http://archive.debian.org; do
    cat > /etc/apt/sources.list <<EOF
deb $base/debian buster main
deb $base/debian buster-updates main
deb $base/debian-security buster/updates main
EOF
    if apt-get -o Acquire::Check-Valid-Until=false update -qq >/dev/null 2>&1; then
        echo "  apt 源: $base"
        break
    fi
done
export DEBIAN_FRONTEND=noninteractive
apt-get install -y -qq -o Acquire::Check-Valid-Until=false \
    build-essential pkg-config ca-certificates dpkg python3 gzip xz-utils \
    libpulse-dev libx11-dev libxcb1-dev libxcb-shape0-dev libxcb-randr0-dev \
    libxcb-present-dev libxcb-shm0-dev libxcb-xfixes0-dev libxcb-render0-dev \
    libxcb-composite0-dev libwayland-dev libxkbcommon-dev \
    libfontconfig1-dev libfreetype6-dev >/dev/null
ldd --version | head -n1
' || die "容器内依赖安装失败"

    echo "== 缓存构建镜像（下次构建跳过 apt） =="
    podman commit "$CONTAINER" "$BUILD_IMAGE" >/dev/null \
        || echo "  警告：镜像缓存失败，不影响本次构建"
fi

# ---- 构建 + 打包 ------------------------------------------------------------
# CARGO_TARGET_DIR 指向独立目录：glibc 2.28 的目标文件不能与宿主的混用。
podman exec \
    -e CARGO_HOME=/src/.dev-env/cargo \
    -e RUSTUP_HOME=/src/.dev-env/rustup \
    -e CARGO_TARGET_DIR=/src/.dev-env/target-uos20 \
    -e CARGO_NET_OFFLINE=true \
    -e DIST_DIR=/src/dist/uos20 \
    -e SPIKE_JSON=/src/docs/spike-deb-uos20.json \
    -e PKG_NAME=deepin-spectrum-uos20 \
    -e "DEEPIN_SPECTRUM_APP_LABEL=UOS 20" \
    -w /src "$CONTAINER" bash scripts/build-deb.sh \
    || die "构建或打包失败（见上方日志）"

# ---- 核对产物 ---------------------------------------------------------------
DEB="$DIST_DIR/deepin-spectrum-uos20_$(awk -F'"' '/^\[workspace\.package\]/{f=1; next} f && /^version/{print $2; exit}' Cargo.toml)_amd64.deb"
[[ -f "$DEB" ]] || die "未找到产物：$DEB"

podman exec -e MAX_GLIBC="$MAX_GLIBC" "$CONTAINER" bash -c '
set -e
for bin in /src/.dev-env/target-uos20/release/deepin-spectrum \
           /src/.dev-env/target-uos20/release/deepin-spectrum-settings; do
    maxv=$(objdump -T "$bin" | grep -o "GLIBC_[0-9.]*" | sort -uV | tail -n1)
    echo "  $(basename "$bin"): 最高 $maxv"
    newest=$(printf "%s\n%s\n" "${maxv#GLIBC_}" "$MAX_GLIBC" | sort -V | tail -n1)
    if [[ "$newest" != "$MAX_GLIBC" ]]; then
        echo "错误：$(basename "$bin") 需要 $maxv，高于 UOS 20 的 glibc $MAX_GLIBC" >&2
        exit 1
    fi
done
' || die "产物 glibc 版本校验失败"

echo
echo "包：$DEB"
echo "证据：$SPIKE_JSON"
echo "安装：sudo apt install ./$(basename "$DEB")"
