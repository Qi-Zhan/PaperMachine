# PaperMachine auto-research 工程评估（2026-08-07）

## 结论先行

当前框架已经可以真实运行由多个持久 Session 组成的 Python Workflow，并且能够在
DeepSeek Responses-compatible provider 上完成检索、并行研究、Evaluator 追查、写作和
独立 post-write grading。缓存也确实在工作；主要问题不是“每次都没有缓存”，而是多
Agent 会创建多个相互隔离的 Session，每条路线都要建立自己的前缀，因此总体 cache read
比例自然低于单 Agent。

本报告包含两轮可区分的 DeepResearch Bench mini 实验。第一轮 5 题 × 3 条件 × 2 次，
共 30 个研究结果和 30 个独立评分，发现 single-agent 偶尔把搜索过程播报误当最终交付物。
我们据此加入通用无工具 finalization 和显式 completion policy。随后使用当前 runtime 在
4 道困难题上运行 `single_agent`、`evidence_r3`、`evidence_r4`，每题每条件 2 次，共
24/24 research 和 24/24 grade 全部完成，且没有整条 Workflow 重跑。

当前 runtime 的复验结果更直接：single-agent 平均 **85.88**，高于 r3 的 82.07 和 r4
的 82.88，并在 4 道题的逐题均分和 4 个 rubric dimension 上全部领先。r3 相对 single
使用 3.13 倍 uncached input、5.24 倍 output 和 2.23 倍时间；r4 分别为 3.64、6.22 和
2.47 倍。r3 只有 1/8 真正进入后续 evaluator round，r4 只有 2/8；固定创建 3 或 4 条
研究路线，而不是 follow-up 本身，是大部分增量成本的来源。因此现有证据明确反对把
“更多 Agent / 更多 max_rounds”当作默认质量旋钮。

更合理的产品方向是：默认给强单 Agent 一个明确的完成契约；当问题的覆盖风险、证据
冲突或用户指定的 Workflow 需要时，再自适应地增加路线、Evaluator 和追查轮次。框架
的核心价值应是让用户舒服地表达这种协作结构，并提供持久化、权限、并发、定时、人类
介入、预算、缓存可观测性和可恢复执行，而不是内置一个永远正确的多 Agent 拓扑。

## 两轮实验到底测了什么

### 题目

使用 `deep_research_bench` 固定 revision
`469cce54ea7f6a63c163d3d9fec879cf289ec484` 的 5 道完整 rubric 题：

| ID | 主题 | 问题摘要 |
|---:|---|---|
| 19 | Prometheus | 高 churn 的影响、系统性解决方案和云厂商方案 |
| 59 | 鸟类迁徙 | 定位与定向机制、线索和干扰因素 |
| 66 | Obsidian | 能复现 Notion 多视图数据库的插件及优缺点 |
| 68 | Kubernetes | 预测式或计划式节点扩缩容策略、实践与项目 |
| 69 | A2A / MCP | 两种协议的联系、区别、创新点和目标问题 |

### 三个研究条件

- `single_agent`：一个持久 Researcher Session 自己检索、推理并直接写最终交付物。
- `evidence_r1`：Planner 冻结 coverage contract；至少两个 Researcher Session 并行
  建立 evidence ledger；Evaluator 评估一次；Writer 根据证据写作；Evaluator 审计
  草稿，必要时 Writer 修订。
- `evidence_r2`：结构同上，但第一次评估不通过时，Evaluator 可以把定向追查任务发回
  原有 Researcher Session，再评估一次。它不会为 follow-up 丢掉该路线历史。

研究 Agent 只看到原问题，评分 rubric 不会泄漏给研究 Workflow。最终文章由另一个
`model_only`、无工具、无研究上下文的 `report-grader` Session 按上游全部 criterion
逐项打分；Python 校验 criterion 索引并应用 criterion 与 dimension 两层权重。这个
分数适合本项目内的绝对 point-wise 对比，但不是上游需要参考答案和 FACT 的官方 RACE
分数。

