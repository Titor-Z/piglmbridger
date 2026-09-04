# AGENTS.md

> 本项目（`piglmbridger`）的工程记忆 / 协作笔记本。
> 供 **AI 代理（任何 agent）** 与 **人类协作者** 共用，作为唯一的“状态快照 + 决策+踩坑溯源”。
>
> 读取约定：
> - 改代码前先读「进度」「认知纠正」，避免重复踩同一个坑。
> - 每次讨论/决策后，在「讨论」「认知纠正」追加一行。
> - 每次发版前，把最新改动整理进「更新日志」用于 GitHub Release 说明。
> - 本节文件由 pi 自动加载为项目上下文。

---

## 1. 更新日志 (Changelog)

> 给 GitHub Action 发 GitHub Release 时用的正文。按最新→最旧排列，HEAD 即下一个待发版内容。

### [Unreleased]
- **新增**【v0.3.0 架构加固】：① 模型感知：仅 `glm-5.3*` SSE 归一化，其它模型字节级直通 ② 带内错误帧：断流/读空闲时丢残帧后补发标准 `error` 事件 + `[DONE]`（不是伪造数据帧）③ 取消透传：客户端断开经 NotifyDrop 实锤上游中止 ④ Header 白名单（仅 Authorization/Content-Type/Accept/User-Agent）⑤ `idle_timeout_secs` 读空闲看门狗（默认 120s）⑥ `auth_token` 远端令牌鉴权 ⑦ `--log-level debug` + `rejoined` 跨块拼接计数 ⑧ 区域/key 启动提醒 + doctor 401 提示。CI windows 首发再修一处（missing CommandExt import）。全 7 项均端到端实测通过（401 拦截、idle 触发错误帧、取消透传 debug、直通分支、白名单滤头）。
- **新增**【CLI 大版本】：① 守护进程化 `start/stop/restart/status`（进程名 `piglmbridged`，pid 文件 + stale 清理 + 优雅退出最长等 30s）② `doctor` 体检子命令（配置/端口/上游探活）③ `--addr` 与代理端 env 通道（`PIGLMBRIDGER_ADDR`，优先级 CLI > env > config > 默认）④ 日志 TTY 自动着色（ERROR 红 / req_id 青色成组），管道自动纯文本，`--color` 可强制；修复 colorize 在中文行上的字节切片 panic（改 `get()` 安全切片，附单测）⑤ 退出时打印本次运行统计（请求数/残断次数/时长）。修复前端口占用直接 panic，现在输出可读错误并 exit(1)。
- **CI**：新增 GitHub Actions 发布流水线（`.github/workflows/release.yml`）：推 `v*` tag 自动交叉编译 5 平台（macOS arm64/x64、Windows x64、Linux x64/arm64 musl 静态）并发布 Release；说明文字自动取自本文件 `[Unreleased]` 段。依赖前提：`reqwest` TLS 从 native-tls 切到 **rustls**（纯 Rust，消除 OpenSSL 交叉编译地狱）。
- **更名**：项目 `glm-fix-proxy` → **`piglmbridger`**（二进制/包名/配置目录 `~/.piglmbridger` 同步更名；旧路径留软链兼容；env `PIGLMBRIDGER_PORT` 兼容旧 `GLM_FIX_PROXY_PORT`）。
- **新增**：仓库附带 pi 接入资产 `pi/` 目录（`pi/extensions/piglmbridger.ts` + `pi/settings.glm-snippet.json`），代理与 pi 侧配置一套交付；README 补安装说明。
- **修复**【重要·根治间歇性断帧】：修正 agent SSE 行重组逻辑
  - 之前：某次上游网络块既含完整 `data:` 行、又含半截未写完的行时，默认其“整块无换行才等下一块”，导致半截 JSON 被当成完整帧下发给 pi → `Unterminated string in JSON` / 反复重试。
  - 现在：仅将真正以换行/空行收尾的片段下发给下游；未写完的尾部一律留缓冲，待下一块拼齐后再发；绝不伪造残缺帧。修复前触发场景约 60% 失败，修复后压测 14/14 通过。
