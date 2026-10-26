#!/usr/bin/env python3
"""把 ui/stats.html 用固定样例数据渲染成 PNG，供设计回测与预览。

不引入前端构建：读取 stats.html，在页面脚本前注入一个最小 Tauri API stub
（invoke 返回 fixtures，listen 返回空退订函数），强制展开明细后交给
headless chromium 截图。产物写在 ui/.preview/（已 gitignore）。

用法：
    python3 scripts/render_stats_preview.py
    python3 scripts/render_stats_preview.py --out /path/to/dir
    python3 scripts/render_stats_preview.py --scale 1   # 模拟 100% 缩放屏
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
STATS = ROOT / "ui" / "stats.html"
DEFAULT_OUT = ROOT / "ui" / ".preview"
WIDTH = 300
# 明细全展开约 440px，留足余量；多余背景最后按内容裁掉
HEIGHT = 760
# 默认按 200% 渲染看清细节；--scale 1 用于核对 100% 缩放屏上的实际观感
DEFAULT_SCALE = 2


def find_chromium() -> str:
    for name in ("chromium", "chromium-browser", "google-chrome", "chrome"):
        path = shutil.which(name)
        if path:
            return path
    sys.exit("未找到 chromium，请先安装或用 --chrome 指定路径")


def build_fixtures_js() -> str:
    """样例数据：数值贴近真实会话，时间相对当前时刻生成，重置倒计时可读。"""
    now = time.time() * 1000
    hour = 3_600_000
    day = 86_400_000
    # 24 小时桶：白天高、凌晨低，带一点子代理用量
    start = int(now // hour * hour) - 23 * hour
    pattern = [
        0.3, 0.2, 0.1, 0.1, 0.2, 0.4, 0.9, 1.8, 3.2, 4.6, 5.4, 4.1,
        3.6, 5.1, 6.3, 5.8, 4.9, 3.7, 2.8, 2.1, 1.5, 1.1, 0.7, 0.5,
    ]
    hours = []
    for i, k in enumerate(pattern):
        main = int(k * 1_800_000)
        sub = int(k * 260_000) if i % 3 == 0 else 0
        hours.append({"t": start + i * hour, "main": main, "sub": sub})

    fixtures = {
        "session_stats": {
            "found": True,
            "session_id": "preview-session",
            "has_tps": True,
            "tps_avg": 55.2,
            "tps_latest": 61.4,
            "input_total": 226_100_000,
            "output_total": 141_300,
            "cache_read": 219_200_000,
            "cache_creation": 5_400_000,
            "cache_hit_rate": 94.1,
            "last_record_time": int(now),
        },
        "daily_stats": {
            "found": True,
            "input_total": 177_100_000,
            "output_total": 1_042_000,
            "cache_read": 171_500_000,
            "cache_hit_rate": 96.8,
        },
        "hourly_stats": {
            "found": True,
            "start": start,
            "step_ms": hour,
            "hours": hours,
            "cache_hit_rate": 92.4,
        },
        "quota_stats": {
            "kimi": {
                "ok": True,
                "five_hour": {
                    "used_pct": 13.0,
                    "reset_at": int(now + 32 * 60_000),
                    "window_ms": 5 * hour,
                },
                "weekly": {
                    "used_pct": 19.0,
                    "reset_at": int(now + 6 * day + 18 * hour),
                    "window_ms": 7 * day,
                },
                "fetched_at": int(now),
            },
            "opencode": {
                "ok": True,
                "rolling": {
                    "used_pct": 5.0,
                    "reset_at": int(now + 2 * hour),
                    "window_ms": 5 * hour,
                },
                "weekly": {
                    "used_pct": 7.0,
                    "reset_at": int(now + 2 * day),
                    "window_ms": 7 * day,
                },
                "monthly": {
                    "used_pct": 3.0,
                    "reset_at": int(now + 30 * day),
                    "window_ms": 31 * day,
                },
                "fetched_at": int(now),
            },
        },
        "get_settings": {
            "stats_enabled": True,
            "stats_show_usage": True,
            "stats_quota_kimi": True,
            "stats_quota_opencode": True,
            "stats_quota_refresh_secs": 120,
        },
    }
    return (
        "const FIXTURES = " + json.dumps(fixtures, ensure_ascii=False) + ";\n"
        "try { localStorage.clear(); } catch (e) {}\n"
        "window.__TAURI__ = {\n"
        "  core: { invoke: function (cmd) {\n"
        "    return Promise.resolve(Object.prototype.hasOwnProperty.call(FIXTURES, cmd) ? FIXTURES[cmd] : null);\n"
        "  } },\n"
        "  event: { listen: function () { return Promise.resolve(function () {}); } }\n"
        "};\n"
    )


def inject(html: str, theme: str) -> str:
    stub = "<script>\n" + build_fixtures_js() + "</script>\n"
    # 唯一的页面脚本在 body 末尾；stub 必须排在它前面
    idx = html.rindex("<script>")
    html = html[:idx] + stub + html[idx:]
    # 预览时强制展开明细，不受 hover / pin 状态影响
    html = html.replace(
        "</head>",
        "<style>#detail{display:block!important;max-height:none!important}</style>\n</head>",
    )
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
    box = diff.getbbox()
    if not box:
        return
    bottom = min(im.height, box[3] + 12 * scale)
    im.crop((0, 0, im.width, bottom)).save(path)


def render(chrome: str, theme: str, out_dir: Path, cache_dir: Path, scale: int) -> Path:
    html_path = out_dir / f"stats-{theme}.html"
    png_path = out_dir / f"stats-{theme}.png"
    html_path.write_text(inject(STATS.read_text(encoding="utf-8"), theme), encoding="utf-8")
    cmd = [
        chrome,
        "--headless=new",
        "--no-sandbox",
        "--disable-dev-shm-usage",
        "--hide-scrollbars",
        "--force-device-scale-factor=%d" % scale,
        "--window-size=%d,%d" % (WIDTH, HEIGHT),
        "--virtual-time-budget=4000",
        "--user-data-dir=%s" % cache_dir,
        "--screenshot=%s" % png_path,
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
    cache_dir = Path.home() / ".cache" / "kimi-stats-preview"
    for theme in ("dark", "light"):
        path = render(chrome, theme, out_dir, cache_dir, args.scale)
        print(path)


if __name__ == "__main__":
    main()
