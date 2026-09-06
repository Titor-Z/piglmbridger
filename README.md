# piglmbridger

在 pi 里顺畅使用智谱 GLM 模型的本地小助手。

有了它，GLM 的回复不再断断续续、不再莫名卡住重试，长任务一次跑完。

## 它是怎么工作的

pi 对话时经过这个运行在你自己电脑上的小助手，再到智谱服务器。它负责把传输出问题的地方悄悄修好——你什么都不用设置。

```
pi ──► piglmbridger（你的电脑） ──► 智谱 GLM
```

## 安装

**macOS / Linux**

```bash
curl -fsSL https://github.com/Titor-Z/piglmbridger/releases/latest/download/install.sh | bash
```

**Windows**：到 [Releases](https://github.com/Titor-Z/piglmbridger/releases) 下载 `piglmbridger-x86_64-pc-windows-msvc.zip`，解压后把 `piglmbridger.exe` 放进 PATH。

## 启动

```bash
piglmbridger service start -d    # 在后台启动（日常推荐）
piglmbridger service status      # 看它是否在线
piglmbridger service stop        # 不用时停止
```

## 配合 pi 使用

在 pi 里安装配套插件即可：

```bash
pi install npm:pi-glmbridger
```

然后在 pi 里 `/login` 选 **zai** 填入智谱 API Key，`/model` 选 **glm-5.3-flash**，开始使用。装好后输入 `/bridger` 可以直接在 pi 里查看状态、改设置、启停服务。

## 常用命令

| 命令 | 作用 |
|---|---|
| `piglmbridger service start -d` | 后台启动 |
| `piglmbridger service status` | 查看运行状态 |
| `piglmbridger logs -f` | 实时查看运行记录 |
| `piglmbridger doctor` | 体检：配置、端口、网络一把查 |
| `piglmbridger stats` | 看使用统计 |

## 常见问题

**需要付费吗？**
小助手免费。使用 GLM 模型的费用取决于你在智谱开放平台的账户。

**我的 API Key 安全吗？**
Key 只保存在你自己的电脑上，请求也只经过你自己的电脑，不经过任何第三方服务器。

**升级**：重新运行上面的安装命令即可覆盖到最新版。

## License

MIT
