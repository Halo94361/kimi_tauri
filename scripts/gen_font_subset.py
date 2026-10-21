#!/usr/bin/env python3
"""生成 UI 用 Sarasa Mono SC woff2 子集。

壳内自绘页面（选项卡栏 / 启动页 / 错误页 / 用量浮窗）统一用更纱黑体等宽，
kimi web 内容页保持原样。子集覆盖 ASCII + 中文标点 + GB2312 一级常用字，
动态错误文案也能落到等宽字体上。

用法：
    python3 scripts/gen_font_subset.py \
        --src /path/to/SarasaMonoSC-Regular.ttf \
        --out ui/fonts/SarasaMonoSC-UI.woff2
"""
import argparse
import pathlib
import sys

from fontTools import subset

ROOT = pathlib.Path(__file__).resolve().parent.parent


def gb2312_hanzi(level: int) -> set:
    """GB2312 一级（第 16~55 区，3755 字）或二级（56~87 区）汉字。"""
    leads = range(0xB0, 0xD8) if level == 1 else range(0xD8, 0xF8)
    chars = set()
    for lead in leads:
        for trail in range(0xA1, 0xFF):
            try:
                ch = bytes([lead, trail]).decode("gb2312")
            except UnicodeDecodeError:
                continue
            if "一" <= ch <= "鿿":
                chars.add(ch)
    return chars


def repo_chars() -> set:
    """页面与 Rust 侧出现过的非 ASCII 字符（含拆分后的所有模块）。"""
    chars = set()
    targets = list((ROOT / "ui").glob("*.html")) + list(
        (ROOT / "src-tauri" / "src").rglob("*.rs")
    )
    for path in targets:
        if not path.exists():
            continue
        chars.update(c for c in path.read_text(encoding="utf-8") if ord(c) > 0x7F)
    return chars


def build_charset(level: int) -> set:
    chars = set()
    chars.update(chr(c) for c in range(0x20, 0x7F))  # ASCII
    chars.update(chr(c) for c in range(0x3000, 0x3040))  # 中文标点
    chars.update(chr(c) for c in range(0xFF01, 0xFF61))  # 全角形式
    chars.update(chr(c) for c in range(0xFFE0, 0xFFE6))  # 全角货币符号
    chars.update(chr(c) for c in range(0x2010, 0x2028))  # 通用标点（— – ' ' " " …）
    chars.update("·—…‘’“”•°±×÷")
    chars.update(chr(c) for c in range(0x2190, 0x2194))  # 箭头
    chars.update("✓✔✗✘■□●○★☆◆◇▲▼")
    chars.update(gb2312_hanzi(level))
    chars.update(repo_chars())
    return chars


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--src", required=True, help="SarasaMonoSC-Regular.ttf 源文件")
    ap.add_argument("--out", required=True, help="输出的 woff2 路径")
    ap.add_argument("--gb-level", type=int, default=1, choices=(1, 2), help="GB2312 汉字级数")
    args = ap.parse_args()

    charset = build_charset(args.gb_level)

    options = subset.Options()
    options.flavor = "woff2"
    options.desubroutinize = True
    options.hinting = False
    options.layout_features = ["kern", "liga", "locl", "ccmp", "vert", "vrt2"]

    font = subset.load_font(args.src, options)
    subsetter = subset.Subsetter(options=options)
    subsetter.populate(text="".join(sorted(charset)))
    subsetter.subset(font)
    subset.save_font(font, args.out, options)

    # 校验：子集 cmap 必须覆盖全部请求字符
    from fontTools.ttLib import TTFont

    check = TTFont(args.out)
    cmap = check.getBestCmap()
    # emoji / 象形符号本字体没有，交给 CSS 回退字体，不算缺字
    missing = sorted(c for c in charset if ord(c) not in cmap and ord(c) < 0x1F000)
    fallback = sorted(c for c in charset if ord(c) not in cmap and ord(c) >= 0x1F000)
    size = pathlib.Path(args.out).stat().st_size
    print(f"字符数 {len(charset)} -> 缺字 {len(missing)}，回退 {len(fallback)}，产物 {size / 1024:.1f} KB")
    if fallback:
        print("回退字形：" + "".join(fallback))
    if missing:
        print("缺失样例：" + "".join(missing[:40]))
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