研究模型和 grader 均为 `deepseek-flash` profile，对应 upstream
`deepseek-v4-flash`。同模型家族 grader 仍可能有 judge bias，但评分 Session 与研究
Session 是隔离的。

## 第一轮完整基线结果（completion 修复前）

| 条件 | Runs | 分数均值 | SD | Operational uncached input | Operational output | Cache read | 端到端时间 | 报告字符数 |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| single-agent | 10 | 75.91 | 26.65 | 60,200 | 17,308 | 89.3% | 267s | 19,092 |
| evidence-r1 | 10 | **85.58** | 4.29 | 263,450 | 110,452 | 60.3% | 942s | 20,221 |
| evidence-r2 | 10 | 85.01 | 4.07 | 327,392 | 125,249 | 67.1% | 1,424s | 22,438 |

`Operational` 包含成功 attempt 之前失败或结构无效的 attempt。时间取 runtime 自己记录
的执行时间与 Workflow `created_at -> updated_at` 的较大值，因此服务进程重启不会把
用户已经等待的时间抹掉。三条跨重启恢复的研究 Workflow 原先只记录了恢复后的
453、162、476 秒，修正后分别为 1,702、2,414、3,776 秒。

### 每题均分

| Task | single-agent | evidence-r1 | evidence-r2 |
|---:|---:|---:|---:|
| 19 | 82.65 | 83.69 | **85.78** |
| 59 | 82.25 | 87.77 | **88.50** |
| 66 | 80.73 | **84.99** | 80.87 |
| 68 | 42.39 | **89.26** | 84.91 |
| 69 | **91.51** | 82.17 | 84.97 |

多 Agent 不是一致获胜：在 A2A/MCP 题上 single-agent 明显最好，Obsidian 题的第二轮
Evaluator 也没有带来收益。`evidence_r1` 在 10 个成对样本中赢 6 次；`evidence_r2`
相对 r1 同样赢 6 次，但小幅胜利抵不过失败样本，均值仍降低。

`evidence_r2` 只有 5/10 个 run 真正触发了 follow-up。换言之，配置 `max_rounds=2`
并不等于一定执行两轮，这个 stop behavior 是正确的；问题在于触发后的边际价值尚未
证明。r1 的 10 个 run 按定义只做第一次 assessment，没有定向 follow-up。

### 灾难性单 Agent 样本

Task 68 的 single-agent 两次得分为 83.73 和 1.055。低分文章只有 615 个字符，内容
基本是“已经完成搜索，现在再核验几个来源”一类过程播报，没有回答用户问题，也没有
URL。这个 action 实际消耗了 30 个步骤、约 247k input 和 10k output，说明它不是没做
研究，而是 provider 在工具循环结束时把 progress narration 当成了最终 action output。

这个样本揭示的不是“需要固定两个 Researcher”，而是框架缺少通用 completion
contract：最终 action output 至少要满足目标交付物的基本形态，否则 runtime 应进入
一次无工具 finalization/repair，仍不满足时显式失败或请求人类，而不能把过程消息当成
成功。去掉这个样本只用于敏感性分析，不改变主表：single-agent 其余 9 次均分 84.22，
r1 的正常质量优势只剩约 1.36 分。

### 重复稳定性

平均 absolute repeat gap 为：single-agent 19.01、r1 5.21、r2 5.08。single-agent
的数值被上述 Task 68 失败主导；除去它，其他 single-agent 的重复差距并不异常。
小样本只够发现 failure mode，不能据此声称多 Agent 普遍降低方差。

## 当前 runtime 的高难高轮次复验

### 条件与可靠性

