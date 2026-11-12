#!/usr/bin/env python3
"""把 ui/cache-keeper.html 用固定样例数据渲染成顶栏集成预览。

不依赖 Tauri 运行时：注入状态 fixture 与最小 API stub，并模拟左侧选项卡、
统计信息条和缓存保活详情。产物写在 ui/.preview/（已 gitignore）。

用法：
    uv run python scripts/render_cache_keeper_preview.py
    uv run python scripts/render_cache_keeper_preview.py --scale 1
"""

from __future__ import annotations

import argparse
import json
import shutil
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
UI = ROOT / "ui"
PAGE = UI / "cache-keeper.html"
DEFAULT_OUT = UI / ".preview"
WIDTH = 960
HEIGHT = 410
DEFAULT_SCALE = 2


def find_chromium() -> str:
    for name in ("chromium", "chromium-browser", "google-chrome", "chrome"):
        path = shutil.which(name)
        if path:
            return path
    sys.exit("未找到 chromium，请先安装或用 --chrome 指定路径")


def build_fixtures_js() -> str:
    now = time.time() * 1000
    fixture = {
        "installed": True,
        "enabled": True,
        "currentSessionId": "preview-current",
        "sessions": [
            {
                "sessionId": "preview-current",
                "title": "实现 25 分钟缓存续保策略",
                "enabled": True,
                "busy": True,
                "state": "waiting",
                "nextProbeAt": None,
                "lastProbeAt": int(now - 78_000),
                "hitRatio": 0.981,
                "error": None,
            },
            {
                "sessionId": "preview-other",
                "title": "tmi53lx_api · 实现鉴权",
                "enabled": True,
                "busy": False,
                "state": "probing",
                "nextProbeAt": None,
                "lastProbeAt": int(now - 10 * 60_000 - 45_000),
                "hitRatio": 0.974,
                "error": None,
            },
        ],
    }
    return (
        "const FIXTURE = " + json.dumps(fixture, ensure_ascii=False) + ";\n"
        "try { localStorage.clear(); } catch (e) {}\n"
        "window.__TAURI__ = {\n"
        "  core: { invoke: function (cmd) {\n"
        "    if (cmd === 'cache_keeper_status') return Promise.resolve(FIXTURE);\n"
        "    return Promise.resolve({ status: 'ok' });\n"
        "  } },\n"
        "  event: { listen: function () { return Promise.resolve(function () {}); } }\n"
        "};\n"
    )


def preview_chrome() -> str:
    return """
      <div class="preview-tabs" aria-hidden="true">
        <span class="preview-tab active">本地</span>
        <span class="preview-tab">远端</span>
        <span class="preview-tab">设置</span>
        <div class="preview-stats">⚡ 55.2 tok/s · 缓存 94.1%</div>
      </div>
    """


def preview_style() -> str:
    return """
      html, body { width: 960px !important; min-width: 960px !important; }
      #root { position: absolute !important; right: 300px !important; top: 0 !important; z-index: 2; }
      #detail { display: block !important; max-height: none !important; }
      .preview-tabs {
        position: absolute; inset: 0 0 auto 0; z-index: 1;
        display: flex; align-items: stretch; height: 36px; box-sizing: border-box;
        padding-left: 8px; border-bottom: 1px solid var(--divider);
        background: var(--tabbar-bg); color: var(--text-2); font-size: 12px;
      }
      .preview-tab { display: flex; align-items: center; padding: 0 18px; }
      .preview-tab.active { color: var(--text); border-bottom: 2px solid var(--accent); }
      .preview-stats {
        position: absolute; right: 0; top: 0; display: flex; align-items: center;
        justify-content: center; width: 300px; height: 36px; box-sizing: border-box;
        border-left: 1px solid var(--divider); color: var(--text-2);
      }
    """


def inject(html: str, theme: str) -> str:
    base = '<base href="' + UI.as_uri() + '/" />'
    html = html.replace("<head>", "<head>\n  " + base, 1)
    html = html.replace("<body>", "<body>" + preview_chrome(), 1)
    html = html.replace("</head>", "<style>" + preview_style() + "</style>\n</head>", 1)
    stub = "<script>\n" + build_fixtures_js() + "</script>\n"
    idx = html.rindex("<script>")
    html = html[:idx] + stub + html[idx:]
    if theme == "light":
        html = html.replace('<html lang="zh-CN">', '<html lang="zh-CN" data-theme="light">')
    return html


def crop_to_content(path: Path, scale: int) -> None:
    try:
        from PIL import Image, ImageChops
    except ImportError:
        return
    im = Image.open(path).convert("RGB")
    bg = im.getpixel((im.width - 1, im.height - 1))
    diff = ImageChops.difference(im, Image.new("RGB", im.size, bg))
    box = diff.convert("L").point(lambda value: 255 if value > 12 else 0).getbbox()
    if not box:
        return
    bottom = min(im.height, box[3] + 12 * scale)
    im.crop((0, 0, im.width, bottom)).save(path)


def render(chrome: str, theme: str, out_dir: Path, cache_dir: Path, scale: int) -> Path:
    html_path = out_dir / f"cache-keeper-{theme}.html"
    png_path = out_dir / f"cache-keeper-{theme}.png"
    html_path.write_text(inject(PAGE.read_text(encoding="utf-8"), theme), encoding="utf-8")
    cmd = [
        chrome,
        "--headless=new",
        "--no-sandbox",
        "--disable-dev-shm-usage",
        "--hide-scrollbars",
        f"--force-device-scale-factor={scale}",
        f"--window-size={WIDTH},{HEIGHT}",
        "--virtual-time-budget=4000",
        f"--user-data-dir={cache_dir}",
        f"--screenshot={png_path}",
        html_path.as_uri(),
    ]
    proc = subprocess.run(cmd, capture_output=True, text=True)
    if proc.returncode != 0 or not png_path.exists():
        sys.stderr.write(proc.stdout)
        sys.stderr.write(proc.stderr)
        sys.exit("chromium 截图失败：" + theme)
    crop_to_content(png_path, scale)
    return png_path


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--out", type=Path, default=DEFAULT_OUT)
    parser.add_argument("--chrome", default=None)
    parser.add_argument("--scale", type=int, default=DEFAULT_SCALE, help="设备像素比，1 模拟 100%% 缩放屏")
    args = parser.parse_args()
    chrome = args.chrome or find_chromium()
    out_dir = args.out.resolve()
    out_dir.mkdir(parents=True, exist_ok=True)
    cache_dir = Path.home() / ".cache" / "kimi-cache-keeper-preview"
    for theme in ("dark", "light"):
        path = render(chrome, theme, out_dir, cache_dir, args.scale)
        print(path)


if __name__ == "__main__":
    main()
