// pi-glmbridger：pi 接入 piglmbridger 的扩展 + /bridger 交互管理命令。
//
// 安装：pi install npm:pi-glmbridger
// 鉴权：pi 里 /login 选 zai，填智谱(open.bigmodel.cn) 的 API Key。
// 数据流：pi -> http://127.0.0.1:${port} -> https://open.bigmodel.cn/api/paas/v4
// 端口来源与 Rust 代理一致：env PIGLMBRIDGER_PORT（旧 GLM_FIX_PROXY_PORT 兼容）> ~/.piglmbridger/config.toml 的 port > 8123
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { readFileSync, writeFileSync, existsSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

const DEFAULT_PORT = 8123;
const CONFIG_PATH = join(homedir(), ".piglmbridger", "config.toml");
const HOST = "open.bigmodel.cn";

interface Health {
  ok: boolean;
  name?: string;
  version?: string;
}

/** 端口解析：env > config.toml > 默认（与 Rust 侧 Config::load 优先级一致） */
function resolvePort(): number {
  const env = process.env.PIGLMBRIDGER_PORT ?? process.env.GLM_FIX_PROXY_PORT;
  if (env) {
    const n = Number(env);
    if (Number.isInteger(n) && n >= 1 && n <= 65535) return n;
  }
  try {
    const raw = readFileSync(CONFIG_PATH, "utf8");
    const m = raw.match(/^\s*port\s*=\s*(\d+)\s*$/m);
    if (m) {
      const n = Number(m[1]);
      if (Number.isInteger(n) && n >= 1 && n <= 65535) return n;
    }
  } catch {
    // 配置不存在走默认
  }
  return DEFAULT_PORT;
}

/** 写回端口到 config.toml：无则创建带注释的最小文件，有则只替换 port 行 */
function writePort(port: number): "created" | "updated" | "error" {
  try {
    if (!existsSync(CONFIG_PATH)) {
      writeFileSync(
        CONFIG_PATH,
        `# piglmbridger 配置文件（优先级：CLI 参数 > 此文件 > 内置默认值）\nport = ${port}\n`,
      );
      return "created";
    }
    const raw = readFileSync(CONFIG_PATH, "utf8");
    if (/^\s*port\s*=\s*\d+\s*$/m.test(raw)) {
      writeFileSync(CONFIG_PATH, raw.replace(/^\s*port\s*=\s*\d+\s*$/m, `port = ${port}`));
    } else {
      writeFileSync(CONFIG_PATH, `port = ${port}\n${raw}`);
    }
    return "updated";
  } catch {
    return "error";
  }
}

/** 探活代理 /health（1.5s 超时）；错误分类返回 */
async function probe(port: number): Promise<{ state: "up" | "down"; health?: Health }> {
  const ctrl = new AbortController();
  const timer = setTimeout(() => ctrl.abort(), 1500);
  try {
    const resp = await fetch(`http://127.0.0.1:${port}/health`, { signal: ctrl.signal });
    if (resp.ok) return { state: "up", health: (await resp.json()) as Health };
    return { state: "down" };
  } catch {
    return { state: "down" };
  } finally {
    clearTimeout(timer);
  }
}

/** 拉取末尾 N 条单行摘要（1.5s 超时） */
async function fetchSummaries(port: number, lines: number): Promise<string[] | null> {
  const ctrl = new AbortController();
  const timer = setTimeout(() => ctrl.abort(), 1500);
  try {
    const resp = await fetch(`http://127.0.0.1:${port}/logs?lines=${lines}`, { signal: ctrl.signal });
    if (!resp.ok) return null;
    const body = (await resp.json()) as { lines?: string[] };
    return Array.isArray(body.lines) ? body.lines : null;
  } catch {
    return null;
  } finally {
    clearTimeout(timer);
  }
}

/** 摘要行轻着色：▶ 淡色、✔ 绿、✘ 红、req_id 青色 */
function colorize(line: string): string {
  return line
    .replace(/▶/, "\x1b[2m▶\x1b[0m")
    .replace(/✔/, "\x1b[1;32m✔\x1b[0m")
    .replace(/✘/, "\x1b[1;31m✘\x1b[0m")
    .replace(/\[([0-9a-f]{6})\]/, "\x1b[36m[$1]\x1b[0m");
}

export default function (pi: ExtensionAPI) {
  const port = resolvePort();

  // 与旧扩展保持一致：改道内置 zai provider 到本地代理
  pi.registerProvider("zai", {
    baseUrl: `http://127.0.0.1:${port}`,
  });

  pi.registerCommand("bridger", {
    description: "piglmbridger 代理管理（状态/端口/日志）",
    handler: async (_args, ctx) => {
      let running = true;
      while (running) {
        const currentPort = resolvePort();
        const choice = await ctx.ui.select("piglmbridger 管理", [
          `状态检查（端口 ${currentPort}）`,
          "更改端口",
          "查看日志",
          "退出",
        ]);
        if (choice === undefined || choice === "退出") return;

        if (choice.startsWith("状态检查")) {
          const { state, health } = await probe(currentPort);
          if (state === "up") {
            const v = health?.version ? ` v${health.version}` : "";
            ctx.ui.notify(`● 代理运行中${v} · 端口 ${currentPort} · 上游 ${HOST}`, "info");
          } else {
            ctx.ui.notify(
              `! 代理未运行（127.0.0.1:${currentPort} 无响应）\n请在终端启动：piglmbridger serve -d`,
              "warning",
            );
          }
          continue;
        }

        if (choice === "更改端口") {
          const input = await ctx.ui.input("更改端口", `当前 ${currentPort}，输入新端口 (1-65535)`);
          if (input === undefined) continue; // 取消
          const n = Number(input?.trim());
          if (!Number.isInteger(n) || n < 1 || n > 65535) {
            ctx.ui.notify("✗ 端口无效，未修改", "error");
            continue;
          }
          if (n === currentPort) {
            ctx.ui.notify(`端口未变化（${n}）`, "info");
            continue;
          }
          const r = writePort(n);
          if (r === "error") {
            ctx.ui.notify("✗ 写入配置失败", "error");
          } else {
            ctx.ui.notify(
              `✓ 端口已${r === "created" ? "创建配置并写入" : "写入"} ${n} → ${CONFIG_PATH}\n` +
                `生效步骤：\n  1. 重启代理（终端运行 piglmbridger serve -d，已在跑则先停掉）\n  2. pi 里 /reload`,
              "info",
            );
          }
          continue;
        }

        if (choice === "查看日志") {
          const lines = await fetchSummaries(currentPort, 50);
          if (lines === null) {
            ctx.ui.notify(
              `! 代理未运行或拉取失败（127.0.0.1:${currentPort}/logs）\n请在终端启动：piglmbridger serve -d`,
              "warning",
            );
          } else if (lines.length === 0) {
            ctx.ui.notify(`● 暂无请求记录（端口 ${currentPort}）`, "info");
          } else {
            // 每请求一行的单行摘要；行多时取末尾展示
            const text = lines.map(colorize).join("\n");
            ctx.ui.notify(text, "info");
          }
          continue;
        }
      }
    },
  });
}