复验选择 Task 19、66、68、69，继续使用相同的上游完整 rubric、隔离的 post-write
grader 和 DeepSeek 模型。`single_agent` 的搜索 Action 在实际使用搜索工具后，会在同一
持久 Session 中自动追加一个无工具 `research_finalize` Turn；这既保留该 Session 的
研究上下文，又防止工具循环最后一条 progress message 成为最终报告。

`evidence_r3` 固定 3 条初始路线，最多 3 次 assessment、每轮最多 3 个 follow-up；
`evidence_r4` 对应 4/4/4。两者均使用 `deliver_with_warning` completion policy：最终
evidence evaluation 或 draft audit 未通过时可以交付，但必须返回机器可见的 warning
及原因。三组各 8 次，24/24 research、24/24 grade 完成；没有 terminal failure，也没有
整条 Workflow retry。

### 结果

| 条件 | Runs | 分数 | SD | Uncached input | Output | Cache read | 时间 | 报告字符 | 直接 URL |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| single-agent | 8 | **85.88** | **4.24** | **143,700** | **24,992** | **79.6%** | **353s** | 20,978 | **30.6** |
| evidence-r3 | 8 | 82.07 | 8.29 | 449,976 | 131,051 | 63.7% | 788s | 21,770 | 23.9 |
| evidence-r4 | 8 | 82.88 | 5.90 | 523,297 | 155,520 | 60.8% | 871s | 21,140 | 27.1 |

| Task | single-agent | evidence-r3 | evidence-r4 |
|---:|---:|---:|---:|
| 19 Prometheus | **89.23** | 86.13 | 86.73 |
| 66 Obsidian | **84.62** | 84.52 | 83.44 |
| 68 Kubernetes | **80.30** | 69.29 | 74.55 |
| 69 A2A / MCP | **89.37** | 88.33 | 86.78 |

rubric dimension 同样没有出现“总体分掩盖局部提升”的情况：single-agent 的
comprehensiveness / insight / instruction following / readability 分别为
83.56 / 82.25 / 93.88 / 87.44；r3 为 79.94 / 77.69 / 91.69 / 82.06；r4 为
81.94 / 79.44 / 89.19 / 83.50。r3 相对 single 的成对差为 -3.81 分，只赢 2/8；
r4 相对 r3 为 +0.81 分、赢 4/8，但仍比 single 低 3.00 分。

### Workflow 实际做了什么

| 条件 | Completion | 实际 round | 初始 research Action | 初始语义修复 | Follow-up research | Draft 修订 | 最终 audit fail |
|---|---|---|---:|---:|---:|---:|---:|
| single-agent | 未单独声明；finalization 全部完成 | n/a | 8 | 0 | 0 | 0 | 0 |
| evidence-r3 | passed 3 / warning 5 | round 1: 7；round 3: 1 | 25（预期 24） | 1，另有 1 次 JSON repair | 6 | 5 | 2 |
| evidence-r4 | passed 6 / warning 2 | round 1: 6；round 2: 1；round 3: 1 | 36（预期 32） | 4 | 5 | 6 | 1 |

这里必须区分三种“重试”：整条 Workflow 重跑为 0；同一个 `ActionInvocation` 的
`ActionAttempt` retry 为 0；Python Workflow 发现 evidence packet 违反语义契约后重新
调用同一 Researcher 的修复 Action，r3 有 1 次、r4 有 4 次。旧报告只统计第一类，会把
后两类遗漏；runner 现在从 durable Action journal 回填并分别报告这些指标。

r3/r4 的最终报告长度与 single 相近，但直接 URL 更少，外部评分也更低。这支持一个尚需
更多实验验证的工程假设：当前主要瓶颈是 evidence ledger 到 Writer 的信息压缩与来源
保真，而不是搜索路线不足。盲目增加路线会扩大 handoff 面积，并不自动扩大最终答案的
有效覆盖。

### completion 修复是否解决了原 failure mode

