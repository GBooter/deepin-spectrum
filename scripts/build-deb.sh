#!/usr/bin/env bash
# 打 deb 包（**不用 debhelper**）。
#
# ## 为什么不走 dpkg-buildpackage
#
# debhelper 本机没装，而 `dpkg-deb --build` 对「纯二进制包」完全够用：
# 我们**没有** maintainer 脚本（postinst 等）——
# 开机自启由用户在设置界面里手动 enable（§11.2），安装时什么都不做。
# debian/ 下的骨架留给将来走完整打包流程时用。
#
# ## 包内容
#
#   /usr/bin/deepin-spectrum                 主程序（daemon/doctor/settings/tray/icon）
#   /usr/bin/deepin-spectrum-settings        设置界面（Slint）
#   /usr/share/applications/deepin-spectrum.desktop   只打包设置界面（F29）
#   （托盘不单独放入口：由设置窗口里的开关启停）
#   /usr/share/icons/hicolor/{256x256,32x32}/apps/deepin-spectrum.png
#   /usr/share/man/man1/deepin-spectrum.1
#   /usr/lib/systemd/user/deepin-spectrum.service     用户单元（开机自启唯一入口）
#   /usr/share/doc/deepin-spectrum/copyright GPL-3.0-only
#
# 用法：
#     bash scripts/build-deb.sh
#
# 产物：
#     dist/deepin-spectrum_<版本>_amd64.deb
#     dist/deepin-spectrum_<版本>_amd64.deb.sha256
#     docs/spike-deb.json                   结构化证据

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

export RUSTUP_HOME="$ROOT/.dev-env/rustup"
export CARGO_HOME="$ROOT/.dev-env/cargo"
export PATH="$ROOT/.dev-env/rustup/toolchains/1.92.0-x86_64-unknown-linux-gnu/bin:$PATH"
export TMPDIR="$ROOT/.dev-env/tmp"
export CARGO_NET_OFFLINE=true
# 产物目录 / 证据文件可被外部覆盖：UOS 20 兼容包由 scripts/build-deb-uos20.sh
# 复用本脚本在 glibc 2.28 容器里产出，不能覆盖 deepin 25 的产物与证据。
DIST_DIR="${DIST_DIR:-$ROOT/dist}"
SPIKE_JSON="${SPIKE_JSON:-$ROOT/docs/spike-deb.json}"
# 容器内构建会把 CARGO_TARGET_DIR 指到独立目录，避免与宿主的 glibc 2.38 产物混用。
MAIN_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
GUI_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/crates/spectrum-settings-gui/target}"
mkdir -p "$TMPDIR" "$DIST_DIR" "$(dirname "$SPIKE_JSON")"

