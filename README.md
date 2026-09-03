# glm-fix-proxy — GLM-5.3-Flash SSE 分片修复中转代理

pi Agent → 本地代理 (默认 `127.0.0.1:8123`) → 智谱 GLM API (`https://open.bigmodel.cn/api/paas/v4`)

## 它能做什么

1. **字节缓冲**：按 UTF-8 字符边界截断，避免中文等多字节字符被 TCP 对半切开。
2. **SSE 行缓冲/重组**：残缺、跨包拆分的不完整 `data:` 行先缓存拼接，完整后再下发。
3. **过滤无效帧**：空 `data:` 帧、重复的 `data: [DONE]`。
4. **断流保护**（`drain_abrupt`）：上游把 `data:` 帧切半后砍断连接时，直接丢弃残帧并记 `[ERROR]`，
   绝不补 `\n` 伪造半条 JSON——那会触发下游 `Unterminated string in JSON`。
5. **文件日志 + 实时跟踪**：每次请求记录耗时/下发行数/丢弃帧数，另开终端可 `logs --follow` 实时查看。

## 构建与运行

```bash
cd ~/glm-fix-proxy
cargo build --release          # 首次编译较慢
./target/release/glm-fix-proxy serve     # 用默认端口 8123
```

## 命令行用法

```
glm-fix-proxy serve [--port 8123] [--upstream <url>] [--timeout <secs>]   # 默认子命令
glm-fix-proxy logs [--lines N] [--follow]                                  # 查看/跟踪日志
glm-fix-proxy --help
```

示例：

```bash
glm-fix-proxy serve                       # 8123 / bigmodel / 300s 超时
glm-fix-proxy serve --port 9999           # 换端口
glm-fix-proxy serve --timeout 600         # 上游超时加到 600 秒
glm-fix-proxy logs -f                      # 实时跟踪日志
```

## 配置文件：~/.glm-fix-proxy/config.toml

优先级：**CLI 参数 > 配置文件 > 内置默认值**。首次启动会自动生成默认配置，直接改端口/超时即可：

```toml
port = 8123
upstream = "https://open.bigmodel.cn/api/paas/v4"
timeout_secs = 300
log_dir = "/Users/<you>/.glm-fix-proxy/logs"
```

## 日志

- 文件：`~/.glm-fix-proxy/logs/proxy.log`（超过 10 MB 会自动轮转为 `proxy.log.1`）
- 一条典型记录：
  ```
  [INFO ] [6f11b1] <- 200 OK SSE 流开始
  [ERROR] [bdfc5e] 流被上游切断：残留未终结的 data 残帧(60B)已丢弃: data: {...…
  [INFO ] [bdfc5e] SSE 流结束(done=false)，规整下发 1 行，丢弃 1 个无效帧，耗时 0.21s
  ```
- `done=false` + 丢弃残帧 → 上游曾把流砍断（GLM 已知坑），代理已安全兜住。

## pi 侧接入（~/glm-fix-proxy 端口与扩展同步）

本仓库已附带 pi 侧接入资产（`pi/` 目录）：
- [`pi/extensions/glm-proxy.ts`](pi/extensions/glm-proxy.ts) — 把内置 `zai` provider 的 baseUrl 指向本地代理
- [`pi/settings.glm-snippet.json`](pi/settings.glm-snippet.json) — GLM 推荐配置**片段**（重试/超时/思考级别），按段合并进你的 settings.json，不要整份覆盖

安装：
```bash
mkdir -p ~/.pi/agent/extensions
cp pi/extensions/glm-proxy.ts ~/.pi/agent/extensions/
# 然后把 pi/settings.glm-snippet.json 里需要的段合并进 ~/.pi/agent/settings.json
```

pi 用一个扩展把内置 `zai` provider 的 baseUrl 指到本地代理，端口两处要保持一致：

| 控制端 | 位置 | 默认 |
|---|---|---|
| 配置文件 | `~/.glm-fix-proxy/config.toml` → `port` | 8123 |
| CLI | `--port`（会覆盖配置文件） | — |
| pi 扩展 | `~/.pi/agent/extensions/glm-proxy.ts` 里 `GLM_FIX_PROXY_PORT` 或 `DEFAULT_PORT` | 8123 |

改端口时三处同步（例如改成 9999）：

```bash
# 1) 配置文件（或直接 --port 9999 启动）
sed -i '' 's/port = 8123/port = 9999/' ~/.glm-fix-proxy/config.toml

# 2) 启动代理
./target/release/glm-fix-proxy serve

# 3) 改扩展端口并重启 pi
#    设置环境变量：export GLM_FIX_PROXY_PORT=9999
#    或修改 glm-proxy.ts 的 DEFAULT_PORT = 9999
pi
```

pi 内步骤：`/login` 选 **zai** 填智谱 API Key → `/model` 选 **glm-5.3-flash** → 跑带工具调用的任务。

## 诊断指南（出问题时怎么分工）

1. **看代理日志**，确认是不是上游截断：
   ```bash
   ./target/release/glm-fix-proxy logs -f
   ```
   - 有 `[ERROR] ... 残帧已丢弃` → 上游断流，代理已兜住；该次多半能正常结束。
   - 某条请求只有 `SSE 流开始`、没有 `流结束` → 说明客户端(pi)中途断开了该条 SSE，属客户端侧主动 abort。
2. **看 pi 侧报错**，与代理日志时间戳对照：若 pi 报错时刻代理日志正好有 `残帧丢弃/流结束(done=false)`，则链路上代理没问题、是 GLM 上游本身行为，需依赖 retry 设置。
3. 若 pi 报 `connection refused` → 代理没起或端口不一致（见上表）。

## 测试

```bash
cargo test           # 单元测试：拼帧、断流去帧、[DONE] 去重
```