用相同 4 道题比较修复前基线中的 single-agent 样本与当前复验，均分从 74.32 变为
85.88，SD 从 29.97 降到 4.24，平均重复差从 23.05 降到 2.45；当前 8 个结果全部是完整
报告，最低分 78.89，没有再次出现 615 字符的 progress-only 输出。这是很强的工程回归
信号，但不是只改变一个变量的随机对照实验，因此不能把全部 11.56 分提升都因果归于
finalization。

## 缓存 deep-dive 结论

缓存是有效的：第一轮 single-agent aggregate cache read 为 89.3%，provider probe 也
出现 90.3% 和 94.6%；当前复验的 single / r3 / r4 分别为 79.6% / 63.7% / 60.8%。
DeepSeek 当前走 HTTP SSE 和 implicit prompt caching，不支持本项目所用的 Responses
WebSocket continuation，因此 continuation hit 为 0；这和 provider 返回的 cached input
是两个不同机制。

多 Agent 的 cache read 较低并不表示 key 每轮变化。PaperMachine 的 routing key 在
同一 Session 内稳定，并且每个 Turn 保存准确 prompt snapshot。多路线 Workflow 会创建
Planner、多个 Researcher、Evaluator、Writer 等独立 Session；每个 Session 有自己的
安全隔离 cache namespace 和不同 instruction/history prefix，不能假装它们共享一段连续
会话。r2 的 67.1% 高于 r1 的 60.3%，正是因为它把 follow-up 发回已有 Researcher
Session，能复用该路线历史，而不是重建新 Agent。

因此优化目标不应是强行让不同 Agent 共用 cache key，而应该是：

1. 保持 Agent Session 持久，follow-up 回到原 Session；
2. 保持 runtime/project/workflow/agent prompt 前缀稳定、可检查；
3. 大 context 用语义 compaction，而不是每 Turn 重发无限历史；
4. UI 同时显示 raw input、cached input、uncached input、cache mode/key 和 compaction；
5. 用 provider capability profile 区分 continuation、显式 breakpoint、隐式 cache 和
   tool-call limit 是否真的生效。

当前单 Agent、r1、r2 的 uncached input 比为 1 : 4.38 : 5.44；output 比为
1 : 6.38 : 7.24。当前复验 single、r3、r4 的 uncached input 比为
1 : 3.13 : 3.64，output 比为 1 : 5.24 : 6.22。即使缓存有效，多 Agent 的真实增量
成本仍然很大。

## 这轮实践发现并修复的框架问题

1. **Project 目录可被旧数据库记录复用但磁盘目录已不存在。** 三个 benchmark runner
   现在都会在复用 Project 前重建 root；提交 `f2123eb`。
2. **兼容 provider 接受 `max_tool_calls` 却不执行。** 现在会从实际响应检测 violation，
   降级到 runtime 自己计数；提交 `aced188`。一个响应内部仍可能 overshoot，说明预算
   必须在 framework 层最终兜底。
3. **Python Workflow 大 effect response 卡死。** 约 64 KiB 以上 JSONL response 超过
   asyncio 默认 StreamReader limit，三个 Workflow 在 action 已完成后永久等待。协议
   limit 已提升到 16 MiB，reader failure 会传播给所有 pending Future，不再静默挂起；
   提交 `8c0fab9`。
4. **服务重启丢失 wall time。** scheduler 的 runtime usage 只累计一次 process execution
   返回的时间，恢复前已等待的时间没有进入 usage。三个 runner 现在同时保留
   `runtime_wall_time_seconds` 和端到端 `wall_time_seconds`，并能回填旧 state；提交
   `2158d02`。
5. **结构化 action 失败会重跑整个 Workflow。** Task 69 的一个 r1 job 因
   `findings must be an array` 完整重试两次，浪费约 260k operational uncached input。
   evidence-loop 现在先在原 Researcher Session 内执行局部语义 repair，当前复验没有整条
   Workflow retry；但原生 checkpoint/retry 粒度最终仍应降到 ActionAttempt，并在 provider
   支持时使用 schema-constrained output。Workflow replay 只应作为 crash recovery。
