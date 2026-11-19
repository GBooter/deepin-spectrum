#!/usr/bin/env python3
"""scripts/probe_visual.py — §9.2 / F93：查询 depth-32 TrueColor visual 是否真的存在.

用法: python3 scripts/probe_visual.py
只查询、不建窗口，可在 CI 与真机重复跑；退出码才是 CI 判据（F93 修掉"只判 XOpenDisplay"的假 PASS）。

CI 用法:
    xvfb-run -a -s "-screen 0 1920x1080x32 +extension GLX +extension RENDER" \
        python3 scripts/probe_visual.py || echo "DEPTH32_UNAVAILABLE"
"""
import ctypes as C
import ctypes.util as U
import sys

lib = C.CDLL(U.find_library("X11") or "libX11.so.6")
lib.XOpenDisplay.restype = C.c_void_p
lib.XOpenDisplay.argtypes = [C.c_char_p]
lib.XDefaultScreen.restype = C.c_int
lib.XDefaultScreen.argtypes = [C.c_void_p]
lib.XGetVisualInfo.restype = C.c_void_p
lib.XGetVisualInfo.argtypes = [C.c_void_p, C.c_long, C.c_void_p, C.POINTER(C.c_int)]
lib.XFree.argtypes = [C.c_void_p]
lib.XCloseDisplay.argtypes = [C.c_void_p]


class XVisualInfo(C.Structure):  # 实测 sizeof == 64 (x86_64)
    _fields_ = [
        ("visual", C.c_void_p), ("visualid", C.c_ulong),
        ("screen", C.c_int), ("depth", C.c_int), ("class_", C.c_int),
        ("red_mask", C.c_ulong), ("green_mask", C.c_ulong),
        ("blue_mask", C.c_ulong), ("colormap_size", C.c_int),
        ("bits_per_rgb", C.c_int),
    ]


d = lib.XOpenDisplay(None)
if not d:
    print("FAIL: XOpenDisplay(None) == NULL")
    sys.exit(2)

scr = lib.XDefaultScreen(d)
tmpl = XVisualInfo(screen=scr, depth=32, class_=4)  # 4 == TrueColor
n = C.c_int(0)
# 0x4 == VisualDepthMask, 0x8 == VisualClassMask
p = lib.XGetVisualInfo(d, 0x4 | 0x8, C.byref(tmpl), C.byref(n))
print("screen =", scr, " depth32 TrueColor visuals =", n.value)
bits_ok = True
if p and n.value:
    a = (XVisualInfo * n.value).from_address(p)
    print("first visualid = 0x%x  bits_per_rgb = %d  masks = %x/%x/%x"
          % (a[0].visualid, a[0].bits_per_rgb,
             a[0].red_mask, a[0].green_mask, a[0].blue_mask))
    bits_ok = a[0].bits_per_rgb >= 8  # alpha 精度要求
    lib.XFree(p)
lib.XCloseDisplay(d)
sys.exit(0 if (n.value > 0 and bits_ok) else 1)
