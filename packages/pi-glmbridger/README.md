# pi-glmbridger

pi 的扩展包，把 pi 内置的 `zai`（智谱）provider 接到 [piglmbridger](https://github.com/Titor-Z/piglmbridger)（GLM SSE 修复中转代理），根治 GLM-5.3 系列流式输出断帧/重试问题。

```
pi ──► http://127.0.0.1:8123 (piglmbridger) ──► https://open.bigmodel.cn/api/paas/v4
```

## 先分清两个东西

| 名称 | 是什么 | 怎么装 |
|---|---|---|
| **pi-glmbridger**（本包） | pi 扩展（TypeScript）。负责把 `zai` baseUrl 改道本地代理，并提供 `/bridger` 命令 | `pi install npm:pi-glmbridger` |
| **piglmbridger** | Rust 代理二进制。真正干活的角色，本包只是它的 pi 侧遥控器 | 见下方"安装第 2 步" |

两者缺一不可：只有本包没有二进制 = pi 连不上代理；只有二进制没有本包 = pi 不知道要走代理。

## 安装

**1. 安装 pi 扩展（本包）**

```bash
pi install npm:pi-glmbridger
```

**2. 安装 piglmbridger 代理二进制**

```bash
curl -fsSL https://github.com/Titor-Z/piglmbridger/releases/latest/download/install.sh | bash
```

**3. 在 pi 里 `/login` 选 `zai`，填智谱（open.bigmodel.cn）的 API Key，`/model` 选 `glm-5.3-flash`**

## `/bridger` 命令

在 pi 里输入 `/bridger`，本包注册的扩展会弹出一个交互菜单（pi 的 select/input 组件）。菜单共四项：

- **状态检查** — 由本包直接请求代理的 `http://127.0.0.1:{port}/health` 接口，显示代理是否在运行及其版本号
- **更改端口** — 由本包直接写入 `~/.piglmbridger/config.toml`（这是 Rust 代理读的同一个配置文件），写完后提示你两条手动命令让新端口生效：
  ```bash
  piglmbridger service restart --port 9999   # 让代理用新端口
  /reload                                     # pi 里执行，让扩展用新端口重新注册 provider
  ```
- **服务控制** — 由本包在后台调用 `piglmbridger` 二进制的 `service start -d / stop / restart / status` 子命令并显示其输出；找不到二进制时会显示安装指引
- **查看日志** — 显示提示命令 `piglmbridger logs -f`（请到另一个终端执行，从本次启动处回放并实时跟踪日志）

一句话：**`/bridger` 是本包提供的 pi 命令，`piglmbridger service ...` 是二进制的 shell 命令**——菜单里除了"状态检查"和"更改端口"由本包亲自完成外，其余都是替你把二进制的命令跑一遍。

## 端口来源（两侧一致）

1. 环境变量 `PIGLMBRIDGER_PORT`（旧名 `GLM_FIX_PROXY_PORT` 兼容）
2. `~/.piglmbridger/config.toml` 的 `port`（`/bridger` 改端口写的就是这里）
3. 默认 `8123`

## License

MIT