- **新增**：异常切断的上游段落不再被“补换行伪造”成数据帧，而是显式丢弃并记 `[ERROR]`（`drain_abrupt`），杜绝 `Unterminated string`。
- **新增**：CLI 子命令 `serve`/`logs`；`--port/--upstream/--timeout` 参数；配置文件 `~/.piglmbridger/config.toml`（优先级 CLI>配置>默认）。
- **新增**：文件日志（`~/.piglmbridger/logs/proxy.log`，自动轮转）+ `logs --follow/--lines` 实时跟踪与查看。
- **新增**：5 个单元测试（含“同块完整行+残缺尾行跨块拼接”回归用例）。
- **工程**：新增 `README.md`、本文件 `AGENTS.md`。

### 0.1.0 (草案)
- GLM-5.3-Flash SSE 修复中转代理初版起跑：UTF-8 字节边界缓冲（中文不切半）、SSE 行拼接、空 json `[DONE]` 去重、透传鉴权、面向 pi 的 zai provider 接入示例。

---

## 2. 讨论 (Discussion / 溯源)

> 记录“为什么这么做”的决策链路，按时间正序。每轮 = 背景 / 结论。

### D01 — 现象
用户用 pi + GLM-5.3-Flash 时：①输出断断续续/一半卡住 ②不停重试 ③报 `Unterminated string in JSON`；换 DeepSeek-V4-Flash 则完全正常。怀疑代理返回的数据流里 GLM 意外退出导致。
**结论**：需分多层定位，先列排查假设（见 D02–D06）。

### D02 — 假设 A：pi retry 设置太激进
把 `~/.pi/agent/settings.json` 的 `retry.maxRetries` 从 3 降到 1、`baseDelayMs` 提到 6000、`provider.timeoutMs`=120 000、`httpIdleTimeoutMs`=120 000。
**结论**：配置里并不存在 `streaming.chunkParseStrict` 与顶层 `retry.maxDelayMs`，是无效键，已剔除。仅是“症状缓解”，不是根因。

### D03 — 接入方式：用扩展改 zai baseUrl 而非 /login 自配
pi 的 `/login` 里无法添加自定义 OpenAI provider；发现 pi 内置 `zai` provider（含 glm-5.3-flash）。方案：`~/.pi/agent/extensions/piglmbridger.ts` 覆盖 `zai` 的 baseUrl 到本地代理，端口可由 `PIGLMBRIDGER_PORT（旧名 GLM_FIX_PROXY_PORT 仍兼容）`（默认 8123）配置。
**结论**：不动脑搭建复杂 provider，直接复用内置模型 definition 与鉴权通道；端口需要与代理配置 / config.toml / CLI `--port` 三处同步（见 README）。

### D04 — 抓住第一个真实 bug：断流时伪造半帧
上游把 `data:` 截半后断流，代理在流结束分支补了个换行把半截 JSON 发出去 → 触发下游 `Unterminated string`。【先被误判为最有嫌疑，回头看只是 D07 bug 的一个特例】
**结论**：绝不能补 `\n` 伪造帧；改为丢弃残帧 + `[ERROR]` 计日志（drain_abrupt）。

### D05 — 深入“为什么 DeepSeek 行、GLM 不行”
查智谱官方文档：GLM-5.3-Flash **强制深度思考**、`thinking.type` 仅支持 `enabled`、推荐 `tool_stream:true`、流式时先吐大量 `reasoning_content` 再吐 content/tool。查 pi 源码发现 `compat.thinkingFormat:"zai"` 分支在没开推理时会发 `thinking:{type:"disabled"}`，而 GLM 会拒它（code 1210）。
**假设 H1**：GLM 报错全因 “disabled” —— 
**真机纠偏（重要）**：`data` 级实际抓包，glm-5.3-flash 的 `off` 级会被 pi 的模型配置自动抬到 `low`，从不发 `disabled`。故 H1 **不成立**，1210 在当前配置下不会触发。（见「认知纠正 · K01」）
**真实结论**：GLM 会**把单个 `data:` 事件切成多个 TCP 块**发送；见 D06。

### D06 — 找到且修复真正的 root cause：同块内“完整行+残缺尾行”被误整帧
用加日志的方式转储上游每个块的原始字节尾部，发现形如 `…content":"R"}}]}}]}\n\ndata: {"id":"` —— 即一个网络块里 end 于一个“未收尾的新 `data:`”中间，后半截在下一块。
根因代码在 `SseNormalizer::push`：以“本块是否 contains('\\n')”判断是否等下一块，粒度太粗；一旦块里已有别的换行，末尾这半截残缺 `data:` 就被当作完整行 emit。
**修复**：改为只有 `segment ends_with('\\n')` 时，最后一个 split 片段才是完整行；不满足则整段留缓冲等下一块。另补回归测试。修复后触发流程 14/14 通过、复杂多步任务干净完成。
**结论**：此 bug 才是“DeepSeek 稳定 vs GLM 间歇错”差异的实质——DeepSeek 把一个 event 放在同一个干净的写里（很少跨越块边界+内容短），GLM 切得碎/常跨块，越靠行缓冲型代理越容易中招。