PKG_NAME="${PKG_NAME:-deepin-spectrum}"
# 发行版标签（如 UOS 20）：用于应用显示名与包描述，避免商店中的同名冲突。
APP_LABEL="${DEEPIN_SPECTRUM_APP_LABEL:-}"
# 版本单一来源：主 workspace 的 `[workspace.package] version`（GUI crate 与 changelog 同步跟它）。
PKG_VERSION="$(awk -F'"' '/^\[workspace\.package\]/{f=1; next} f && /^version/{print $2; exit}' Cargo.toml)"
[[ -n "$PKG_VERSION" ]] || { echo "无法从 Cargo.toml 读取版本号" >&2; exit 1; }
# 版本号分散在三处：主 workspace、GUI crate、debian/changelog。升级时漏改一处
# 会产出"版本对不上"的包，这里直接拒绝打包。
GUI_VERSION="$(awk -F'"' '/^version = "/{print $2; exit}' crates/spectrum-settings-gui/Cargo.toml)"
[[ "$GUI_VERSION" == "$PKG_VERSION" ]] || {
    echo "版本不一致：主工程 $PKG_VERSION，GUI $GUI_VERSION" >&2
    exit 1
}
CHANGELOG_VERSION="$(head -n1 debian/changelog | sed -n 's/^[^(]*(\([^)]*\)).*/\1/p')"
[[ "$CHANGELOG_VERSION" == "$PKG_VERSION" ]] || {
    echo "版本不一致：主工程 $PKG_VERSION，debian/changelog $CHANGELOG_VERSION" >&2
    exit 1
}
# 架构跟构建机走（ARM64 上打出来就是 arm64，而不是错标成 amd64）。
#
# 多源降级：`dpkg --print-architecture` 在受限沙箱/容器里会因无法 remount /usr
# 而失败（"failed to remount /usr: operation not permitted"），stdout 为空。
# 之前直接把空串写进 control 的 Architecture，dpkg-deb --build 才报错。
# 宁可多探几层，也不要产出「架构未知」的包；全部失败就明确拒绝打包。
detect_arch() {
    local arch=""
    arch="$(dpkg --print-architecture 2>/dev/null || true)"
    if [[ -z "$arch" ]]; then
        arch="$(dpkg-architecture -qDEB_HOST_ARCH 2>/dev/null || true)"
    fi
    if [[ -z "$arch" ]]; then
        case "$(uname -m 2>/dev/null || true)" in
            x86_64|amd64)  arch="amd64" ;;
            aarch64|arm64) arch="arm64" ;;
            armv7l|armhf)  arch="armhf" ;;
            i?86)          arch="i386" ;;
            riscv64)       arch="riscv64" ;;
            loongarch64)   arch="loong64" ;;
            ppc64le)       arch="ppc64el" ;;
            s390x)         arch="s390x" ;;
        esac
    fi
    printf '%s' "$arch"
}
PKG_ARCH="$(detect_arch)"
# 白名单：只接受已知 Debian 架构名，挡住上游报错文本或异常输出。
case "$PKG_ARCH" in
    amd64|arm64|armhf|i386|riscv64|loong64|ppc64el|s390x|all) ;;
    *)
        echo "无法探测 Debian 架构（得到：'${PKG_ARCH}'），拒绝打包" >&2
        exit 1
        ;;
esac
echo "  目标架构: $PKG_ARCH  (版本 $PKG_VERSION)"
TREE="$ROOT/.dev-env/deb-tree"
OUT="$DIST_DIR/${PKG_NAME}_${PKG_VERSION}_${PKG_ARCH}${PKG_FILE_TAG:-}.deb"

echo "== 1/4 构建两个 release 二进制 =="
# ★ 主程序必须带全 feature：不带的话 X11/Pulse/托盘/D-Bus 全都"未编译"，
#   装出去的包只能 doctor —— 这是 M74 的教训，doctor 输出里要逐项核对。
# fft-realfft：发布包用 realfft R2C 后端（文档承诺，比内置复数 FFT 快约 10×）。
cargo build --release --offline --features dbus,x11,pulse,tray,wayland,fft-realfft || exit 1
# ★ 设置界面是独立 workspace；它的 target/release 可能已被清理，
#   必须在这里重建，否则打完包才发现缺二进制。
(
    cd "$ROOT/crates/spectrum-settings-gui" || exit 1
    cargo build --release --offline || exit 1
) || exit 1
GUI_BIN="$GUI_TARGET_DIR/release/deepin-spectrum-settings"
[[ -x "$GUI_BIN" ]] || { echo "设置界面二进制缺失：$GUI_BIN" >&2; exit 1; }
MAIN_BIN="$MAIN_TARGET_DIR/release/deepin-spectrum"
[[ -x "$MAIN_BIN" ]] || { echo "主程序二进制缺失" >&2; exit 1; }

echo "== 2/4 组装包目录 =="
# rm 有 safe-delete 拦截风险，改用「换目录名」的方式保证干净。
# 移动失败必须立即退出：否则会在旧树上继续安装，打出夹带旧文件的包（AUD-039）。
rm_tree() {
    if [[ -e "$1" ]]; then
        mv "$1" "$1.old.$(date +%s%N)" || {
            echo "无法清理旧目录：$1" >&2
            exit 1
        }
    fi
}
rm_tree "$TREE"
mkdir -p "$TREE/DEBIAN" \
         "$TREE/usr/bin" \
         "$TREE/usr/share/applications" \
         "$TREE/usr/share/icons/hicolor/256x256/apps" \
         "$TREE/usr/share/icons/hicolor/32x32/apps" \
         "$TREE/usr/share/man/man1" \
         "$TREE/usr/share/doc/$PKG_NAME" \
         "$TREE/usr/lib/systemd/user"

