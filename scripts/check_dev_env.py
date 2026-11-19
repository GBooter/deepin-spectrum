#!/usr/bin/env python3
"""scripts/check_dev_env.py — deepin-spectrum 开发环境只读自检.

用法:
    source scripts/activate-dev-env.sh
    python scripts/check_dev_env.py            # 人类可读报告
    python scripts/check_dev_env.py --json    # 结构化结果（可留档）

只读：不开窗口、不建 null-sink、不改壁纸、不改默认音频设备、不装包。
所有外部命令强制 LC_ALL=C（F42：zh_CN.UTF-8 会让英文 grep 静默失效）。
"""
from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DEV = ROOT / ".dev-env"
CARGO_BIN = DEV / "cargo" / "bin"
ENV = {**os.environ, "LC_ALL": "C"}

# §S0 / F118 逐个 apt-cache policy 核验过的构建依赖
BUILD_DEPS = [
    "build-essential", "pkg-config", "libx11-dev", "libxcb1-dev",
    "libxcb-shape0-dev", "libxcb-randr0-dev", "libxcb-present-dev",
    "libxcb-shm0-dev", "libxcb-xfixes0-dev", "libxcb-render0-dev",
    "libxcb-composite0-dev", "libpulse-dev", "libwayland-dev",
    "libxkbcommon-dev", "libfontconfig1-dev", "libfreetype-dev",
]
# §S0 验证工具（F105 去掉 xxd，用 od/hexdump；实测补 xauth：xvfb-run 依赖）
VERIFY_DEPS = [
    "x11-utils", "x11-xserver-utils", "xdotool", "xvfb", "xserver-xephyr",
    "xauth", "mesa-utils", "wayland-utils", "weston", "pulseaudio-utils",
    "alsa-utils", "imagemagick", "valgrind",
]
RUNTIME_LIBS = [
    "libEGL.so.1", "libGLESv2.so.2", "libX11.so.6", "libpulse.so.0",
    "libwayland-client.so.0", "libxcb.so.1",
]


def run(cmd: list[str], timeout: int = 20) -> tuple[int, str]:
    try:
        p = subprocess.run(cmd, env=ENV, capture_output=True, text=True, timeout=timeout)
        return p.returncode, (p.stdout + p.stderr).strip()
    except (OSError, subprocess.TimeoutExpired) as e:
        return 127, str(e)


def tool(path: Path, arg: str = "--version") -> dict:
    if not path.exists():
        return {"present": False}
    rc, out = run([str(path), arg])
    return {"present": True, "version": out.splitlines()[0] if out else "", "rc": rc}