### D07 — settings 与代理作用边界
两层方案并存：pi settings 只管“重试次数/超时”（缓解），代理管“碎帧重组/去错/丢弃”（根治分片问题）。代理修复后即使 `maxRetries` 高也不至于疯狂刷屏。

### D08 — 剩余提示
想彻底理解 GLM vs OpenAI 的 tool 拆包差异时，抓 OpenAI 兼容 SDK 时序（delta.role/content/tool_calls index 语义），不要假设所有兼容端点 chunk 语义完全一致。

---

## 3. 进度 (Progress)

### 已完成 ✅
- [x] pi settings：宽松重试（maxRetries=1、basDelay=6000、timeoutMs=120000、httpIdleTimeoutMs=120000）；已剔除无效键。
- [x] Rust 代理功能集齐：`serve`/`logs` 子命令、`--port/--upstream/--timeout`、`config.toml`、文件日志 + follow、日志轮转。
- [x] pi 扩展 `piglmbridger.ts`：zai baseUrl → 本地代理；端口可用 `PIGLMBRIDGER_PORT（旧名 GLM_FIX_PROXY_PORT 仍兼容）`（默认 8123）改。
- [x] settings 保险：modelThinkingLevels 里把 glm-5.3 系列默认思考钉到 `low`（防用户手动拉到 off 的裸奔 1210）。
- [x] 修复同块“完整行+残缺尾行”误整帧（root cause，[Unreleased]）。
- [x] SSE 断流残帧丢弃逻辑（不伪造）。
- [x] 单元测试 5/5 通过。
- [x] 真实自测：修复前 5 次失败~3；修复后 14 次多工具任务 0 失败；更重的多步 Python + bash 任务干净完成，代理日志无 error。
- [x] 仓库附带 pi 接入资产 `pi/`（扩展 + settings 片段）并写入 README 安装说明。
- [x] README.md、AGENTS.md。
- [x] git 初始化并提交检查点。

### 待办 / 下一步 🔜
- [ ] python lib 之类引用的 code 文件 artifact 精确性由 agent 描述保证（本次 script.py 里被模型混入了它声称之外的 timestamp 行——是模型/提示词产物，不是代理 bug，如需可加例程剔除杂散行）。
- [x] 守护进程化（piglmbridged）+ doctor + 颜色日志 + --addr（v0.2.0）。
- [x] stats 统计子命令（stats.jsonl 落盘 + 汇总展示）；pi 扩展更名 piglmbridger.ts；LICENSE(MIT) + README 徽章 + 仓库转公开。
- [x] v0.3.0 架构加固：模型感知/错误帧/取消透传/Header白名单/读空闲看门狗/令牌鉴权/log-level。
- [ ] 可选：launchd / systemd 开机自启示例文档。
- [ ] 可选：加更细粒度指标（每流字节数/耗时打进另一张表），方便灰度期观察。
- [ ] 接入 GitHub Action 发布（workflow 已写好 `.github/workflows/release.yml`；待重启会话后本地验证 rustls 构建通过 + git commit + 推 tag 首发实测）。
- [ ] 确认 .git / CI 接入；规划 GitHub Action 用 AGENTS.md「更新日志」生成 Release。（workflow 已就绪，待验证）
- [ ] 等用户在真实场景再压一轮后,更新 D08/认知纠正。

---

## 4. 认知纠正 (Cognitive Corrections / 已知盲区)

> 供守同一工程的不同 agent 共用“已踩的坑”，避免再次踩。

### K01 — 读 pi 源码 ≠ 实际行为，别据此下结论 (⚠️ 高优先级)
pi 里 `compat.thinkingFormat:"zai"` 分支看起来会发 `thinking:{type:"disabled"}`，看似与 GLM “不支持 disabled”冲突 → 一度以为这就是根因（H1）。
**真相**：glm-5.3-flash 的 `thinkingLevelMap.off = null`，pi 会把它“钳高”成最接近的支持档（low），所以**从不真的发 disabled**。
**纪律**：凡是“模型拒我 / 报错语义出现在接口层”的猜测，第一时间在真实 endpoint + 同账号抓实际请求体验证，而不是只读调用方源码。

