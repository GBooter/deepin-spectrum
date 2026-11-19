#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""Prepare deepin Store assets from Screenshout/*.png.

Rules (deepin Store):
  - App icon : jpg/png, 1:1, 96*96 - 512*512 px
  - Screenshots: jpg/png, <= 2 MB each, 3-6 images,
                 landscape 3:2 (1050*700 - 1920*1280) or portrait 9:16

Strategy:
  - Icon is redrawn geometrically (not upscaled) from the exact pixel layout of
    logo.png, so edges stay razor sharp at 512px. Verified against the original.
  - Window screenshots are 3196x1996 (~1.60:1). Center-cropping to 3:2 would
    cut the left sidebar, so they are downscaled uniformly onto a 1920x1280
    canvas with a thin (40px) neutral letterbox.
  - Desktop shots are 3196x1346 (~2.37:1). A 3:2 crop biased to the left keeps
    the whole tri-color dock spectrum bar and the main wallpaper composition.
  - Anything over the 2 MB cap falls back to high-quality JPEG.
"""
from __future__ import annotations

import json
import os
from PIL import Image, ImageDraw

ROOT = "/media/Another/Date/AI-Date/WorkBuddy-Date/deepin 任务栏音频频谱插件"
SRC_DIR = os.path.join(ROOT, "Screenshout")
OUT_DIR = os.path.join(ROOT, "store-assets")
SHOT_DIR = os.path.join(OUT_DIR, "screenshots")
ALT_DIR = os.path.join(OUT_DIR, "alternatives")
ICON_PATH = os.path.join(ROOT, "crates/spectrum-settings-gui/slint/logo.png")

TARGET_W, TARGET_H = 1920, 1280
SIZE_LIMIT = 2_000_000  # stricter than 2 MiB, deliberately

# ---- Exact geometry parsed from logo.png (256x256, transparent) ----
TOP_COLOR = (255, 43, 214)     # magenta at the top of every bar
BOTTOM_COLOR = (0, 229, 255)   # cyan at the bottom
BAR_BOTTOM = 244               # exclusive; bars occupy y .. 243
BARS = [
    (12, 152, 47),
    (60, 91, 95),
    (108, 40, 143),
    (156, 111, 191),
    (204, 173, 239),
]  # (x0, y0, x1), all inclusive, 36px wide, 12px gaps

# ---- Asset selection -------------------------------------------------------
# 6 official shots: dock in action -> presets -> fine tune -> custom scheme
# -> runtime -> English UI. Two alternates are kept out of the upload set.
SHOTS = [
    ("01-dock-live-spectrum", "截图_选择区域_20261115211216.png", "dock",
     "桌面实拍：任务栏紫→青→绿多色标频谱条"),
    ("02-preset-library", "截图_选择区域_20261115202846.png", "window",
     "中文 01 预置风格：30 套方案 / 6 类情绪色卡"),
    ("03-fine-tune", "截图_选择区域_20261115202951.png", "window",
     "中文 02 微调：彩虹频谱、多色标与参数滑块"),
    ("04-motion-save-custom", "截图_选择区域_20261115203023.png", "window",
     "中文 03 动态：动画参数与命名保存「我的方案」"),
    ("05-runtime", "截图_选择区域_20261115203130.png", "window",
     "中文 04 运行：开机自启、静音门限、显示位置"),
    ("06-english-ui", "截图_选择区域_20261115211417.png", "window",
     "英文 01 Preset Library：中英双语切换"),
]
ALTERNATES = [
    ("alt-01-dock-orange", "截图_选择区域_20261115211157.png", "dock",
     "备选：桌面实拍·橙红频谱条"),
    ("alt-02-about", "截图_选择区域_20261115203217.png", "window",
     "备选：中文 05 关于（品牌 / 作者 / GPL-3.0-only）"),
]


def lerp_color(t: float) -> tuple[int, int, int]:
    return tuple(round(TOP_COLOR[i] + (BOTTOM_COLOR[i] - TOP_COLOR[i]) * t)
                 for i in range(3))


def redraw_icon(scale: int, background=None,
                dx: int = 0, dy: int = 0) -> Image.Image:
    """Vector-accurate redraw of logo.png at `scale`x, with a continuous
    per-bar vertical gradient (smoother than the 256px original, identical
    geometry). `dx`/`dy` only re-balance the surrounding whitespace; the bar
    artwork itself is unchanged. Default (0, 0) is pixel-identical to the
    in-app Logo."""
    side = 256 * scale
    im = Image.new("RGBA", (side, side),
                   background if background is not None else (0, 0, 0, 0))
    px = im.load()
    for x0, y0, x1 in BARS:
        X0, X1 = (x0 + dx) * scale, (x1 + 1 + dx) * scale
        Y0, Y1 = (y0 + dy) * scale, (BAR_BOTTOM + dy) * scale
        span = Y1 - Y0 - 1
        for y in range(Y0, Y1):
            color = lerp_color((y - Y0) / span) + (255,)
            for x in range(X0, X1):
                px[x, y] = color
    return im


def verify_icon_redraw() -> dict:
    """Assert the redraw reproduces the original logo pixel-for-pixel."""
    orig = Image.open(ICON_PATH).convert("RGBA").load()
    made = redraw_icon(1).load()
    diffs = []
    for x0, y0, x1 in BARS:
        for y in range(y0, BAR_BOTTOM):
            for x in range(x0, x1 + 1):
                a, b = orig[x, y], made[x, y]
                diffs.append(max(abs(a[i] - b[i]) for i in range(3)))
    result = {"max_channel_diff": max(diffs),
              "avg_channel_diff": round(sum(diffs) / len(diffs), 3),
              "compared_pixels": len(diffs)}
    assert result["max_channel_diff"] <= 3, result
    return result


def fit_window(path: str, pad=(255, 255, 255)) -> Image.Image:
    """Uniform downscale + thin letterbox; never crops the sidebar."""
    im = Image.open(path).convert("RGB")
    w, h = im.size
    new_h = round(h * TARGET_W / w)
    assert new_h <= TARGET_H, (path, new_h)
    resized = im.resize((TARGET_W, new_h), Image.LANCZOS)
    canvas = Image.new("RGB", (TARGET_W, TARGET_H), pad)
    canvas.paste(resized, (0, (TARGET_H - new_h) // 2))
    return canvas


def crop_dock(path: str, x0: int = 180) -> Image.Image:
    """3:2 crop biased left so the tri-color dock spectrum bar survives."""
    im = Image.open(path).convert("RGB")
    w, h = im.size
    crop_w = round(h * 1.5)
    assert x0 + crop_w <= w, (path, x0, crop_w, w)
    return im.crop((x0, 0, x0 + crop_w, h)).resize(
        (TARGET_W, TARGET_H), Image.LANCZOS)


def save_checked(im: Image.Image, stem: str, folder: str) -> str:
    os.makedirs(folder, exist_ok=True)
    for ext in ("png", "jpg"):          # keep the run idempotent
        stale = os.path.join(folder, stem + "." + ext)
        if os.path.exists(stale):
            os.remove(stale)
    png = os.path.join(folder, stem + ".png")
    im.convert("RGB").save(png, "PNG", optimize=True)
    if os.path.getsize(png) <= SIZE_LIMIT:
        return png
    jpg = os.path.join(folder, stem + ".jpg")
    quality = 92
    while True:
        im.convert("RGB").save(jpg, "JPEG", quality=quality,
                               subsampling=0, optimize=True, progressive=True)
        if os.path.getsize(jpg) <= SIZE_LIMIT or quality <= 72:
            break
        quality -= 4
    os.remove(png)
    return jpg


def describe(path: str) -> dict:
    with Image.open(path) as im:
        w, h = im.size
        fmt = im.format
    return {"file": os.path.relpath(path, ROOT), "format": fmt,
            "width": w, "height": h, "ratio": round(w / h, 4),
            "bytes": os.path.getsize(path)}


def assert_shot(info: dict) -> None:
    assert info["format"] in ("PNG", "JPEG"), info
    assert (info["width"], info["height"]) == (TARGET_W, TARGET_H), info
    assert abs(info["ratio"] - 1.5) < 1e-6, info
    assert 1050 <= info["width"] <= 1920 and 700 <= info["height"] <= 1280, info
    assert info["bytes"] <= SIZE_LIMIT, info


def build_preview(paths: list[str], captions: list[str], out: str) -> None:
    cols, tw, th, gap, label_h = 2, 460, 307, 12, 26
    rows = (len(paths) + cols - 1) // cols
    sheet = Image.new("RGB",
                      (cols * tw + (cols + 1) * gap,
                       rows * (th + label_h) + (rows + 1) * gap),
                      (232, 234, 238))
    draw = ImageDraw.Draw(sheet)
    for i, (p, cap) in enumerate(zip(paths, captions)):
        c, r = i % cols, i // cols
        x = gap + c * (tw + gap)
        y = gap + r * (th + label_h + gap)
        with Image.open(p) as im:
            sheet.paste(im.convert("RGB").resize((tw, th), Image.LANCZOS),
                        (x, y + label_h))
        draw.text((x + 2, y + 6), f"{i + 1:02d}  {cap}", fill=(25, 28, 35))
    sheet.save(out, "PNG", optimize=True)


def main() -> None:
    os.makedirs(OUT_DIR, exist_ok=True)
    os.makedirs(SHOT_DIR, exist_ok=True)
    os.makedirs(ALT_DIR, exist_ok=True)

    icon_check = verify_icon_redraw()
    icon_paths = {
        "icon-512.png": redraw_icon(2),
        os.path.join("alternatives", "icon-256.png"): redraw_icon(1),
        os.path.join("alternatives", "icon-512-white-bg.png"):
            redraw_icon(2, (255, 255, 255, 255)),
        # Same artwork, whitespace re-balanced for a 1:1 store tile
        # (original: top 40 / bottom 12 / left 12 / right 16 ->
        #  centered: 26 / 26 / 14 / 14).
        os.path.join("alternatives", "icon-512-centered.png"):
            redraw_icon(2, None, 2, -14),
    }
    icon_info = {}
    for rel, im in icon_paths.items():
        p = os.path.join(OUT_DIR, rel)
        im.save(p, "PNG", optimize=True)
        info = describe(p)
        assert info["width"] == info["height"], info
        assert 96 <= info["width"] <= 512, info
        assert info["bytes"] <= SIZE_LIMIT, info
        icon_info[rel] = info

    results, preview_paths, preview_caps = [], [], []
    for group, folder in ((SHOTS, SHOT_DIR), (ALTERNATES, ALT_DIR)):
        for stem, src, kind, caption in group:
            src_path = os.path.join(SRC_DIR, src)
            assert os.path.exists(src_path), src_path
            im = crop_dock(src_path) if kind == "dock" else fit_window(src_path)
            out = save_checked(im, stem, folder)
            info = describe(out)
            assert_shot(info)
            info["caption"] = caption
            info["source"] = src
            info["group"] = "official" if folder == SHOT_DIR else "alternate"
            results.append(info)
            if folder == SHOT_DIR:
                preview_paths.append(out)
                preview_caps.append(caption)

    official = [r for r in results if r["group"] == "official"]
    assert 3 <= len(official) <= 6, len(official)

    preview = os.path.join(OUT_DIR, "preview.png")
    build_preview(preview_paths, preview_caps, preview)

    manifest = {
        "spec": {
            "icon": "jpg/png, 1:1, 96x96 - 512x512",
            "screenshots": "jpg/png, <=2MB each, 3-6 images, 3:2 "
                           "(1050x700 - 1920x1280)",
        },
        "target_canvas": [TARGET_W, TARGET_H],
        "size_limit_bytes": SIZE_LIMIT,
        "icon_redraw_verification": icon_check,
        "icon": icon_info,
        "screenshots": results,
        "official_count": len(official),
        "assertions": "ALL PASS",
    }
    with open(os.path.join(OUT_DIR, "manifest.json"), "w",
              encoding="utf-8") as f:
        json.dump(manifest, f, ensure_ascii=False, indent=2)

    print("icon redraw verification:", icon_check)
    for rel, info in icon_info.items():
        print(f"  ICON  {rel:42s} {info['width']}x{info['height']} "
              f"{info['bytes']:>8} B  {info['format']}")
    for info in results:
        print(f"  SHOT  [{info['group']:9s}] "
              f"{os.path.basename(info['file']):34s} "
              f"{info['width']}x{info['height']} ratio={info['ratio']} "
              f"{info['bytes']:>8} B  {info['format']}")
    print(f"  official screenshots: {len(official)} (limit 3-6)")
    print("  ALL ASSERTIONS PASS")


if __name__ == "__main__":
    main()
