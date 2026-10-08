#!/usr/bin/env python3
"""生成 Halcyon 的 Velopack 安装 splash。

最终版（splash.png）= 程序化图标版：深青渐变底 + 真实品牌图标 + 产品名 + slogan。
  - 图标：src-tauri/icons/halcyon-app.png（透明底，chevron 是镂空，会自动取底色）
  - 文字一律由脚本用 Pillow 绘制；中文字体走 packaging/brand-art.py 的查找与校验，
    找不到直接报错，不允许画出方块（含 Latin 混排 slogan）

备选留档（可随时重生成，不参与安装器接线）：
  - splash-dark.png  与 splash.png 同款（保留旧文件名，避免下游引用断裂）
  - splash-light.png 淡米色底 + 深青文字的同版式画面

AI 素材说明：packaging/velopack/splash-ai-dark.png 与 splash-ai-light.png 是付费生成的
艺术背景，当前未接入脚本、仅作素材留档（来源与 prompt 见 README.md）。

用法：
    python packaging/velopack/make-splash.py                  # 生成最终版 splash.png（默认）
    python packaging/velopack/make-splash.py --variant all    # 最终版 + 两版备选
    python packaging/velopack/make-splash.py --variant light  # 只重生成米色备选

依赖：Pillow（与 scripts/gen-icons.py 相同）。
"""

from __future__ import annotations

import argparse
import importlib.util
import sys
from pathlib import Path

from PIL import Image, ImageDraw

# 不写 .pyc：importlib 按路径加载品牌模块时避免在 packaging/ 下产生 __pycache__。
sys.dont_write_bytecode = True

REPO = Path(__file__).resolve().parent.parent.parent
OUT_DIR = Path(__file__).resolve().parent

CANVAS = (1760, 990)

# 最终版版式：图标居中偏上，产品名 + slogan 居中于画面下半部。
MARK_HEIGHT = 320
MARK_CENTER_Y = 348
NAME_SIZE = 108
NAME_TRACKING = 8
NAME_Y = 636
SLOGAN_SIZE = 44
SLOGAN_Y = 744
SLOGAN = "任务不掉线，Codex好伴侣"

DARK_NAME_FILL = (245, 238, 220)
DARK_SLOGAN_FILL = (150, 181, 183)
LIGHT_SLOGAN_FILL = (95, 128, 130)


def load_brand():
    """加载共享品牌模块（文件名带连字符，按路径加载）。"""
    path = REPO / "packaging" / "brand-art.py"
    if not path.exists():
        raise SystemExit("缺少共享品牌模块：" + str(path))
    spec = importlib.util.spec_from_file_location("brand_art", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


brand = load_brand()


def render_icon(dark: bool) -> Image.Image:
    """程序化图标版：渐变底 + 真实图标 + 产品名 + slogan。"""
    width, height = CANVAS
    if dark:
        bg = brand.vertical_gradient(CANVAS, (12, 40, 45), (5, 12, 15))
        glow = brand.radial_mask(CANVAS, (width // 2, 330), 760, gamma=2.4)
        glow_layer = Image.new("RGB", CANVAS, brand.TEAL)
        bg = Image.composite(Image.blend(bg, glow_layer, 0.22), bg, glow)
        name_fill = DARK_NAME_FILL
        slogan_fill = DARK_SLOGAN_FILL
    else:
        bg = brand.vertical_gradient(CANVAS, (248, 243, 232), (238, 230, 212))
        name_fill = brand.TEAL
        slogan_fill = LIGHT_SLOGAN_FILL

    canvas = bg.convert("RGBA")
    mark = brand.load_mark(MARK_HEIGHT)
    canvas.alpha_composite(mark, (width // 2 - mark.width // 2, MARK_CENTER_Y - mark.height // 2))
    draw = ImageDraw.Draw(canvas)
    name_font = brand.load_font(bold=True, size=NAME_SIZE)
    brand.draw_tracked_center(draw, (width // 2, NAME_Y), brand.PRODUCT_NAME, name_font, name_fill, NAME_TRACKING)
    slogan_font = brand.load_font(bold=False, size=SLOGAN_SIZE)
    draw.text((width // 2, SLOGAN_Y), SLOGAN, font=slogan_font, fill=slogan_fill, anchor="mm")
    return canvas.convert("RGB")


RENDERERS = {
    "icon": ("splash.png", lambda: render_icon(dark=True)),
    "dark": ("splash-dark.png", lambda: render_icon(dark=True)),
    "light": ("splash-light.png", lambda: render_icon(dark=False)),
}


def main() -> int:
    parser = argparse.ArgumentParser(description="生成 Halcyon Velopack 安装 splash")
    parser.add_argument(
        "--variant",
        choices=("icon", "dark", "light", "all"),
        default="icon",
        help="icon=最终版 splash.png（默认）；dark/light=备选留档；all=全部",
    )
    args = parser.parse_args()

    wanted = ("icon", "dark", "light") if args.variant == "all" else (args.variant,)
    for variant in wanted:
        filename, renderer = RENDERERS[variant]
        image = renderer()
        path = OUT_DIR / filename
        image.save(path, optimize=True)
        print(
            "render: "
            + path.name
            + " "
            + str(image.size[0])
            + "x"
            + str(image.size[1])
            + " "
            + image.mode
            + " "
            + str(round(path.stat().st_size / 1024))
            + " KB"
        )
    if "icon" not in wanted:
        print("note: 本次未生成最终版 splash.png（--variant " + args.variant + "）")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
