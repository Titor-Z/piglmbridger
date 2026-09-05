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

### 0.4.0
- **修复**【内存泄漏·重构回归】：v 模块化加测试捕获时，`Logger.captured` Vec 在 `write_file`/`write_line` 无条件 push，生产进程每条日志永久驻留内存 → RSS 随会话数单调上涨。修复：`captured` 改 `Option<Arc<Mutex<Vec>>>>`，仅 `Logger::memory()` 为 `Some`，生产构造一律 `None`；新增单测 `captured_vec_must_not_grow_in_production`（生产 empty + memory 捕获）防回归。
- **修复**【重构回归·结束行双打】：模块化重构时 `if done { return None }` 守卫从 unfold 闭包迁入 `StreamState::step()` 时被遗漏，导致 `[DONE]` 收尾后循环继续跑到 `Ok(None)` 分支再打一次结束行。修复：`step()` 开头恢复 done 守卫；集成测试补"结束行仅一次"回归断言。
- **重构**【OOP 模块化】：main.rs 1327 行拆为 lib+bin 架构：`stream.rs`（SseNormalizer 纯逻辑，274 行）+ `proxy.rs`（转发核心，385 行，新增 `StreamState` struct 收敛原 14 项 unfold 元组）+ `state.rs`（AppState/NotifyDrop/状态行辅助，132 行）+ `main.rs` 瘦身为入口（615 行）+ `lib.rs` 暴露模块供集成测试复用。行为零变更，重构后 mock SSE 端到端复验通过。
- **新增**【集成测试】：`tests/integration.rs`（mock SSE 上游 + 真实 axum 链路）四用例：① 正常收尾断言结束行含 req/resp/150 tok/首包 ② 上游残帧切断 → 带内 error 帧 + “上游异常切断” ③ 非 glm-5.3 字节级直通逐字节一致 ④ 令牌鉴权 401 本地拦截。Logger 新增 `memory()` 捕获模式供测试断言日志行。单测 11 + 集成 4 全绿。
- **修复**【重要·结束行丢失】：SSE 正常收尾时结束行/动画/统计全部不打印。根因：收到 `[DONE]` 后置 `done=true` 下发末批行，下一轮 poll 走 `if done { return None; }` 提前退出，`Ok(None)` 收尾分支（结束行所在地）永远执行不到，随后 body 被 drop 动画也被擦掉。修复：在 `done_seen` 置位处就地调用 finish_request + cleanup_stream（含真·首包/总耗时/↑↓字节/tokens）。此前冒烟全是 401/非 SSE 路径，未覆盖正常 SSE 收尾——补了 mock SSE 上游端到端验证（含 usage 帧）。教训入 K10。
- **精炼**【↑↓ 字节方向图标】：① 开始行/结束行/动画行统一 ↑（上行请求体，绿）/↓（下行响应，蓝）：`▶ [id] glm-5.3-flash → bigmodel.cn/chat/completions ↑ 357.6KB` → `⠹ [id] ↓ 48.2KB…` → `✔ [id] 200 · +5.2s · ↑ 357.6KB · ↓ 96.4KB · 1250 tok`；② finish_request 新增 req_bytes 参数（9 处调用点同步），结束行进出对比一目了然；③ 删除“请求/返回/POST”字样，图例职责归 README（`↑ 上行 ↓ 下行 tok`）；④ 文件行同步紧凑措辞 `req X resp Y`（保留“耗时/首包”中文供 grep）。单测 11/11。
- **升级**【原位动画状态行】：① 传输中进度不再占新行：终端单行原位刷新（`\r\x1b[2K` + spinner/字节数），结束时擦除——终端永远只留开始/结束两条历史，中间数据只进文件（`… ↓ 19.2KB` 每 3s 一条纯文本行）；② 并发安全：全代理共用一条状态行（`active_streams: Arc<Mutex<HashMap<req_id, bytes>>>`），单流 `⠹ [id] ↓ 48.2KB…`、多流聚合 `⠹ 2 个流 · ↓ 128.4KB…`，任何普通日志行打印前先擦动画（tty_prepare），流收尾/客户端断开（NotifyDrop::drop）统一 cleanup_stream 移除+刷新；③ 控制符安全：update_status 仅 TTY+彩色生效，管道/文件/daemon 零影响；④ 首报提前到 1s（last_report 初始化回拨 2s）；⑤ 措辞自解释：开始行 `↑ 6.1KB`、进度 `↓ X…`（图例见 README）。单测 11/11。
- **重构**【日志语言极简化】：① 每请求压到三行：`▶ [id] {model} → bigmodel.cn/chat/completions POST 6.1KB`（shorten_upstream 用 reqwest::Url，host 去 open./api. 前缀，零新依赖；文件行保留完整 URL）② 传输中不再死寂：节流活性行 `⠹ [id] 48.2KB ↑`（≥3s 或 ≥256KB 触发，spinner 轮转）③ 结束行纯指标串：`✔ [id] 200 · +5.2s · 首包 812ms · 1250 tok · 96.4KB`——首包=真·第一个上游 chunk（不是响应头），tokens 从 usage 帧提取（SseNormalizer 仅对含 "usage" 的完整行 JSON 解析，透传字节零改动，直通路径不碰）④ 删除“SSE 流开始/转发至/非 glm-5.3 不归一化”等口水话，实现细节降级 debug；非 2xx 统一 `✘` 红字短语（如“上游拒绝”），idle 中止 `✘ 504 … 读空闲中止`。单测 11/11（新增 shorten_upstream/usage 提取含跨 chunk 拼接回归）。
- **美化**【终端日志极客化】：① 请求生命周期行重设计：`▶ [req_id] POST /path → upstream` / `✔ [req_id] 200 OK +3987ms 105B 详情`（图标 ✔/✘ 按 ok、状态码按类别着色、耗时 fmt_duration <1s 用 ms）② 文件日志保持纯文本旧格式（`-> POST … 转发至 …` / `<- {status} … 耗时 …`），`logs --follow`/grep/轮转零影响；daemon file_only 模式终端零输出不变 ③ 新增 fmt_duration/fmt_bytes 单测 + 多字节不 panic 回归，总计 8/8。设计上否决了 DeepSeek 方案里的 `println!` 直印（会绕过 file_only/文件日志）、`colored`/`lazy_static` 依赖（手工 ANSI 即可、耗时直接传参不需 HashMap）。
- **清理**：移除 v0.3.0 遗留的死代码 CancellationToken（从未 cancel，实际取消靠 drop 传播），卸掉 `tokio-util` 依赖；补注释说明取消透传真实机制。- **新增**【v0.3.0 架构加固】：① 模型感知：仅 `glm-5.3*` SSE 归一化，其它模型字节级直通 ② 带内错误帧：断流/读空闲时丢残帧后补发标准 `error` 事件 + `[DONE]`（不是伪造数据帧）③ 取消透传：客户端断开经 NotifyDrop 实锤上游中止 ④ Header 白名单（仅 Authorization/Content-Type/Accept/User-Agent）⑤ `idle_timeout_secs` 读空闲看门狗（默认 120s）⑥ `auth_token` 远端令牌鉴权 ⑦ `--log-level debug` + `rejoined` 跨块拼接计数 ⑧ 区域/key 启动提醒 + doctor 401 提示。CI windows 首发再修一处（missing CommandExt import）。全 7 项均端到端实测通过（401 拦截、idle 触发错误帧、取消透传 debug、直通分支、白名单滤头）。
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

