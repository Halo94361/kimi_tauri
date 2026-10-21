#!/usr/bin/env bash
# 环境同步检查：逐项输出本机工具链关键信息。
# 用法：
#   参考机器：bash scripts/check_env.sh > scripts/check_env_golden.txt   # 生成 golden
#   新机器：  bash scripts/check_env.sh | diff scripts/check_env_golden.txt -
# diff 中用户名/安装路径差异可忽略，重点核对版本号与 gcc_eh 两项是否存在。
set -uo pipefail

cd "$(dirname "$0")/.."

section() { printf '\n== %s ==\n' "$1"; }

section "rustc（版本与 host 必须一致，host 应为 x86_64-pc-windows-gnu）"
rustc -vV

section "gcc（必须是完整 MinGW-Builds，w64devkit 缺 libgcc_eh.a 会链接失败）"
which gcc
gcc --version | head -1

section "gcc 库目录的 libgcc_eh（必须能列出 libgcc_eh.a）"
ls "$(dirname "$(gcc -print-libgcc-file-name)")" | grep gcc_eh

section "Rust self-contained 的 libgcc_eh（链接双保险之一）"
ls "$(rustc --print target-libdir)/self-contained/" | grep gcc_eh

section "cargo-tauri（需要 v2）"
cargo tauri --version

section "kimi CLI（壳的运行时依赖）"
kimi --version

section "WebView2 Runtime（注册表版本号，查不到说明未安装）"
reg query 'HKLM\SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}' //v pv 2>/dev/null \
  || echo "未找到 WebView2 Runtime 注册表项"

section ".cargo/config.toml 中的绝对路径（须指向本机实际仓库位置）"
cfg="src-tauri/.cargo/config.toml"
if [ -f "$cfg" ]; then
  grep -o '[A-Za-z]:/[^"]*' "$cfg" | while read -r p; do
    if [ -d "$p" ]; then
      echo "存在: $p"
    else
      echo "失效: $p  （在本机不存在，该链接参数会被静默忽略）"
    fi
  done
else
  echo "无 config.toml"
fi