install -m 0755 "$MAIN_BIN" "$TREE/usr/bin/deepin-spectrum"
install -m 0755 "$GUI_BIN"  "$TREE/usr/bin/deepin-spectrum-settings"
if [[ -n "$APP_LABEL" ]]; then
    # 带标签的桌面入口：Name/Comment 追加发行版标识，Exec/Icon 不变。
    sed -e "s/^Name=频谱工坊\$/Name=频谱工坊 $APP_LABEL/" \
        -e "s/^Name\\[en\\]=Spectrum Atelier\$/Name[en]=Spectrum Atelier $APP_LABEL/" \
        -e "s/^Comment=让任务栏跟着声音呼吸 · 实时音频频谱\$/Comment=让任务栏跟着声音呼吸 · 实时音频频谱（$APP_LABEL 兼容版）/" \
        -e "s/^Comment\\[en\\]=Live audio spectrum for the Deepin dock\$/Comment[en]=Live audio spectrum for the Deepin dock ($APP_LABEL compatible)/" \
        data/deepin-spectrum.desktop > "$TREE/usr/share/applications/deepin-spectrum.desktop"
    chmod 0644 "$TREE/usr/share/applications/deepin-spectrum.desktop"
else
    install -m 0644 data/deepin-spectrum.desktop "$TREE/usr/share/applications/deepin-spectrum.desktop"
fi
# 托盘不再单独占应用菜单入口：改由设置窗口里的开关启停（0.2.6）。
install -m 0644 data/deepin-spectrum.png    "$TREE/usr/share/icons/hicolor/256x256/apps/deepin-spectrum.png"
install -m 0644 data/deepin-spectrum-32.png "$TREE/usr/share/icons/hicolor/32x32/apps/deepin-spectrum.png"
install -m 0644 data/deepin-spectrum.1      "$TREE/usr/share/man/man1/deepin-spectrum.1"
install -m 0644 data/deepin-spectrum.service "$TREE/usr/lib/systemd/user/deepin-spectrum.service"
install -m 0644 debian/copyright "$TREE/usr/share/doc/$PKG_NAME/copyright"
# GPL-3 全文随包分发（debian/copyright 只引用系统 common-licenses，
# 源码/独立安装包必须自带全文才算完整履行 §4/§6）。
install -m 0644 LICENSE "$TREE/usr/share/doc/$PKG_NAME/LICENSE.GPL-3"
install -m 0644 debian/changelog "$TREE/usr/share/doc/$PKG_NAME/changelog.Debian"
gzip -n "$TREE/usr/share/doc/$PKG_NAME/changelog.Debian"

# 二进制 strip：release 已带 strip = true（主工程），GUI 再兜底一次
strip --strip-unneeded "$TREE/usr/bin/deepin-spectrum" 2>/dev/null || true
strip --strip-unneeded "$TREE/usr/bin/deepin-spectrum-settings" 2>/dev/null || true

# 动态依赖（供 Depends 字段，人工核对）
MAIN_LIBS=$(ldd "$TREE/usr/bin/deepin-spectrum" 2>/dev/null | awk '{print $1}' | sort -u | tr '\n' ' ')
GUI_LIBS=$(ldd "$TREE/usr/bin/deepin-spectrum-settings" 2>/dev/null | awk '{print $1}' | sort -u | tr '\n' ' ')
echo "  主程序动态库: $MAIN_LIBS"
echo "  界面动态库: $GUI_LIBS"