def dpkg_state(pkg: str) -> bool:
    rc, out = run(["dpkg-query", "-W", "-f=${Status}", pkg])
    return rc == 0 and "install ok installed" in out


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--json", action="store_true", help="输出 JSON")
    args = ap.parse_args()

    report: dict = {"root": str(ROOT), "checks": {}, "blockers": [], "warnings": []}

    # 1. 会话与显示基线
    session = {
        "XDG_SESSION_TYPE": os.environ.get("XDG_SESSION_TYPE", ""),
        "DISPLAY": os.environ.get("DISPLAY", ""),
        "WAYLAND_DISPLAY": os.environ.get("WAYLAND_DISPLAY", ""),
        "LANG": os.environ.get("LANG", ""),
    }
    rc, xrandr = run(["xrandr", "--current"])
    session["xrandr_primary"] = next(
        (l.strip() for l in xrandr.splitlines() if "connected primary" in l), "")
    session["refresh_modes"] = [
        l.strip().split()[0] + "Hz" for l in xrandr.splitlines()
        if "3200x2000" in l or "90.00*" in l
    ][:3]
    for key, cmd in {
        "geometry_logical": ["busctl", "--user", "get-property", "org.deepin.dde.Dock1",
                             "/org/deepin/dde/Dock1", "org.deepin.dde.Dock1", "geometry"],
        "frontend_rect_physical": ["busctl", "--user", "get-property",
                                   "org.deepin.dde.daemon.Dock1",
                                   "/org/deepin/dde/daemon/Dock1",
                                   "org.deepin.dde.daemon.Dock1", "FrontendWindowRect"],
        "scale_factor": ["busctl", "--user", "call", "org.deepin.dde.XSettings1",
                         "/org/deepin/dde/XSettings1", "org.deepin.dde.XSettings1",
                         "GetScaleFactor"],
    }.items():
        rc, out = run(cmd)
        session[key] = out if rc == 0 else f"UNAVAILABLE(rc={rc})"
    drm = Path("/dev/dri")
    session["drm_devices"] = sorted(str(p) for p in drm.glob("*")) if drm.exists() else []
    report["checks"]["session"] = session
    if not session["drm_devices"]:
        report["warnings"].append(
            "当前执行环境无 /dev/dri：EGL 硬件路径不可验证，probe_egl.py 自动降级 llvmpipe；"
            "D1 Spike 必须在真实终端复跑硬件基线（100/60/0）")
    if session["XDG_SESSION_TYPE"] != "x11":
        report["warnings"].append("当前非 X11 会话；D1 的 X11 Spike 需切回 X11")

    # 2. 隔离工具链
    tc = {
        "RUSTUP_HOME": os.environ.get("RUSTUP_HOME", ""),
        "CARGO_HOME": os.environ.get("CARGO_HOME", ""),
        "rustc": tool(CARGO_BIN / "rustc"),
        "cargo": tool(CARGO_BIN / "cargo"),
        "rustfmt": tool(CARGO_BIN / "rustfmt"),
        "cargo-clippy": tool(CARGO_BIN / "cargo-clippy"),
        "pinned_channel": (ROOT / "rust-toolchain.toml").exists(),
        "system_rustc": shutil.which("rustc") or "",
    }
    report["checks"]["toolchain"] = tc
    if not (tc["rustc"]["present"] and tc["cargo"]["present"]):
        report["blockers"].append("Rust 工具链未就绪（需 .dev-env/cargo/bin 下 rustc/cargo）")
    if not tc["RUSTUP_HOME"].startswith(str(DEV)):
        report["warnings"].append("未 source scripts/activate-dev-env.sh：RUSTUP_HOME 不在隔离目录")

    # 3. Python venv
    py = DEV / "venv" / "bin" / "python"
    rc, ver = run([str(py), "-V"]) if py.exists() else (127, "")
    report["checks"]["python_venv"] = {"present": py.exists(), "version": ver.strip()}
    if not py.exists():
        report["blockers"].append("Python venv 缺失")

    # 4. Day1 探针（F21 / F93）
    probes = {}
    python = str(py) if py.exists() else sys.executable
    for name, script in (("egl", "probe_egl.py"), ("visual", "probe_visual.py")):
        rc, out = run([python, str(ROOT / "scripts" / script)], timeout=30)
        probes[name] = {"rc": rc, "pass": rc == 0, "output": out,
                        "degraded": "DEGRADED" in out}
    report["checks"]["day1_probes"] = probes
    if probes["egl"]["degraded"]:
        report["warnings"].append("F21 结论来自 llvmpipe 降级路径，需真机硬件路径复核")
    if not probes["egl"]["pass"]:
        report["blockers"].append("F21 EGL 探测未通过：D1 上午不可开工")
    if not probes["visual"]["pass"]:
        report["blockers"].append("depth-32 TrueColor visual 不可用：F1 不可验证")

    # 5. 系统依赖
    deps = {
        "build": {p: dpkg_state(p) for p in BUILD_DEPS},
        "verify": {p: dpkg_state(p) for p in VERIFY_DEPS},
    }
    missing_build = [p for p, ok in deps["build"].items() if not ok]
    deps["missing_build"] = missing_build
    deps["missing_verify"] = [p for p, ok in deps["verify"].items() if not ok]
    report["checks"]["system_packages"] = deps
    if "xauth" in deps["missing_verify"]:
        report["warnings"].append(
            "xauth 缺失：xvfb-run 会报 'xauth command not found'；"
            "可用 `Xvfb :99 -screen 0 1920x1080x32 ... -ac` 直接启动绕过，或补装 xauth")
    if missing_build:
        report["blockers"].append(
            f"缺 {len(missing_build)} 个编译 -dev 包（x11rb/Slint 无法编译），需 root 安装")
    apt_rc, apt = run(["apt-get", "-s", "install", "-y", "libxcb1-dev"], timeout=30)
    report["checks"]["apt_usable"] = {"rc": apt_rc, "note": apt.splitlines()[0] if apt else ""}
    if apt_rc != 0:
        report["warnings"].append(
            "当前执行环境 apt 不可用（deepin 25 不可变系统 / ostree 或沙箱限制）；"
            "须在真实终端以 root 执行 scripts/install-system-deps.sh --apply")

    # 6. 运行期动态库
    rc, ldc = run(["ldconfig", "-p"])
    libs = {l: (l in ldc) for l in RUNTIME_LIBS}
    report["checks"]["runtime_libs"] = libs
    if not all(libs.values()):
        report["blockers"].append("运行期动态库缺失：" + ", ".join(l for l, ok in libs.items() if not ok))

    # 7. 文档与骨架
    report["checks"]["workspace"] = {
        "Cargo.toml": (ROOT / "Cargo.toml").exists(),
        "rust-toolchain.toml": (ROOT / "rust-toolchain.toml").exists(),
        "doc_v13": (ROOT / "deepin 任务栏音频频谱插件-开发指导文档.txt").exists(),
        "crates": sorted(p.name for p in (ROOT / "crates").glob("*")),
    }

    report["verdict"] = "READY" if not report["blockers"] else "BLOCKED"

    if args.json:
        print(json.dumps(report, ensure_ascii=False, indent=2))
        return 0

    print("=== deepin-spectrum 开发环境自检 ===")
    print(f"项目根     : {ROOT}")
    print(f"结论       : {report['verdict']}")
    print(f"会话       : {session['XDG_SESSION_TYPE']}  DISPLAY={session['DISPLAY']}  LANG={session['LANG']}")
    print(f"屏幕       : {session['xrandr_primary']}")
    print(f"Dock(逻辑) : {session['geometry_logical']}")
    print(f"Dock(物理) : {session['frontend_rect_physical']}")
    print(f"缩放       : {session['scale_factor']}")
    rustc_v = tc["rustc"].get("version", "缺失")
    print(f"rustc      : {rustc_v}")
    print(f"venv       : {report['checks']['python_venv']['version'] or '缺失'}")
    egl_tag = "PASS(降级)" if probes["egl"]["degraded"] and probes["egl"]["pass"] else (
        "PASS" if probes["egl"]["pass"] else "FAIL")
    print(f"EGL(F21)   : {egl_tag}  {probes['egl']['output'].splitlines()[-2] if len(probes['egl']['output'].splitlines()) > 1 else ''}")
    print(f"             {probes['egl']['output'].splitlines()[-1] if probes['egl']['output'] else ''}")
    print(f"Visual(F93): {'PASS' if probes['visual']['pass'] else 'FAIL'}  {probes['visual']['output'].splitlines()[0] if probes['visual']['output'] else ''}")
    print(f"编译依赖   : {len(BUILD_DEPS) - len(missing_build)}/{len(BUILD_DEPS)} 已安装；缺 {len(missing_build)}")
    print(f"验证工具   : {len(VERIFY_DEPS) - len(deps['missing_verify'])}/{len(VERIFY_DEPS)} 已安装")
    print(f"运行库     : {sum(libs.values())}/{len(libs)} 可用")
    print(f"apt        : {'可用' if apt_rc == 0 else '不可用（需真实终端/授权）'}")
    print("--- 阻塞项 ---")
    for b in report["blockers"] or ["无"]:
        print("  [!] " + b)
    print("--- 提示 ---")
    for w in report["warnings"] or ["无"]:
        print("  [-] " + w)
    return 1 if report["blockers"] else 0


if __name__ == "__main__":
    sys.exit(main())
