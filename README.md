# piglmbridger — Pi-Agent × GLM-5.3-Flash SSE 桥接器

![Release](https://img.shields.io/github/v/release/Titor-Z/piglmbridger?logo=github)
![CI](https://github.com/Titor-Z/piglmbridger/actions/workflows/release.yml/badge.svg)
![License](https://img.shields.io/github/license/Titor-Z/piglmbridger)

> 专为 Pi-Agent + GLM-5.3-Flash 打造的 Rust 代理桥接器：修复智谱 SSE 流式分片破碎引发的无限重试、输出截断问题。
> 配合 pi 内置 `zai` provider 覆写 baseUrl，对接智谱国内 API（open.bigmodel.cn）。


pi Agent → 本地代理 (默认 `127.0.0.1:8123`) → 智谱 GLM API (`https://open.bigmodel.cn/api/paas/v4`)

## 为什么用它 ？
在`pi`中，使用 `ZAI` 的 `GLM-5.3-flash` 模型，会遇到输出一卡一卡的、或者运行着突然不停的 连接重试、之后会频繁的报错退出执行状态，这都是 `GLM-5.3-flash` 和 `pi` 在设计上有不兼容的地方造成的，为了修复这方面的问题，建议你使用本程序。

## 构建与运行

```bash
cd ~/piglmbridger
cargo build --release          # 首次编译较慢
./target/release/piglmbridger serve     # 用默认端口 8123
```

## 命令行用法

```
piglmbridger serve [--addr 127.0.0.1] [--port 8123] [--upstream <url>] [--timeout <secs>] [--color auto|always|never] [--log-level info|debug]  # 前台
piglmbridger start | stop | restart | status                              # 守护进程（进程名 piglmbridged）
piglmbridger doctor [--api-key <key>]                                     # 体检：配置/端口/上游连通性
piglmbridger logs [--lines N] [--follow]                                  # 查看/跟踪日志
piglmbridger --help
```

### 守护进程（推荐日常使用）

```bash
piglmbridger start    # 后台启动，进程名 piglmbridged，pid 写入 ~/.piglmbridger/piglmbridged.pid
piglmbridger status   # 运行状态 + 端口可达性
piglmbridger stop     # 优雅退出（SIGTERM，等在途流收尾最长 30s，超时 SIGKILL）
piglmbridger restart
```

### 体检

```bash
piglmbridger doctor                # 配置校验 + 端口占用 + 上游连通性（401=可达属预期）
piglmbridger doctor --api-key sk-… # 带真实 key 探活（验证 key 与模型可用性）
```

### 日志颜色

终端下自动着色（ERROR 红、req_id 青色成组）；接管道时自动退回纯文本，`logs -f | grep <req_id>` 干净可用。`--color always|never` 可强制。


示例：

```bash
piglmbridger serve                       # 8123 / bigmodel / 300s 超时
piglmbridger serve --port 9999           # 换端口
piglmbridger serve --timeout 600         # 上游超时加到 600 秒
piglmbridger logs -f                      # 实时跟踪日志
```

## 配置文件：~/.piglmbridger/config.toml

优先级：**CLI 参数 > 配置文件 > 内置默认值**。首次启动会自动生成默认配置，直接改端口/超时即可：

```toml
port = 8123
addr = "127.0.0.1"              # 0.0.0.0 = 对外开放（务必配好 auth_token！）
upstream = "https://open.bigmodel.cn/api/paas/v4"
timeout_secs = 300              # 总超时
idle_timeout_secs = 120         # 读空闲看门狗（GLM 长思考保护），0=禁用
auth_token = ""                 # 远端令牌，非空则校验 Authorization: Bearer <token>
log_dir = "/Users/<you>/.piglmbridger/logs"
```

**架构说明（v0.3.0）**：
- **模型感知**：仅 `glm-5.3*` 走 SSE 归一化（分片拼帧/过滤/错误帧）；其它模型字节级直通，零副作用。
- **带内错误帧**：上游断流/读空闲时，先丢残帧，再补发标准 `data: {"error":{...}}` + `[DONE]`，pi 立即判定失败而非干等超时（不伪造数据帧）。
- **取消透传**：客户端提前断开 → body 被 cancel → 上游随即中止（已 debug 日志实锤），不再白扣费用。
- **Header 白名单**：只透传 Authorization/Content-Type/Accept/User-Agent。
- **日志等级**：`serve --log-level debug` 看细节；终端彩色、管道纯文本。
- **`重试`模式**：若配了 `auth_token`，未带/错令牌直接 401，不上游。


## 日志

### 终端日志图例（`serve` 前台运行时）

```
02:25:10.561 ▶ [c8bdb0] glm-5.3-flash → bigmodel.cn/chat/completions ↑ 357.6KB
⠹ [c8bdb0] ↓ 48.2KB…
02:25:42.517 ✔ [c8bdb0] 200 · +5.2s · ↑ 357.6KB · ↓ 96.4KB · 1250 tok
```

| 符号 | 含义 |
|---|---|
| `▶` | 请求开始（模型 + 上游缩写 + ↑ 请求体大小） |
| `⠋⠙⠹…` | SSE 传输中原位动画（↓ 已下行字节数；结束时擦除，不占历史） |
| `✔` / `✘` | 请求完成 / 失败（含 idle 中止、上游拒绝） |
| `↑` / `↓` | 上行请求体 / 下行响应字节数 |
| `tok` | 上游 usage 的 total_tokens（计费口径） |
| `+5.2s` / `首包 812ms` | 总耗时 / 真·首包延迟（第一个上游 chunk） |
| `[c8bdb0]` | 请求 ID（6 位十六进制，同一请求跨行成组） |

终端只保留开始/结束两条历史，中间过程数据在文件日志里完整保留。

### 文件日志

- 文件：`~/.piglmbridger/logs/proxy.log`（超过 10 MB 会自动轮转为 `proxy.log.1`）
- 一条典型记录：
  ```
  [INFO ] [c8bdb0] -> glm-5.3-flash POST /v1/chat/completions (req 357.6KB) 转发至 https://open.bigmodel.cn/api/paas/v4/chat/completions
  [INFO ] [c8bdb0] … ↓ 48.2KB
  [INFO ] [c8bdb0] <- 200 耗时 5.21s 首包 812ms req 357.6KB resp 96.4KB 1250 tok
  [ERROR] [bdfc5e] 流被上游切断：残留未终结的 data 残帧(60B)已丢弃: data: {...…
  ```
- `done=false` + 丢弃残帧 → 上游曾把流砍断（GLM 已知坑），代理已安全兜住。

## pi 侧接入（~/piglmbridger 端口与扩展同步）

本仓库已附带 pi 侧接入资产（`pi/` 目录）：
- [`pi/extensions/piglmbridger.ts`](pi/extensions/piglmbridger.ts) — 把内置 `zai` provider 的 baseUrl 指向本地代理
- [`pi/settings.glm-snippet.json`](pi/settings.glm-snippet.json) — GLM 推荐配置**片段**（重试/超时/思考级别），按段合并进你的 settings.json，不要整份覆盖

安装：
```bash
mkdir -p ~/.pi/agent/extensions
cp pi/extensions/piglmbridger.ts ~/.pi/agent/extensions/
# 然后把 pi/settings.glm-snippet.json 里需要的段合并进 ~/.pi/agent/settings.json
```

pi 用一个扩展把内置 `zai` provider 的 baseUrl 指到本地代理，端口两处要保持一致：

| 控制端 | 位置 | 默认 |
|---|---|---|
| 配置文件 | `~/.piglmbridger/config.toml` → `port` | 8123 |
| CLI | `--port`（会覆盖配置文件） | — |
| pi 扩展 | `~/.pi/agent/extensions/piglmbridger.ts` 里 `PIGLMBRIDGER_PORT`（旧名 `GLM_FIX_PROXY_PORT` 仍兼容）或 `DEFAULT_PORT` | 8123 |

改端口时三处同步（例如改成 9999）：

```bash
# 1) 配置文件（或直接 --port 9999 启动）
sed -i '' 's/port = 8123/port = 9999/' ~/.piglmbridger/config.toml

# 2) 启动代理
./target/release/piglmbridger serve

# 3) 改扩展端口并重启 pi
#    设置环境变量：export PIGLMBRIDGER_PORT=9999   （旧名 GLM_FIX_PROXY_PORT 仍兼容）
#    或修改 piglmbridger.ts 的 DEFAULT_PORT = 9999
pi
```

pi 内步骤：`/login` 选 **zai** 填智谱 API Key → `/model` 选 **glm-5.3-flash** → 跑带工具调用的任务。

## 诊断指南（出问题时怎么分工）

1. **看代理日志**，确认是不是上游截断：
   ```bash
   ./target/release/piglmbridger logs -f
   ```
   - 有 `[ERROR] ... 残帧已丢弃` → 上游断流，代理已兜住；该次多半能正常结束。
   - 某条请求只有 `SSE 流开始`、没有 `流结束` → 说明客户端(pi)中途断开了该条 SSE，属客户端侧主动 abort。
2. **看 pi 侧报错**，与代理日志时间戳对照：若 pi 报错时刻代理日志正好有 `残帧丢弃/流结束(done=false)`，则链路上代理没问题、是 GLM 上游本身行为，需依赖 retry 设置。
3. 若 pi 报 `connection refused` → 代理没起或端口不一致（见上表）。

## 测试

```bash
cargo test           # 单元测试：拼帧、断流去帧、[DONE] 去重
```