cat > "$TREE/DEBIAN/control" <<EOF
Package: $PKG_NAME
Version: $PKG_VERSION
Architecture: $PKG_ARCH
Maintainer: Deepin Spectrum Contributors <deepin-spectrum@localhost>
Depends: libc6, libfontconfig1, libfreetype6, libpulse0, libgl1, libegl1, libxkbcommon0, libxkbcommon-x11-0, libx11-6, libxcursor1, libxrandr2, libxi6, libwayland-client0
Section: utils
Priority: optional
Description: 频谱工坊${APP_LABEL:+ $APP_LABEL} - Deepin 任务栏音频频谱
 在 Deepin 任务栏上显示系统混音的实时频谱柱。
 独立常驻进程，不修改 dde-shell 源码：
 PulseAudio 采集 → FFT → 空输入区叠加表面（XShape 穿透）。
 .
 配置在 ~/.config/deepin-spectrum/config.toml，改完 ≤250ms 热生效；
 设置界面改参数、托盘启停频谱、开机自启走 systemd 用户单元。
 .
 许可：GPL-3.0-only（Slint GUI 的许可约束，见 /usr/share/doc/$PKG_NAME/copyright）
EOF

echo "== 3/4 组包 =="
dpkg-deb --root-owner-group --build "$TREE" "$OUT"
BUILD_RC=$?
[[ $BUILD_RC -eq 0 ]] || { echo "dpkg-deb 失败" >&2; exit 1; }

# 清理历史组装目录：上面的 rm_tree 会把旧树改名成 deb-tree.old.<时间戳>，
# 不清理的话每次打包都会留下 ~21MB，长期累积到几百 MB。
find "$ROOT/.dev-env" -maxdepth 1 -type d -name 'deb-tree.old.*' \
    -exec rm -rf {} + 2>/dev/null || true

echo "== 4/4 校验与证据 =="
# 从包里抽二进制与 copyright 做核验（不污染包树）；用 mktemp 避免固定路径冲突。
VERIFY_BIN="$(mktemp /tmp/deb-verify-spectrum.XXXXXX)"
VERIFY_COPYRIGHT="$(mktemp /tmp/deb-verify-copyright.XXXXXX)"
cleanup_verify() { rm -f "$VERIFY_BIN" "$VERIFY_COPYRIGHT"; }
trap cleanup_verify EXIT
dpkg-deb --fsys-tarfile "$OUT" | tar -xO ./usr/bin/deepin-spectrum > "$VERIFY_BIN" 2>/dev/null
chmod +x "$VERIFY_BIN" 2>/dev/null || true
dpkg-deb --fsys-tarfile "$OUT" | tar -xO ./usr/share/doc/$PKG_NAME/copyright > "$VERIFY_COPYRIGHT" 2>/dev/null
dpkg-deb --info "$OUT" > .dev-env/deb-info.txt 2>&1
dpkg-deb --contents "$OUT" > .dev-env/deb-contents.txt 2>&1
# 摘要里只写文件名：容器内构建时 $OUT 是 /src/dist/...，写绝对路径会让
# 宿主机上的 `sha256sum -c` 找不到文件。
( cd "$DIST_DIR" && sha256sum "$(basename "$OUT")" ) > "$OUT.sha256"
SIZE=$(stat -c%s "$OUT")
SIZE_MB=$(( SIZE / 1024 / 1024 ))
echo "  包大小: ${SIZE_MB} MB"
echo "  内容清单见 .dev-env/deb-contents.txt"

python3 - "$OUT" "$SIZE" "$MAIN_LIBS" "$GUI_LIBS" "$PKG_NAME" "$PKG_VERSION" \
    "$VERIFY_BIN" "$VERIFY_COPYRIGHT" "$SPIKE_JSON" <<'PY'
import json
import subprocess
import sys
from pathlib import Path

# M78：PKG_NAME/PKG_VERSION 是 bash 变量，heredoc 里拿不到——必须经 argv 传入。
# （上一版直接在 python 里引用，NameError 让证据脚本静默用旧 JSON，D2 假 FAIL。）
out, size, main_libs, gui_libs, pkg_name, pkg_version, verify_bin, verify_copyright, spike_json = (
    sys.argv[1:10]
)
checks = []


def check(cid, desc, ok, detail):
    checks.append({"id": cid, "desc": desc, "pass": bool(ok), "detail": detail})


info = Path(".dev-env/deb-info.txt").read_text(encoding="utf-8", errors="replace")
contents = Path(".dev-env/deb-contents.txt").read_text(encoding="utf-8", errors="replace")