### D09 — 取消透传机制澄清 + 死代码清理
背景：用户确认 Esc 停止时是否立即停止上游传输（token 浪费关切）。
**结论**：取消透传成立且靠 drop 传播——pi abort → hyper drop body → unfold 状态（含 reqwest bytes_stream）一并 drop → 上游连接立即关闭。但 v0.3.0 引入的 CancellationToken 是死代码（从未调用 cancel()，`let _ = token` 直接丢弃），已从 unfold 元组中全部移除，并卸掉 Cargo.toml 的 `tokio-util` 依赖；在原位置留注释说明真实取消机制。测试 6/6 通过。注：K05 的「取消透传」叙述以本条为准（机制是 drop 传播，非显式 token）。

### D08 — 剩余提示
想彻底理解 GLM vs OpenAI 的 tool 拆包差异时，抓 OpenAI 兼容 SDK 时序（delta.role/content/tool_calls index 语义），不要假设所有兼容端点 chunk 语义完全一致。

### D10 — 终端日志美化：采纳“意图”否决“实现”（DeepSeek 方案纠偏）
背景：用户嫌终端输出单薄，DeepSeek 建议用 `colored`+`lazy_static`+`println!` 重写 logger.rs。
**结论**：目标采纳（时间轴/图标/耗时/对齐），实现三处否决：① `println!` 直印会绕过 file_only（daemon 污染终端）、文件日志、`--color` 管道逻辑——必须走 Logger 双通道；② `HashMap<req_id, Instant>` 多余——调用点已持有 `start: Instant` 直接传参，零新增状态；③ `colored` crate 多余——项目已有手工 ANSI 基建（K09 的 colorize_req_ids），零新依赖。另发现并修正：状态码颜色应按类别（2xx 绿/4xx 5xx 红）而非随 ok 标志（否则 401 被标绿），图标才随 ok（✔/✘）。实测：`✔ [2bb668] 401 +237ms 105B` 红色状态码 + 绿色对勾，文件行纯文本可 grep。

