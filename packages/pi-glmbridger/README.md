# pi-glmbridger

pi 的 [piglmbridger](https://github.com/Titor-Z/piglmbridger) 接入扩展：把内置 `zai`（智谱）provider 的 baseUrl 改道本地 SSE 修复中转代理，根治 GLM-5.3 系列流式输出断帧/重试问题。

```
pi ──► http://127.0.0.1:8123 (piglmbridger) ──► https://open.bigmodel.cn/api/paas/v4
```

## 安装（两步）

**1. 安装 pi 扩展（本包）**

```bash
pi install npm:pi-glmbridger
```

**2. 安装 piglmbridger 二进制（Rust 代理本体，不在本包内）**

```bash
# macOS arm64 示例，全部平台见 Releases
curl -fsSL https://github.com/Titor-Z/piglmbridger/releases/latest/download/install.sh | bash
```

然后在 pi 里 `/login` 选 **zai**，填智谱（open.bigmodel.cn）的 API Key。

## `/bridger` 命令

在 pi 里输入 `/bridger` 打开交互菜单：

| 菜单项 | 说明 |
|---|---|
| 状态检查 | 探测 `http://127.0.0.1:{port}/health`，显示代理版本与端口 |
| 更改端口 | 写入 `~/.piglmbridger/config.toml`（与 Rust 代理共享同一配置），提示重启与 `/reload` 生效 |
| 服务控制 | `piglmbridger service start -d / stop / restart / status` |
| 查看日志 | 提示 `piglmbridger logs -f`（从本次启动处回放 + 实时跟踪） |

## 端口来源（与 Rust 代理一致）

1. 环境变量 `PIGLMBRIDGER_PORT`（旧名 `GLM_FIX_PROXY_PORT` 兼容）
2. `~/.piglmbridger/config.toml` 的 `port`
3. 默认 `8123`

改端口在 `/bridger` 里改一处即可——扩展读、代理读的是同一个配置文件。

## License

MIT
