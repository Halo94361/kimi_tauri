# opencode-session-proxy

本地反向代理：为 OpenCode Go 的模型请求补上 `x-opencode-session` 头。

## 为什么需要它

OpenCode Go 从 2026-09-05 起要求所有推理请求携带稳定的 `x-opencode-session`（用于路由与 prompt cache 亲和），缺失时返回：

```
400 {"type":"error","error":{"type":"MissingSessionID", ...}}
```

Kimi Code CLI 不发送这个头，并明确拒绝适配：官方理由是只按 OpenAI / Anthropic 规范实现，改用请求体的 `prompt_cache_key` / `metadata.user_id` 表达会话（见 [MoonshotAI/kimi-code#3506](https://github.com/MoonshotAI/kimi-code/issues/3506)）。因此需要在本机把 body 字段翻译成 header。

静态 `custom_headers` 也能绕过 400，但整个客户端共用一个 ID，失去“每会话一个 ID”的路由与缓存收益；本代理从请求体取值，恢复会话时 ID 保持不变。

## 数据流

```
Kimi Code CLI（kimi web）
  │  POST http://127.0.0.1:58787/openai/v1/chat/completions
  │  Authorization: Bearer oc_sk_...
  │  {"model":"deepseek-v4.1-flash","prompt_cache_key":"session_...", ...}
  ▼
opencode-session-proxy（127.0.0.1:58787）
  │  1. 读取 body（默认上限 64 MiB，可配置）
  │  2. 解析 prompt_cache_key / metadata.user_id
  │  3. 覆盖 x-opencode-session
  │  4. 重写 URL 为 upstream + 原 path
  ▼
https://opencode.ai/inference/go/...
  │  SSE 流式响应
  ▼
原样透传回 CLI
```

## 一次请求的处理步骤

1. 接受连接，只监听 `127.0.0.1`。
2. 读取请求体（默认上限 64 MiB）：
   - 未超限：完整缓冲后解析，按会话 ID 注入
   - 已知 `Content-Length` 超限：不缓冲，原样流式转发，注入进程级 fallback ID
   - chunked 且超限：已读前缀与剩余流拼接后转发，不截断，同样注入 fallback ID
3. 反序列化 JSON，按优先级取会话 ID。
4. 清洗 ID：只保留可打印 ASCII，去引号与反斜杠，截断到 191 字符。
5. 删除客户端自带的 `x-opencode-session`（大小写两种），写入新值。
6. 拼接上游 URL：`upstream.path + 请求 path`，保留 query；Host 设为上游域名。
7. 带上原始 `Authorization` 转发；补 `X-Forwarded-For` 等标准头。
8. 响应按块透传（`FlushInterval = -1`），SSE 不缓冲。
9. 上游不可达时返回 `502` 与一行纯文本。

## 会话 ID 取值规则

| 优先级 | 来源 | 说明 |
|---:|---|---|
| 1 | 请求体 `prompt_cache_key` | OpenAI 协议；Kimi Code 用会话 ID 填充，恢复会话不变 |
| 2 | 请求体 `metadata.user_id` | Anthropic 协议；Kimi Code 同样会发 |
| 3 | `ses_proxy_<8位hex>` | 进程启动时生成；仅当请求体没有任何会话标识时使用 |

Kimi Code 的 `prompt_cache_key` 与它打印的 `session_<uuid>` 完全一致，因此恢复会话时缓存亲和性连续。

## 路由规则

代理不改写业务路径，只做前缀替换：

| 客户端请求 | 上游 |
|---|---|
| `/openai/v1/chat/completions` | `https://opencode.ai/inference/go/openai/v1/chat/completions` |
| `/openai/v1/responses` | `https://opencode.ai/inference/go/openai/v1/responses` |
| `/anthropic/v1/messages` | `https://opencode.ai/inference/go/anthropic/v1/messages` |

因此 Kimi Code 的 provider 必须配置：

```toml
[providers."OpenCode Go"]
type = "openai"
api_key = "oc_sk_..."
base_url = "http://127.0.0.1:58787/openai/v1"
```

`base_url` 若仍是 `https://opencode.ai/inference/go`，客户端会请求 `/chat/completions`（缺少 `/openai/v1`），上游返回 404。

## 生命周期

### Windows 桌面壳（tauri_kimi）

`src-tauri/src/server.rs` 在启动 `kimi web` 之前调用 `start_proxy`：

- 先做健康检查：`/__ocsp_health` 返回代理标识 → 复用；端口能连上但不是代理 → 判为被其他程序占用；连不上 → 拉起
- 否则按顺序查找二进制：exe 同目录 → `%USERPROFILE%\.kimi-code\bin` → PATH
- 找到就 spawn（`CREATE_NO_WINDOW`），挂入与 `kimi.exe` 相同的 Job Object，最多等 3 秒确认健康
- 找不到二进制 → 静默跳过，不影响其他 provider
- 启动失败且 `config.toml` 确实指向 `127.0.0.1:58787` 时，选项卡栏显示「OpenCode 代理未启动」，hover 看原因，附「重试」；未使用该 provider 时不打扰
- 托盘“退出”时显式 kill；壳被强杀时由 Job Object 级联回收

### Linux

以 systemd user service 常驻：

```bash
systemctl --user status opencode-session-proxy
journalctl --user -u opencode-session-proxy -f
```

服务设置了 `Restart=always`，上游直连：unit 不注入 `HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY`（与 Windows 侧 `server.rs` spawn 时一致）。需要临时走代理时在 unit 里自行补这几行再 `systemctl --user restart`。

## 配置项

| 参数 / 环境变量 | 默认值 | 说明 |
|---|---|---|
| `-listen` / `OCSP_LISTEN` | `127.0.0.1:58787` | 监听地址 |
| `-upstream` / `OCSP_UPSTREAM` | `https://opencode.ai/inference/go` | 上游前缀 |
| `-debug` / `OCSP_DEBUG=1` | 关闭 | 每次请求打印注入的 session id 与来源 |
| `-max-body` / `OCSP_MAX_BODY` | `64`（MiB） | 缓冲解析上限；超限时改为流式转发并注入 fallback ID |

debug 日志只含路径、session id 和来源，不含请求体、Authorization 或 key。

## 验证与排障

正常日志（`OCSP_DEBUG=1` 才有 `inject` 行；默认关闭时只有启动行与上游错误行）：

```
listening on 127.0.0.1:58787 -> https://opencode.ai/inference/go (debug=true)
inject path=/openai/v1/chat/completions session=session_0c8c... source=prompt_cache_key
```

健康检查（壳据此区分“代理正常 / 端口被占用”）：

```bash
curl -s http://127.0.0.1:58787/__ocsp_health   # → opencode-session-proxy
```

直连自测（key 用环境变量传入，不要写进命令历史）：

```bash
curl -sS -o /dev/null -w '%{http_code}\n' \
  -X POST http://127.0.0.1:58787/openai/v1/chat/completions \
  -H "Authorization: Bearer $OPENCODE_API_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"model":"deepseek-v4.1-flash","messages":[{"role":"user","content":"hi"}],"prompt_cache_key":"ses_test"}'
```

| 症状 | 原因 | 处理 |
|---|---|---|
| `404` 无 body | `base_url` 少了 `/openai/v1` | 改成 `.../openai/v1` |
| `400 MissingSessionID` | 请求没经过代理，或代理没注入 | 查 `base_url`、代理日志 |
| `502` | 代理未启动 / 上游不可达 | 看服务状态、上游是否可直连 |
| `Invalid API key` | key 不属于该 workspace | 在 Console 重新生成 |
| 连接被拒 | 端口无人监听 | 启动代理，或检查 58787 是否被占用 |

## 已知限制

- **多实例**：第二个壳实例会复用第一个拉起的代理；先退出的实例会随 Job Object 带走代理，第二个实例的请求随之失败。
- **端口判定依赖健康端点**：只有 `/__ocsp_health` 返回代理标识才算就绪；其他程序占用 58787 会被判为“端口被占用”并提示，而不是误复用。
- **超限请求丢失每会话 ID**：超过 `-max-body`（默认 64 MiB）的请求仍会完整转发，但只能注入进程级 fallback ID，多个大会话会共用同一路由标识；需要时调大该值，代价是内存占用。纯文本请求（1M 上下文约 4 MB）远低于默认上限，图片密集的会话才可能触及。
- **不处理鉴权**：`Authorization` 原样透传，代理不持有 key。
- **仅本机**：绑定 `127.0.0.1`，不要改成 `0.0.0.0` 暴露到局域网。

## 与其他方案对比

| 方案 | 每会话 ID | 需改源码 | 恢复会话保持 ID |
|---|---|---|---|
| 静态 `custom_headers` | 否 | 否 | 是（但全局同一个） |
| 包装脚本 + `KIMI_CODE_CUSTOM_HEADERS` | 每进程 | 否 | 否（恢复时换新 ID） |
| 本代理 | 是 | 否 | 是 |
| 改 CLI 源码重编译 | 是 | 是 | 是 |

## 构建

```bash
cd sidecar/opencode-session-proxy
CGO_ENABLED=0 GOOS=windows GOARCH=amd64 go build -trimpath -buildvcs=false -ldflags="-s -w" -o opencode-session-proxy.exe .
```

`-buildvcs=false` 用于消除 git 元数据导致的产物差异，保证可复现。