6. **Draft audit 失败后没有统一的完成政策。** r1 有 2 次、r2 有 4 次最终 audit 仍为
   false。现在 Workflow 必须明确选择 `deliver_with_warning`、`wait_for_human` 或
   `fail_run`；等待人类时支持 `/deliver`、`/fail` 或把自由文本作为带来源的 Writer
   revision Turn，提交 `f311284`。
7. **工具循环可能把 progress narration 当最终报告。** 通用 Action DSL 新增
   `finalize="after_search"`，若首个 Action 使用过 hosted search，就在同一 Agent Session
   上追加无工具 finalization Turn；当前 8/8 single-agent 报告均正常，提交 `f311284`。
8. **只统计整条 Workflow retry 会漏掉内部修复。** runner 现在同时统计 Action 名称、
   research phase、初始 contract repair、follow-up、同一 Action 的 attempt retry 与
   completion status，并可从 durable journal 回填旧 state，提交 `adf7f15`。

## 其他 benchmark 的证据边界

### LiveDRBench mini（完整但来自较早 runtime）

7 题 × 3 条件 × 2 次均完成。独立语义 grader 的 aggregate F1：single-agent 0.586、
r1 0.387、r2 0.411；uncached input 分别为 36,794、140,658、237,418。Task 40 所有
条件都为 0，Task 47 和 66 的 single-agent 明显优于多 Agent。这组数据再次说明固定
coverage/evaluator graph 可能把强单 Agent 已经找到的精确答案在 handoff 或筛选阶段
损坏，而不是稳定增强它。

它运行于本轮大响应、provider limit 和 wall-time 修复之前，因此只作为诊断证据，不能
和主表直接合并。

### BrowseComp mini（失败诊断，不能报准确率）

旧 6 题矩阵的 36 个 job 中只有 23 个 research 完成，且没有形成可用的完整 grade
矩阵；报告里“0% accuracy”主要反映运行失败，不是模型质量。后续 Task 788 单点 probe
成功找到 UFC 219 并通过独立 grader，cache read 90.3%；第二次 probe 因 provider 忽略
工具调用上限而触发 runtime budget 失败。它验证了 provider capability bug 和修复方向，
但不能当作 benchmark 分数。

### 较早 DeepResearch 5×3×2

旧矩阵得到 single 82.62、r1 78.58、r2 78.22，但有 15 次 research retry 和 80 次
grading retry，并跨越多次 runtime 修改。它适合说明早期可靠性问题，不能用于当前拓扑
的质量结论。本报告的主表只使用后续 30/30 research、30/30 grade 的矩阵；其中三条
已完成 action 的 Workflow 通过 `8c0fab9` 修复后的 runner 从 durable journal 恢复，
这一 runtime 变化已记录在 `state.json` 的 source history 中。

## 对 Workflow 产品设计的含义

Workflow 仍然应该是用户可写的 Python agent DSL，而不是一个固定的“多 Agent
research 内核”。用户表达的是长期协作结构；runtime 负责解释和持久执行。当前实践表明
最值得做成稳定 primitive 的是：

- 持久 `Agent`/Session 和同一 Session 内的多轮 action；
- `together(...)` 并行、结构化关系与作用域；
- typed action input/output 与 completion contract；
- evaluator 产生窄 follow-up，再路由回指定 Session；
- durable timer、signal、channel、`ask_human` 和每轮 `wait_for_human`；
- Project/Workflow/Agent 三层权限上限，以及每 Turn 不可变快照；
- Agent/action 级预算、重试、停止策略和可观察的成本；
- Project snapshot、artifact 和后续 Workflow 对已有结果的显式消费。