### K02 — 一块上游网络碎片里可以有“完整行”+“半截未收尾行”同时出现
不能拿“本块含不含 \\n”来判断残尾——那会误把半截 JSON 当完整帧。正确判据：**本段是否以换行结尾**。真实世界里 SSE 内容行边界与 TCP 写边界绝对不对齐，是常态不是异常。

### K03 — SSE 断流 ≠ 要补换行把它“讲圆”
代理看到上游拔线，第一反应“把 buffer 里剩的补个换行发出去”是错的：那正是制造 `Unterminated JSON` 的作者。原则：**宁可丢弃半截帧并记日志，也不伪造“看着能解析”的完整帧**。（DeepSeek 很少断在帧中间；越像这类生态的模型越需要这条。）

### K04 — “深拷了官方文档的推荐参数” 与 “上游真实兼容性” 是两码事
GLM 官方约只有 payload 大体兼容；`tool_stream`、thinking 策略、分块习惯和 OpenAI/DeepSeek 并不完全一致。把某一兼容端点的“在真实抓包才发现的 chunk 切块习惯”直接当作通用事实是易错点。

### K05 — 客户端 abort 的会话不会留下“流结束”日志
当下游（pi/HTTP 客户端）提前断开 SSE，服务端的 unfold/body 会被 cancel，收尾日志不一定执行。别用“有开始没结束”去反推“上游断流”；要用连接/存活 socket 状态去判断。默认视为“客户端主动断开”，而非上游或代理故障。

### K06 — 判断“上游是否残缺”必须先区分两件事
1) 单个 `data:` 事件自身 JSON 不完整（不是换行问题，需要 JSON 级聚合）；
2) 事件内容被 TCP 切在中间（换行级行重组可解决）。
本次问题属（2）。若以后遇到“事件实际以 \\n 收尾但 JSON 仍不完整”，那是（1），处理方式不同，需要配另一套策略，别直接搬行重组逻辑。

### K07 — 别混淆“测试目标”与“修复目标”
写 8/8、14/14 这类“通过触发场景跑”只证明那一个触发没炸；不代表别的模型/别的切块也稳。发布说明要写明“验证覆盖的场景边界”，别夸张成“对所有 GLM 场景都根治”。

---

### K08 — 跨平台 cfg 门控代码在本地永远编译不到 (⚠️ 发版必踩)
`#[cfg(windows)]` 里的代码（如 `creation_flags` 缺 `CommandExt` 导入）在 mac 本地 `cargo check` 完全不报错，只有 Windows runner 能暴露。
**纪律**：① 写了 cfg 门控分支后，尽量跑一次 `cargo check --target x86_64-pc-windows-msvc`。注意局限：`rustls`→`ring` 依赖含 C/asm，**从 mac 交叉到 windows 需要目标 C 工具链**，本地会因无 toolchain 在 `ring` 构建脚本处报错——那是环境限制，不是代码 bug；Windows 原生 CI 可正常编 ring。所以该预检主要用来抓“纯 Rust 代码”的 cfg 错误；带 ring 时仍需靠 windows runner 实跑。② CI 红了先 `gh api .../jobs/<id>/logs`（加 `--allow-escape-sequences`）拉日志，错误都在最后几行。

### K09 — 别用字节下标切片可能含中文的 String
`&s[pos..pos+8]` 这种字节切片在多字节字符中间会直接 panic（`is not a char boundary`）。本次 colorize_req_ids 在中文日志行上就是被实测打中的。
**纪律**：非 ASCII 可能出现的字符串切片一律用 `s.get(a..b)`（返回 Option）或 `char_indices`。

## 附：相关路径速查
| 项 | 路径 / 命令 |
|---|---|
| 源码 | `~/piglmbridger/src/main.rs`、`src/logger.rs` |
| 配置 | `~/.piglmbridger/config.toml` |
| 日志 | `~/.piglmbridger/logs/proxy.log`（logs -f 看） |
| pi 扩展（本仓库参考版） | `pi/extensions/piglmbridger.ts` |
| pi settings 片段（本仓库参考版） | `pi/settings.glm-snippet.json` |
| pi 扩展（已安装） | `~/.pi/agent/extensions/piglmbridger.ts` |
| pi settings | `~/.pi/agent/settings.json` |
| 构建/测试 | `cargo build --release`、`cargo test` |
| 运行 | `./target/release/piglmbridger serve [--port]` |
