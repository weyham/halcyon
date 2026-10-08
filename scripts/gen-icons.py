#!/usr/bin/env python3
"""生成 Halcyon 图标资源（应用图标 + 托盘状态图标）。

源文件是一张透明底 PNG（由 `media-aigc` 技能经 aiyun / gpt-image-2-per-call 生成）：
  - `src-tauri/icons/halcyon-app.png`：应用图标（透明底，两个节点 + 橙色接续拱桥），
    同时作为托盘图形的基底（工单 M1：托盘与主图标统一，不再使用简化版 halcyon-tray.png）

流程：
  1. 清理 alpha：AI 输出的透明区常留极低 alpha 的噪点，直接缩到 32px 会变成一层灰雾；
  2. 裁掉透明留白，等比缩放到画布的 fill 比例并居中 → `icon.png`；
  3. 调用本地 tauri CLI 由 `icon.png` 派生 32x32.png / 128x128.png /
     128x128@2x.png / icon.ico / icon.icns / Square*.png / StoreLogo.png；
  4. 托盘图形缩到 32px，叠加右上角状态圆点（直径 = 边长 / 4，带深色衬底）；
  5. 顺带写 `ui/public/favicon.png`。

用法：
    python scripts/gen-icons.py [--skip-tauri]

依赖：Pillow。
"""

from __future__ import annotations

import argparse
import shutil
import subprocess
import sys
from pathlib import Path

from PIL import Image, ImageDraw

REPO = Path(__file__).resolve().parent.parent
ICONS = REPO / "src-tauri" / "icons"
APP_SRC = ICONS / "halcyon-app.png"
# 工单 M1：托盘图标改为主图标衍生（缩放 + 状态点叠加），不再独立设计。
TRAY_SRC = APP_SRC
BASE_PNG = ICONS / "icon.png"
FAVICON = REPO / "ui" / "public" / "favicon.png"

CANVAS = 1024
APP_FILL = 0.92
TRAY_FILL = 0.84
TRAY_SIZE = 32

# AI 输出里低于 ALPHA_FLOOR 的算背景噪点（归零），高于 ALPHA_CEIL 的算实心（拉满）。
ALPHA_FLOOR = 96
ALPHA_CEIL = 250

# 桌面端用不到、由 tauri CLI 顺带生成的产物（可随时重跑本脚本复现）。
DESKTOP_UNUSED = ["android", "ios", "64x64.png"]

# 托盘状态圆点：直径 = 边长 / 4，直角外边距 = 边长的 3%。
DOT_DIAMETER_RATIO = 1 / 4
DOT_MARGIN_RATIO = 0.03
DOT_HALO_COLOR = (6, 14, 24, 235)

STATUS_COLORS = {
    "status-green": (34, 197, 94, 255),
    "status-green-dim": (34, 197, 94, 110),
    "status-yellow": (234, 179, 8, 255),
    "status-red": (239, 68, 68, 255),
}


def clean_alpha(img):
    """收拾 AI 输出的 alpha：掐掉背景噪点，实心部分拉满，中间保留抗锯齿过渡。"""
    lut = []
    span = max(1, ALPHA_CEIL - ALPHA_FLOOR)
    for value in range(256):
        if value < ALPHA_FLOOR:
            lut.append(0)
        elif value >= ALPHA_CEIL:
            lut.append(255)
        else:
            lut.append(round((value - ALPHA_FLOOR) * 255 / span))
    out = img.convert("RGBA")
    out.putalpha(out.getchannel("A").point(lut))
    return out


def fit(img, size: int, fill: float):
    """裁掉透明留白，等比缩放到画布的 fill 比例并居中。"""
    box = img.getchannel("A").getbbox()
    trimmed = img.crop(box) if box else img
    target = max(1, int(size * fill))
    scale = min(target / trimmed.width, target / trimmed.height)
    resized = trimmed.resize(
        (max(1, round(trimmed.width * scale)), max(1, round(trimmed.height * scale))),
        Image.LANCZOS,
    )
    canvas = Image.new("RGBA", (size, size), (0, 0, 0, 0))
    canvas.paste(resized, ((size - resized.width) // 2, (size - resized.height) // 2), resized)
    return canvas


def generate_status_icons() -> None:
    base = fit(clean_alpha(Image.open(TRAY_SRC)), TRAY_SIZE, TRAY_FILL)
    for name, color in STATUS_COLORS.items():
        img = base.copy()
        draw = ImageDraw.Draw(img, "RGBA")
        radius = max(2, round(TRAY_SIZE * DOT_DIAMETER_RATIO / 2))
        margin = round(TRAY_SIZE * DOT_MARGIN_RATIO)
        cx, cy = TRAY_SIZE - radius - margin, radius + margin
        draw.ellipse(
            (cx - radius - 1, cy - radius - 1, cx + radius + 1, cy + radius + 1),
            fill=DOT_HALO_COLOR,
        )
        draw.ellipse((cx - radius, cy - radius, cx + radius, cy + radius), fill=color)
        img.save(ICONS / f"{name}.png")
        print(f"status icon: {name}.png ({TRAY_SIZE}px, dot {radius * 2}px)")


def drop_desktop_unused() -> None:
    """清掉 tauri CLI 顺带产出的移动端图标（本应用只发 Windows/macOS）。"""
    for rel in DESKTOP_UNUSED:
        target = ICONS / rel
        if target.is_dir():
            shutil.rmtree(target)
            print(f"cleanup: 删除 {rel}/")
        elif target.exists():
            target.unlink()
            print(f"cleanup: 删除 {rel}")


def tauri_cli() -> list[str] | None:
    for rel in ("ui/node_modules/.bin/tauri.cmd", "ui/node_modules/.bin/tauri"):
        path = REPO / rel
        if path.exists():
            return [str(path)]
    return None


def main() -> int:
    parser = argparse.ArgumentParser(description="生成 Halcyon 图标资源")
    parser.add_argument("--skip-tauri", action="store_true", help="跳过 tauri CLI")
    args = parser.parse_args()

    for required in (APP_SRC, TRAY_SRC):
        if not required.exists():
            raise SystemExit(f"缺少图标源文件：{required}")

    app_icon = fit(clean_alpha(Image.open(APP_SRC)), CANVAS, APP_FILL)
    app_icon.save(BASE_PNG)
    print(f"render: {APP_SRC.name} -> {BASE_PNG.name} ({CANVAS}px)")

    if not args.skip_tauri:
        cli = tauri_cli()
        if cli is None:
            raise SystemExit("找不到 tauri CLI（ui/node_modules/.bin/tauri）")
        cmd = cli + ["icon", str(BASE_PNG), "-o", str(ICONS)]
        print("tauri: " + " ".join(cmd))
        subprocess.run(cmd, check=True, cwd=REPO)

    generate_status_icons()
    drop_desktop_unused()
    fit(clean_alpha(Image.open(APP_SRC)), 256, APP_FILL).save(FAVICON)
    print(f"favicon: {FAVICON.relative_to(REPO)}")
    print("done")
    return 0


if __name__ == "__main__":
    sys.exit(main())