check("D1", "包已生成", Path(out).is_file(), out)
# GPL-3 全文必须真的随源码与安装包分发，而不只是引用系统路径。
license_text = Path("LICENSE").read_text(encoding="utf-8", errors="replace")
check(
    "D6",
    "GPL-3 全文随包分发",
    "GNU GENERAL PUBLIC LICENSE" in license_text and "Version 3, 29 June 2007" in license_text,
    f"LICENSE 共 {len(license_text)} 字符",
)
# 版本号随 PKG_VERSION 走，别写死（M77：bump 版本后断言忘了跟 → 假 FAIL）
check("D2", "控制信息含包名与版本",
      f"Package: {pkg_name}" in info and f"Version: {pkg_version}" in info,
      info.splitlines()[0] if info else "")

# ★ M74 教训：doctor 必须逐项核对，防止"装出去只能 doctor"的空包
doctor = subprocess.run(
    [verify_bin, "doctor"], capture_output=True, text=True
).stdout
for cid, keyword, desc in [
    ("D2c", "capture          : pulse（已编译）", "采集 pulse"),
    ("D2p", "presenter        : x11（depth-32 叠加窗 + 空输入区）", "X11 上屏"),
    (
        "D2d",
        "dock follow      : dbus（Wayland 轮询 geometry；X11 订阅 FrontendWindowRectChanged）",
        "D-Bus 跟随",
    ),
    (
        "D2w",
        "presenter(wl)    : wayland（zwlr_layer_shell_v1 + SHM）",
        "Wayland 上屏",
    ),
    (
        "D2t",
        "tray             : tray（StatusNotifierItem + DBusMenu）",
        "托盘",
    ),
    (
        "D2f",
        "fft backend      : realfft-3.5.1-r2c",
        "realfft FFT 后端",
    ),
]:
    check(cid, f"doctor 确认已编译：{desc}", keyword in doctor, keyword)
required = [
    "/usr/bin/deepin-spectrum",
    "/usr/bin/deepin-spectrum-settings",
    "/usr/share/applications/deepin-spectrum.desktop",
    "/usr/lib/systemd/user/deepin-spectrum.service",
    "/usr/share/man/man1/deepin-spectrum.1",
    "/usr/share/icons/hicolor/256x256/apps/deepin-spectrum.png",
    f"/usr/share/doc/{pkg_name}/copyright",
    f"/usr/share/doc/{pkg_name}/LICENSE.GPL-3",
]
# dpkg-deb --contents 的路径形如 `./usr/bin/...`（required 里已是 /usr/bin/...）
missing = [p for p in required if f"./{p[1:]}" not in contents]
check("D3", "关键文件都在包里", not missing, f"缺失: {missing}" if missing else "全部就位")
# D4：严格断言没有 maintainer 脚本（dpkg-deb 的 info 会写 "no maintainer scripts"）。
script_entries = [p for p in ["/preinst", "/postinst", "/prerm", "/postrm"] if p in contents]
check(
    "D4",
    "deb 无 maintainer 脚本（安装时不做任何系统动作）",
    not script_entries and "maintainer scripts" not in info.lower(),
    f"maintainer scripts: {script_entries}" if script_entries else "no maintainer scripts",
)
# D5：读包内 copyright 正文，不能只看路径存在。
copyright_text = Path(verify_copyright).read_text(encoding="utf-8", errors="replace")
check(
    "D5",
    "许可文件正文是 GPL-3.0-only",
    "GPL-3.0-only" in copyright_text and "GNU General Public License version 3" in copyright_text,
    "copyright 正文包含 GPL-3.0-only",
)

report = {
    "feature": "deb-package",
    "verdict": "PASS" if all(c["pass"] for c in checks) else "FAIL",
    "package": out,
    "size_bytes": int(size),
    "main_libs": main_libs.split(),
    "gui_libs": gui_libs.split(),
    "checks": checks,
}
Path(spike_json).write_text(
    json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
)
print(json.dumps(report, ensure_ascii=False, indent=2))
sys.exit(0 if report["verdict"] == "PASS" else 1)
PY
RC=$?
echo "包：$OUT"
exit $RC
