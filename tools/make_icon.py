#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""生成程序图标：crates/ss-app/app.ico 和 assets/app-icon.png。

图标是矢量绘制的：界面主色的渐变圆角方块上放白色图形。
- 32 像素及以上：白色文件夹加蓝色放大镜，说明“搜索文件和保存位置”。
- 24 像素及以下（托盘）：只画白色放大镜，文件夹在这个尺寸下会糊成一团。

依赖：Pillow（pip install pillow）。用法：python tools/make_icon.py
"""
from pathlib import Path

from PIL import Image, ImageDraw

ROOT = Path(__file__).resolve().parent.parent
ICO = ROOT / "crates" / "ss-app" / "app.ico"
PNG = ROOT / "assets" / "app-icon.png"
SIZES = [16, 20, 24, 32, 40, 48, 64, 128, 256]
SMALL_MAX = 24

TOP = (0x2F, 0x76, 0xDC)
BOTTOM = (0x1F, 0x5F, 0xBF)
WHITE = (255, 255, 255, 255)
ACCENT = BOTTOM + (255,)
RADIUS = 0.23  # 圆角半径占边长的比例
SS = 8  # 超采样倍数


def tile(big):
    """渐变圆角方块。"""
    grad = Image.new("RGB", (1, big))
    for y in range(big):
        t = y / (big - 1)
        grad.putpixel((0, y), tuple(round(TOP[i] + (BOTTOM[i] - TOP[i]) * t) for i in range(3)))
    im = grad.resize((big, big)).convert("RGBA")
    mask = Image.new("L", (big, big), 0)
    ImageDraw.Draw(mask).rounded_rectangle([0, 0, big - 1, big - 1], radius=RADIUS * big, fill=255)
    im.putalpha(mask)
    return im


def magnifier(d, big, cx, cy, r, stroke, color):
    """放大镜：圆环加圆头手柄。坐标都是 0 到 1 的比例。"""
    px = lambda v: v * big  # noqa: E731
    d.ellipse([px(cx - r), px(cy - r), px(cx + r), px(cy + r)], outline=color, width=round(px(stroke)))
    a = 0.7071
    x0, y0 = cx + (r - stroke / 2) * a, cy + (r - stroke / 2) * a
    x1, y1 = cx + r * 1.95 * a, cy + r * 1.95 * a
    d.line([px(x0), px(y0), px(x1), px(y1)], fill=color, width=round(px(stroke * 1.1)))
    rad = px(stroke * 0.55)
    d.ellipse([px(x1) - rad, px(y1) - rad, px(x1) + rad, px(y1) + rad], fill=color)


def folder(d, big, x0, y0, x1, y1, color):
    px = lambda v: v * big  # noqa: E731
    r, tab_h = 0.05, 0.09
    d.rounded_rectangle([px(x0), px(y0), px(x0 + (x1 - x0) * 0.36), px(y0 + tab_h * 2)], radius=px(r), fill=color)
    d.rounded_rectangle([px(x0), px(y0 + tab_h), px(x1), px(y1)], radius=px(r), fill=color)


def render(size):
    big = size * SS
    im = tile(big)
    d = ImageDraw.Draw(im)
    if size <= SMALL_MAX:
        magnifier(d, big, 0.43, 0.43, 0.225, 0.105, WHITE)
    else:
        folder(d, big, 0.17, 0.24, 0.83, 0.78, WHITE)
        magnifier(d, big, 0.47, 0.54, 0.125, 0.055, ACCENT)
    return im.resize((size, size), Image.LANCZOS)


def main():
    frames = {s: render(s) for s in SIZES}
    ICO.parent.mkdir(parents=True, exist_ok=True)
    frames[256].save(
        ICO,
        format="ICO",
        sizes=[(s, s) for s in SIZES],
        append_images=[frames[s] for s in SIZES if s != 256],
    )
    PNG.parent.mkdir(parents=True, exist_ok=True)
    frames[256].save(PNG)
    print(f"wrote {ICO} ({ICO.stat().st_size} bytes) and {PNG}")


if __name__ == "__main__":
    main()
