#!/usr/bin/env python3
"""Halcyon 品牌素材共享模块（packaging 下的脚本复用）。

集中定义品牌色、产品名、
图标源文件路径与中文字体查找逻辑，供打包相关脚本（如 make-splash.py）复用。

品牌色（规范值）：
  - 淡米色 CREAM   #F5EEDC
  - 深青   TEAL    #0E7C86
  - 橙     ORANGE  #F4622F

中文字体：只使用本机已安装的 Windows 系统字体，找不到直接报错，
不允许回退到默认点阵字体（那会画出方块）。
"""

from __future__ import annotations

import os
from pathlib import Path

from PIL import Image, ImageFont

REPO = Path(__file__).resolve().parent.parent
MARK_SRC = REPO / "src-tauri" / "icons" / "halcyon-app.png"

PRODUCT_NAME = "Halcyon"

CREAM = (245, 238, 220)
TEAL = (14, 124, 134)
ORANGE = (244, 98, 47)
INK = (7, 20, 23)

FONT_FILES = {
    "bold": ["msyhbd.ttc", "msyh.ttc", "simhei.ttf"],
    "regular": ["msyh.ttc", "msyh.ttf", "simhei.ttf"],
}


def font_dirs() -> list[Path]:
    """中文字体的可能目录：系统字体目录 + 用户级字体目录。"""
    dirs = [
        Path(os.environ.get("WINDIR", "C:/Windows")) / "Fonts",
        Path("C:/Windows/Fonts"),
        Path.home() / "AppData" / "Local" / "Microsoft" / "Windows" / "Fonts",
    ]
    return dirs


def find_font(bold: bool = False) -> Path:
    """按候选顺序找中文字体文件；一个都找不到就报错退出。"""
    key = "bold" if bold else "regular"
    for directory in font_dirs():
        for name in FONT_FILES[key]:
            candidate = directory / name
            if candidate.exists():
                return candidate
    raise SystemExit(
        "找不到中文字体（Microsoft YaHei）。请确认 C:/Windows/Fonts/msyh.ttc 存在；"
        "本脚本要求系统中文字体，找不到时不渲染，避免输出方块。"
    )


def load_font(bold: bool = False, size: int = 40) -> ImageFont.FreeTypeFont:
    path = find_font(bold)
    try:
        return ImageFont.truetype(str(path), size)
    except OSError as exc:
        raise SystemExit("字体加载失败 " + str(path) + "：" + str(exc)) from exc


def load_mark(max_height: int) -> Image.Image:
    """读取品牌标记：裁掉透明留白，按高度等比缩放（LANCZOS）。"""
    if not MARK_SRC.exists():
        raise SystemExit("缺少品牌图标源文件：" + str(MARK_SRC))
    img = Image.open(MARK_SRC).convert("RGBA")
    box = img.getchannel("A").getbbox()
    if box:
        img = img.crop(box)
    scale = max_height / img.height
    size = (max(1, round(img.width * scale)), max_height)
    return img.resize(size, Image.LANCZOS)


def vertical_gradient(size, top, bottom) -> Image.Image:
    """竖向线性渐变（RGB）。"""
    width, height = size
    strip = Image.new("RGB", (1, height))
    for y in range(height):
        t = y / max(1, height - 1)
        strip.putpixel((0, y), tuple(round(top[i] + (bottom[i] - top[i]) * t) for i in range(3)))
    return strip.resize((width, height))


def radial_mask(size, center, radius: int, gamma: float = 2.2) -> Image.Image:
    """居中的径向渐变遮罩（L）：中心 255，边缘 0，向右下距离按 gamma 衰减。"""
    steps = 320
    small = Image.new("L", (steps, steps))
    px = small.load()
    for y in range(steps):
        for x in range(steps):
            dx = (x + 0.5) / steps * 2 - 1
            dy = (y + 0.5) / steps * 2 - 1
            dist = (dx * dx + dy * dy) ** 0.5
            px[x, y] = 0 if dist >= 1 else round(255 * (1 - dist) ** gamma)
    diameter = max(1, radius * 2)
    large = small.resize((diameter, diameter), Image.LANCZOS)
    mask = Image.new("L", size, 0)
    mask.paste(large, (round(center[0] - large.width / 2), round(center[1] - large.height / 2)))
    return mask


def draw_tracked_center(draw, center, text, font, fill, tracking: int = 0) -> None:
    """带字距的居中文字（Pillow 原生不支持 letter-spacing）。"""
    width = sum(font.getlength(ch) for ch in text) + tracking * (len(text) - 1)
    x = center[0] - width / 2
    for ch in text:
        draw.text((x, center[1]), ch, font=font, fill=fill, anchor="lm")
        x += font.getlength(ch) + tracking
