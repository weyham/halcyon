# Velopack 安装 splash

Velopack 安装器没有向导，只在安装过程中显示一张 splash。本目录存放 Halcyon 的
splash 素材与生成脚本。

## 最终产物

`splash.png`（1760×990、RGB、不透明）= **程序化图标版**：

- 底：深青到近黑的竖向渐变 + 标记后方一层极淡的径向青色辉光；
- 图标：`src-tauri/icons/halcyon-app.png`（真实品牌图标；其 chevron 是透明镂空，
  会自动取底色，因此深浅底都成立）；
- 文字（全部由 `make-splash.py` 用 Pillow 绘制，不经过 AI）：
  - 产品名 `Halcyon`：粗体，淡米色 `#F5EEDC`，108px + 8px 字距；
  - slogan `任务不掉线，Codex好伴侣`：常规体，冷灰青 `#96B5B7`，44px，一行居中；
- 版式：图标居中偏上，产品名 + slogan 纵向居中于画面下半部，四周留白充足。

## 生成 / 重跑

```powershell
# 生成最终版 splash.png（默认只出这一张）
python packaging/velopack/make-splash.py

# 需要时顺带重生成两版备选
python packaging/velopack/make-splash.py --variant all
```

改文案或版式：编辑 `make-splash.py` 顶部的 `SLOGAN`、字号/位置常量后重跑；脚本输出
稳定（同输入同输出，连续两次运行 SHA-256 一致）。

## 备选留档

- `splash-light.png`：淡米色底 + 深青文字的同版式画面，可 `--variant light` 重生成；
- `splash-dark.png`：与 `splash.png` 同款（保留旧文件名，避免下游引用断裂）；
- 品牌色、字体查找、渐变/遮罩等公共逻辑在 `../brand-art.py`。

## AI 素材（未接入，不入库）

`splash-ai-dark.png` / `splash-ai-light.png` 是付费生成的艺术背景候选素材，
**当前未接入** `splash.png`；因体积较大且未使用，**不随仓库分发**，
留档在维护者的内部资料库。若要切「AI 底图 + 叠字」风格，从留档处取回即可。

| 项 | 值 |
| --- | --- |
| 文件 | 不入库（维护者内部留档） |
| 生成方式 | `media-aigc` 技能（用户侧调用） |
| provider | `aiyun` |
| model | `gpt-image-2-per-call` |
| 尺寸 | 2048×1152（16:9） |
| 模式 | RGB |

prompt 原文（深色版）：

```text
Installer splash background for a developer tool called Halcyon, 16:9 widescreen. Abstract, minimal, no text, no letters, no logo, no watermark. Deep teal to near-black vertical gradient background with a soft radial glow in the upper center. A subtle abstract motif of two rounded nodes connected by a smooth arch bridge, flat deep teal #0E7C86 with a thin warm orange #F4622F accent, placed in the upper third. Large clean negative space across the middle and lower area for a product name overlay. Faint cream #F5EEDC highlights. Modern, calm, premium software aesthetic, soft depth, very subtle film grain, high quality.
```

## 字体要求

中文字体走 `packaging/brand-art.py` 的查找逻辑（Microsoft YaHei：`msyhbd.ttc` / `msyh.ttc`），
只使用本机已安装的系统字体；找不到时脚本报错退出，**不会回退到会画出方块的默认字体**。
