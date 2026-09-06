// pi-glmbridger：pi 接入 piglmbridger 的扩展 + /bridger 交互管理命令。
//
// 安装：pi install npm:pi-glmbridger
// 鉴权：pi 里 /login 选 zai，填智谱(open.bigmodel.cn) 的 API Key。
// 数据流：pi -> http://127.0.0.1:${port} -> https://open.bigmodel.cn/api/paas/v4
// 端口来源与 Rust 代理一致：env PIGLMBRIDGER_PORT（旧 GLM_FIX_PROXY_PORT 兼容）> ~/.piglmbridger/config.toml 的 port > 8123
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { readFileSync, writeFileSync, existsSync } from "node:fs";
import { execFile } from "node:child_process";
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

/** 执行 piglmbridger 子命令（3s 超时），返回 { ok, output } */
function runCli(args: string[]): Promise<{ ok: boolean; output: string }> {
  return new Promise((resolve) => {
    execFile(
      "piglmbridger",
      args,
      { timeout: 3000, encoding: "utf8" },
      (err, stdout, stderr) => {
        const output = `${stdout ?? ""}${stderr ?? ""}`.trim();
        resolve({ ok: !err, output: output || (err ? String(err) : "") });
      },
    );
  });
}

const INSTALL_HINT =
  "未找到 piglmbridger 二进制。安装：\n" +
  "  curl -fsSL https://github.com/Titor-Z/piglmbridger/releases/latest/download/install.sh | bash\n" +
  "（或到 https://github.com/Titor-Z/piglmbridger/releases 下载对应平台二进制）";

export default function (pi: ExtensionAPI) {
  const port = resolvePort();

  // 与旧扩展保持一致：改道内置 zai provider 到本地代理
  pi.registerProvider("zai", {
    baseUrl: `http://127.0.0.1:${port}`,
  });

  pi.registerCommand("bridger", {
    description: "piglmbridger 代理管理（状态/端口/服务/日志）",
    handler: async (_args, ctx) => {
      let running = true;
      while (running) {
        const currentPort = resolvePort();
        const choice = await ctx.ui.select("piglmbridger 管理", [
          `状态检查（端口 ${currentPort}）`,
          "更改端口",
          "服务控制",
          "查看日志（提示命令）",
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
              `! 代理未运行（127.0.0.1:${currentPort} 无响应）\n启动：piglmbridger service start -d`,
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
                `生效步骤：\n  1. piglmbridger service restart --port ${n}\n  2. pi 里 /reload`,
              "info",
            );
          }
          continue;
        }

        if (choice === "服务控制") {
          const action = await ctx.ui.select("服务控制", ["start（后台）", "stop", "restart", "status"]);
          if (action === undefined) continue;
          const arg = action!.split(" ")[0] as "start" | "stop" | "restart" | "status";
          const args = arg === "start" ? ["service", "start", "-d"] : ["service", arg];
          const r = await runCli(args);
          if (!r.ok && /ENOENT|not found/i.test(r.output)) {
            ctx.ui.notify(INSTALL_HINT, "error");
          } else {
            ctx.ui.notify(r.output || (r.ok ? "✓ 完成" : "✗ 失败"), r.ok ? "info" : "error");
          }
          continue;
        }

        if (choice.startsWith("查看日志")) {
          ctx.ui.notify(
            `在另一个终端运行：\n  piglmbridger logs -f\n（从本次启动处回放并实时跟踪 ${join(homedir(), ".piglmbridger", "logs", "proxy.log")}）`,
            "info",
          );
          continue;
        }
      }
    },
  });
}