### D12 — 日志语言设计：符号承载状态，数字承载信息，零口水话
背景：用户认为 v1 美化后仍有“傻乎乎的话”（`POST /v1/chat/completions → 全 URL`、`SSE 流开始`、`非 glm-5.3 不归一化`），且传输期间一片死寂，无法回答“现在什么状态”。
**结论**：① 一行开始（模型+缩写目标+请求体大小）、节流活性行、一行纯指标结束，实现细节全部降 debug；② 删除“SSE 流开始”行的理由：它的耗时是响应头 TTFB，语义误导，真·首包（第一个上游 chunk）并入结束行更准更专业；③ usage 提取内聚在 SseNormalizer::normalize_line（仅含 "usage" 的完整 data 行才 JSON 解析，残帧绝不解析，直通路径字节零触碰）；④ 活性行走同一 Logger 双通道，spinner 只进 TTY，文件写纯文本 `… 传输中 X`；⑤ 非 2xx 一律 ✘+人类短语（“上游拒绝”），ok 标志跟 status.is_success() 而非流程成败（401 是转发成功但上游拒绝）。教训：finish_request 签名扩展时 6 处调用点要一次性同步，否则 E0061；unfold 元组已 13 项，下次再加状态考虑收敛成 struct。

### D13 — 原位动画状态行：单行共享 + 擦除式生命周期
背景：用户要求“中间过程是动画，跑完终端只留开始/完成两条，数据进文件”。
**结论/坑**：① 并发糊屏是最大风险——多流各刷各的行必交叉穿插，故全代理共用一条状态行，多流显示聚合（`N 个流 · 回传 X`）；② 普通行打印前必须先擦动画（tty_prepare），否则错误日志会拼在动画行尾；③ 流收尾有三条路径（正常 Ok(None)/idle 中止/客户端断开 NotifyDrop::drop），漏掉任一都会残留过期动画或泄漏 active_streams 表项——Drop 里做 cleanup 是兜底；④ 状态行擦除语义是“消失”而非“提交为历史”（用户明确要求终端只留两条），文件里才是完整中间过程；⑤ `\r\x1b[2K` 控制符只允许在 color && !file_only 分支出现，管道安全是底线（K09 同族纪律）。

**D12 补充**：↑↓ 图标决策——用户问“用图标是否可删掉‘请求’文字”，结论：可以，且“教学职责归 README 图例不归日志行”（git 的 +/- 同理）；开始行时响应不存在故只有 ↑，结束行 ↑↓ 成对出现方向对比自解释；开始行不发 POST 字样（文件行保留完整 method/URL 供 grep）。

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

### K10 — 状态机提前 return 会跳过“收尾分支”，流类代码必须验证正常路径 (⚠️ 高优先级)
unfold/循环式流处理里，“完成标志置位后提前 return None”会让写在循环末尾（Ok(None) 分支）的收尾逻辑变成死代码。本次实测：`[DONE]` 置 done=true → 下一轮提前退出 → 结束行/统计/动画清理全部丢失；而所有冒烟都是 401/非 SSE 路径，正常 SSE 收尾从未被真实验证过。
**纪律**：① 流的收尾逻辑放在“完成事件发生处”（done_seen 置位点），不要放在“资源关闭分支”（Ok(None)）；② 新日志/统计功能必须用 mock SSE 上游端到端验证正常路径（usage 帧分片 + [DONE]），不能只用错误路径冒烟。

### K09 — 别用字节下标切片可能含中文的 String
`&s[pos..pos+8]` 这种字节切片在多字节字符中间会直接 panic（`is not a char boundary`）。本次 colorize_req_ids 在中文日志行上就是被实测打中的。
**纪律**：非 ASCII 可能出现的字符串切片一律用 `s.get(a..b)`（返回 Option）或 `char_indices`。

### K11 — 给测试加的可观测字段必须带开关，无条件 push 的 Vec 是隐性泄漏
Logger.captured（模块化重构时为集成测试断言日志行而加）在 write_file/write_line 无条件 push，生产 daemon 每条日志永久驻留 → RSS 随会话单调涨，症状恰似“会话泄漏”。
**纪律**：① 为测试改共享结构时，开关必须默认关（`Option`/feature gate），生产构造路径显式为空；② “随请求/会话数量线性增长的全局容器”是泄漏速查项——凡是 `Arc<Mutex<Vec>>`/全局 map，逐条列出所有 remove/clear 路径；③ RSS 判断：随会话涨且活跃流为 0 = 真泄漏；涨到平台后稳定 = 分配器留存，勿混谈。

## 附：相关路径速查
| 项 | 路径 / 命令 |
|---|---|
| 源码 | `~/piglmbridger/src/`（main/proxy/stream/state/logger + lib.rs） |
| 配置 | `~/.piglmbridger/config.toml` |
| 日志 | `~/.piglmbridger/logs/proxy.log`（logs -f 看） |
| pi 扩展（本仓库参考版） | `pi/extensions/piglmbridger.ts` |
| pi settings 片段（本仓库参考版） | `pi/settings.glm-snippet.json` |
| pi 扩展（已安装） | `~/.pi/agent/extensions/piglmbridger.ts` |
| pi settings | `~/.pi/agent/settings.json` |
| 构建/测试 | `cargo build --release`、`cargo test`（单测 11 + 集成 4） |
| 运行 | `./target/release/piglmbridger serve [--port]` |
