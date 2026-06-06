#!/usr/bin/env python3
"""从 assets/generated/savesearch-app-icon-concept.png 生成 crates/ss-app/app.ico。

源图是 macOS 风格的浅色圆角方块图（24bpp，无 alpha）。本脚本：
  1. 用四角背景色推出阈值，检测白色卡片的外接正方形（去掉外层浅色留白）；
  2. 裁到卡片，套圆角矩形 alpha 蒙版 → 透明圆角；
  3. 高质量(LANCZOS)缩放导出多尺寸 .ico（256 自动以 PNG 压缩存储）。

同时在 target/icon-preview.png 写一张 256 预览图便于目视核验。

依赖：Pillow（pip install pillow）。用法：python tools/make_icon.py
"""
from pathlib import Path

from PIL import Image, ImageChops, ImageDraw

ROOT = Path(__file__).resolve().parent.parent
SRC = ROOT / "assets" / "generated" / "savesearch-app-icon-concept.png"
DST = ROOT / "crates" / "ss-app" / "app.ico"
PREVIEW = ROOT / "target" / "icon-preview.png"
SIZES = [16, 20, 24, 32, 48, 64, 128, 256]

RADIUS_FRAC = 0.18   # 圆角半径占卡片边长比例（贴近 squircle）
INSET_FRAC = 0.012   # 蒙版相对卡片外缘内缩，吃掉浅色描边
SS = 4               # 蒙版超采样倍数（抗锯齿圆角）


def detect_card_bbox(img):
    """检测近白卡片的外接正方形框 (l, t, r, b)。"""
    rgb = img.convert("RGB")
    w, h = rgb.size
    s = 16
    corners = [(0, 0), (w - s, 0), (0, h - s), (w - s, h - s)]
    samples = [
        rgb.crop((x, y, x + s, y + s)).resize((1, 1), Image.LANCZOS).getpixel((0, 0))
        for (x, y) in corners
    ]
    # 各通道取最大背景值，阈值略高于它即可把背景排除、保留纯白卡片
    bg = tuple(max(c[i] for c in samples) for i in range(3))
    minv = min(max(bg) + 3, 252)

    r, g, b = rgb.split()
    thr = lambda im: im.point(lambda v: 255 if v >= minv else 0)  # noqa: E731
    mask = ImageChops.multiply(ImageChops.multiply(thr(r), thr(g)), thr(b))
    bbox = mask.getbbox()
    print(f"corner bg≈{bg}  minv={minv}  raw card_bbox={bbox}")

    if bbox is None:
        m = int(0.06 * w)
        return (m, m, w - m, h - m)
    l, t, rr, bb = bbox
    cx, cy = (l + rr) / 2.0, (t + bb) / 2.0
    half = max(rr - l, bb - t) / 2.0
    return (
        max(int(round(cx - half)), 0),
        max(int(round(cy - half)), 0),
        min(int(round(cx + half)), w),
        min(int(round(cy + half)), h),
    )


def main():
    img = Image.open(SRC).convert("RGBA")
    l, t, r, b = detect_card_bbox(img)
    card = img.crop((l, t, r, b))
    side = min(card.width, card.height)
    card = card.crop((0, 0, side, side))
    print(f"card bbox=({l},{t},{r},{b})  side={side}  src={img.size}")

    # 超采样画圆角矩形蒙版，再降回卡片尺寸 → 平滑圆角
    big = side * SS
    inset = int(side * INSET_FRAC) * SS
    radius = int(side * RADIUS_FRAC) * SS
    m = Image.new("L", (big, big), 0)
    ImageDraw.Draw(m).rounded_rectangle(
        [inset, inset, big - 1 - inset, big - 1 - inset], radius=radius, fill=255
    )
    m = m.resize((side, side), Image.LANCZOS)

    out = card.copy()
    out.putalpha(m)

    DST.parent.mkdir(parents=True, exist_ok=True)
    out.save(DST, format="ICO", sizes=[(s, s) for s in SIZES])
    print(f"wrote {DST}  sizes={SIZES}")

    PREVIEW.parent.mkdir(parents=True, exist_ok=True)
    small = out.resize((256, 256), Image.LANCZOS)
    # 预览：左浅右深双底，便于核验透明边缘与白卡可见性
    canvas = Image.new("RGBA", (560, 300), (255, 255, 255, 255))
    ImageDraw.Draw(canvas).rectangle([280, 0, 560, 300], fill=(43, 43, 43, 255))
    canvas.alpha_composite(small.resize((180, 180), Image.LANCZOS), (50, 60))
    canvas.alpha_composite(small.resize((180, 180), Image.LANCZOS), (330, 60))
    canvas.alpha_composite(out.resize((32, 32), Image.LANCZOS), (140, 250))
    canvas.alpha_composite(out.resize((32, 32), Image.LANCZOS), (420, 250))
    canvas.convert("RGB").save(PREVIEW)
    print(f"wrote preview {PREVIEW}")


if __name__ == "__main__":
    main()
