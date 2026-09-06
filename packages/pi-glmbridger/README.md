# pi-glmbridger

在 [pi](https://pi.dev) 里顺畅使用智谱 GLM 模型的插件。

用它之后，GLM 回复不再断断续续、不再莫名卡住重试，长任务一次跑完。

## 安装

```bash
pi install npm:pi-glmbridger
```

再装上它的搭档——本地小助手 [piglmbridger](https://github.com/Titor-Z/piglmbridger#安装)：

```bash
curl -fsSL https://github.com/Titor-Z/piglmbridger/releases/latest/download/install.sh | bash
```

然后在 pi 里 `/login` 选 **zai** 填入智谱 API Key，`/model` 选 **glm-5.3-flash**，开始使用。

## 日常使用：`/bridger`

在 pi 里输入 `/bridger`，会弹出一个菜单：

- **状态检查** — 一眼看到小助手是否在线、什么版本
- **更改端口** — 界面里点几下就能改，不用手动编辑配置文件
- **查看日志** — 直接在 pi 里展示每个请求的一行摘要（模型、耗时、上下行流量、token 数）

通常装完就不需要再管它了。遇到问题时 `/bridger` 看一眼状态即可。

## 问答

**需要付费吗？**
插件本身免费。使用 GLM 模型的费用取决于你在智谱开放平台的账户。

**我的 API Key 安全吗？**
Key 只保存在你自己的电脑上（pi 的登录系统管理），请求也只经过你自己电脑上的本地小助手，不经过任何第三方服务器。

## License

MIT