`interactive-agent` 和 `project-summary` 都继续作为 built-in Workflow，而不是特殊旁路。
Project 管理多个正在运行或已完成的 Workflow；Workflow 管理参与的 Session。Project
目录是它的持久 research workspace。summary Workflow 读取 Project snapshot，在手动、
定时或 stale-on-open 条件触发时生成 HTML artifact；它的 prompt 使用正常的 Project、
Workflow、Agent 分层机制。这样普通交互、定时总结和复杂研究共享同一个 runtime，用户
又不需要理解额外的 “instance” 概念。

## 下一阶段：优化构建 Workflow 的能力，而不是继续堆默认 Agent

高轮次实验已经完成，并否定了把 r3/r4 设为推荐默认值。下一阶段应围绕用户可写 DSL 和
runtime primitive 做以下工作：

1. **自适应 escalation。** built-in research 从一个强 Researcher 开始；Evaluator 只在
   能指出可搜索、可路由、预期收益足够高的具体 gap 时请求额外路线。DSL 需要支持运行中
   `spawn`/激活 Agent、给既有 Session 派发 follow-up，以及预算化的 stop policy。
2. **一等 completion contract。** `finalize="after_search"` 已解决最明显 failure mode；
   下一步把交付物验证器、repair 次数、失败/等待人类策略做成所有 Action 都可组合的
   primitive，而不是每个 Workflow 手写。
3. **保真 evidence handoff。** evidence packet 应成为带来源、claim、coverage ID 和稳定
   artifact 引用的结构，而不是把大量搜索结果再次塞进 Writer prompt；grader 应测 citation
   coverage/正确性，不能只计算 URL 数。
4. **Action 级 checkpoint 与预算。** 原生保存每次结构校验、repair 原因和 token；失败从
   最近 Action 恢复。预算同时约束 uncached input、output、search call、wall time 和动态
   Agent 数，并让 evaluator 看见剩余预算。
5. **可校准的人类介入。** `ask_human` 已能暂停并保留来源；还需要让 Evaluator 生成一个
   具体问题、解释为什么不能自动解决，并在 UI 中把 warning、等待和用户修订展示为普通
   Workflow/Session 事件。
6. **继续做对照实验。** 下一组比较应是“强 single”对“single + 条件式 evaluator”以及
   “条件式 evaluator + 动态新增路线”，而不是固定 r5/r6。至少扩到更多任务、增加重复，
   并用不同模型家族的 grader 或上游 RACE/FACT 降低同模型 judge bias。

这些能力都应服务于用户自己的 Workflow。`evidence-loop`、`interactive-agent` 和
`project-summary` 是 built-in 程序和可学习示例，不是特殊内核；用户可以在一个 Project
里先运行任意研究 Workflow，再从已有 Session、artifact 或 Project snapshot 启动另一个
Workflow 继续工作。

## 可复核材料

- 当前 runtime 高难报告：`benchmarks/deep-research-mini/runs/deepseek-hard-f311284-4x3x2-2026-08-07/report.md`
- 当前 runtime 完整 state：`benchmarks/deep-research-mini/runs/deepseek-hard-f311284-4x3x2-2026-08-07/state.json`
- 第一轮基线报告：`benchmarks/deep-research-mini/runs/deepseek-clean-f2123eb-5x3x2-2026-08-07/report.md`
- DeepResearch runner：`benchmarks/deep-research-mini/run_matrix.py`
- LiveDR runner：`benchmarks/live-dr-mini/run_matrix.py`
- BrowseComp runner：`benchmarks/browsecomp-mini/run_matrix.py`
- 默认多 Agent Workflow：`workflows/builtin/evidence-loop/workflow.py`
- 单 Agent Workflow：`workflows/builtin/single-agent-research/workflow.py`

`runs/` 中包含大体积、可能带 benchmark 明文或 provider 返回内容的本地执行记录，按
`.gitignore` 不提交；本文件只固化可公开审阅的汇总、证据边界和工程结论。API key 从未
写入报告、配置或 Git。
