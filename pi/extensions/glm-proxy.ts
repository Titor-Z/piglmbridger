// pi 接入 piglmbridger 的扩展：把内置 zai（智谱）provider 的 baseUrl 改道本地代理。
//
// 安装：复制到 ~/.pi/agent/extensions/glm-proxy.ts （或用 pi 的 extensions 配置指向本文件）
// 鉴权：/login 选 zai，填智谱(open.bigmodel.cn) 的 API Key。
//
// 数据流：pi -> http://127.0.0.1:${PIGLMBRIDGER_PORT:-8123} -> https://open.bigmodel.cn/api/paas/v4
// 端口需与代理侧（config.toml 的 port 或启动参数 --port）保持一致。
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

const DEFAULT_PORT = 8123;

export default function (pi: ExtensionAPI) {
  const port = Number(process.env.PIGLMBRIDGER_PORT ?? process.env.GLM_FIX_PROXY_PORT) || DEFAULT_PORT;
  pi.registerProvider("zai", {
    baseUrl: `http://127.0.0.1:${port}`,
  });
}
