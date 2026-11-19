#!/usr/bin/env python3
"""scripts/probe_egl.py — §7.2.1 / F21：EGL 透明配置探测（Day1 Spike 第一项）.

用法: python3 scripts/probe_egl.py
只依赖 libEGL.so.1 + Python 标准库，不需要任何 -dev 包；不创建窗口、不修改系统状态。

判定（Day1 上午通过标准）:
    alpha>=8 matched > 0  且  TRANS_TYPE=RGB matched == 0  -> exit 0（必须用 EGL_ALPHA_SIZE=8 写法）
    否则 -> exit 1
"""
import os
import sys
from pathlib import Path

# 环境差异保护（本机实测）：AI 会话 / 容器内没有 /dev/dri，
# 直接 eglInitialize 会挂住（MESA: Failed to query drm device /
# failed to create dri2 screen），表现为脚本被 SIGTERM 而非断言失败。
# 此时自动 re-exec 到 llvmpipe 软件路径并明确标注 DEGRADED：
#   F21 的核心结论（TRANS_TYPE=RGB 匹配 0）在软件路径同样成立，
#   但 config 数量不同（硬件 100/60，llvmpipe 50/30），
#   因此 Day1 Spike 仍必须在有 /dev/dri 的真实终端复跑硬件路径。
DEGRADED = not Path("/dev/dri").exists()
if DEGRADED and os.environ.get("SPECTRUM_EGL_DEGRADED") != "1":
    env = {**os.environ, "LIBGL_ALWAYS_SOFTWARE": "1", "SPECTRUM_EGL_DEGRADED": "1"}
    os.execve(sys.executable,
              [sys.executable, os.path.abspath(__file__), *sys.argv[1:]], env)

import ctypes as C  # noqa: E402

egl = C.CDLL("libEGL.so.1")
egl.eglGetDisplay.restype = C.c_void_p
egl.eglGetDisplay.argtypes = [C.c_void_p]
egl.eglInitialize.argtypes = [C.c_void_p, C.POINTER(C.c_int), C.POINTER(C.c_int)]
egl.eglGetConfigs.argtypes = [C.c_void_p, C.c_void_p, C.c_int, C.POINTER(C.c_int)]
egl.eglChooseConfig.argtypes = [
    C.c_void_p, C.POINTER(C.c_int), C.c_void_p, C.c_int, C.POINTER(C.c_int)
]
egl.eglGetConfigAttrib.argtypes = [C.c_void_p, C.c_void_p, C.c_int, C.POINTER(C.c_int)]
egl.eglTerminate.argtypes = [C.c_void_p]

d = egl.eglGetDisplay(None)
maj, mn = C.c_int(), C.c_int()
if not d or not egl.eglInitialize(d, C.byref(maj), C.byref(mn)):
    print("FAIL: eglInitialize failed")
    sys.exit(2)

n = C.c_int()
egl.eglGetConfigs(d, None, 0, C.byref(n))
cfgs = (C.c_void_p * n.value)()
got = C.c_int()
egl.eglGetConfigs(d, cfgs, n.value, C.byref(got))

SURF, WIN, REND, ES2 = 0x3033, 0x4, 0x3040, 0x4
R, G, B, A, NONE, TRANS = 0x3024, 0x3023, 0x3022, 0x3021, 0x3038, 0x3034
BASE = [SURF, WIN, REND, ES2, R, 8, G, 8, B, 8, A, 8]


def choose(extra):
    vals = BASE + extra + [NONE]
    a = (C.c_int * len(vals))(*vals)
    out = (C.c_void_p * 256)()
    m = C.c_int()
    return egl.eglChooseConfig(d, a, out, 256, C.byref(m)), m.value


backend = "llvmpipe(DEGRADED)" if DEGRADED else "hardware"
print(f"EGL {maj.value}.{mn.value}  total configs = {got.value}  backend = {backend}")
ok_alpha, n_alpha = choose([])
ok_trans, n_trans = choose([TRANS, 0x3039])
print("alpha>=8 only            -> ok=%d matched=%d" % (ok_alpha, n_alpha))
print("+TRANS_TYPE=RGB (v1.1)   -> ok=%d matched=%d" % (ok_trans, n_trans))
egl.eglTerminate(d)

# F21：v1.1 请求 EGL_TRANSPARENT_TYPE 在本机匹配 0 个 config，
# 因此实现里绝不能出现该属性；只用 EGL_ALPHA_SIZE=8。
passed = bool(ok_alpha) and n_alpha > 0 and n_trans == 0
verdict = ("PASS(DEGRADED)" if DEGRADED else "PASS") if passed else "FAIL"
print("SPIKE_D1_EGL: %s (alpha8=%d, transparent=%d)" % (verdict, n_alpha, n_trans))
if DEGRADED:
    print("NOTE: 无 /dev/dri，走 llvmpipe；D1 请在真实终端复跑硬件路径取 100/60/0 基线")
sys.exit(0 if passed else 1)
