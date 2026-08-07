# PaperMachine 多 Provider 与 GLM 长程实测（2026-08-08）

## 结论

PaperMachine 已经能够在同一个 Workflow 中按 Agent 选择不同的 model profile，而不把
provider 配置耦合到 Codex 的 `config.toml`。本轮真实运行使用 Aeroides 的 `glm-5.2`
承担 Planner、Evaluator、Writer 和独立 post-write Grader，使用
`deepseek-v4-flash` 承担需要 hosted web search 的 Researcher。4/4 research 和 4/4
grading 全部完成，没有整条 Workflow retry、ActionAttempt retry 或 terminal failure。

兼容性不能只用“OpenAI-compatible”一个布尔值描述。Aeroides 端点支持 Responses、流式
文本、函数工具、并发和隐式 prompt cache，但没有返回可审计的 Responses
`web_search_call`；因此它的 `hosted_web_search=false`。DeepSeek profile 则显式声明
`hosted_web_search=true`。Agent runtime 根据本次 Session 选择的 model profile 过滤 hosted
tools，URL 获取等本地函数工具仍由权限快照独立控制。

## Provider 探针

所有 credential 只通过被 `.gitignore` 排除的 `.env` 注入；配置和结果中没有保存 API
key。Aeroides profile 使用官方模型配置所列的 1,048,576 context window：
[GLM-5.2 config](https://huggingface.co/zai-org/GLM-5.2/blob/main/config.json)。

| 能力 | Aeroides / GLM 结果 | PaperMachine 处理 |
|---|---|---|
| Responses HTTP SSE | 成功 | 正常流式解析与 usage 记账 |
| Chat Completions | 端点可用 | runtime 仍统一使用 Responses 路径 |
| Function tool calling | 成功返回结构化调用 | 暴露本地工具时可用 |
| 4 路并发 | 四次请求均成功 | 可并发运行不同 Agent Session |
| 隐式 prompt cache | 12,608-token 重复前缀首次 cached=0，随后两次 cached=12,608 | 使用 provider telemetry，不伪造 cache hit |
| 并发 cache 正确性 | 共享前缀、不同后缀均返回正确 marker，cached=9,920 | routing key 保持 Session 隔离；缓存命中仍取决于 provider 的前缀策略 |
| Hosted web search | 未观察到真实 `web_search_call`；强制调用会被拒绝或退化成文本中的伪调用 | 明确配置为 false，不向模型暴露该 hosted tool |

一个实际 PaperMachine 单 Agent dogfood 在项目 workspace 中读取文件、获取一个明确 URL、
写出报告并完成 Workflow：5 秒、6 个步骤、4,670 input、1,277 output，其中 3,264 input
命中缓存。Workflow 完成后同一 Session 仍可继续对话，后续 Turns 分别观察到
1,856/1,927 和 1,920/1,997 cached/input；这同时验证了“Workflow terminal”与
“Session 可继续交流”是两个独立生命周期。

## 混合模型长程实验

### 设置

- 数据：DeepResearch Bench 固定 revision
  `469cce54ea7f6a63c163d3d9fec879cf289ec484` 的 Task 68 和 69。
- 条件：`evidence_r2`，每题重复 2 次；每次固定 2 条并行初始路线，并允许一次
  Evaluator 定向 follow-up round。
- 角色：Planner / Evaluator / Writer=`glm-5-2`，Researcher=`deepseek-flash`。
- Grader：另一个隔离、无工具的 `glm-5-2` Session，看到原问题、最终报告和上游完整
  rubric，但看不到 Workflow 内部评价。
- context：每个 job 都从 Project 级 `fresh` context 启动；不同 benchmark 题不会继承彼此
  的研究内容。

### 结果

| 指标 | 结果 |
|---|---:|
| Research / grading 完成 | 4/4 / 4/4 |
| Post-write score | 75.48 ± 7.72 |
| Task 68 Kubernetes | 72.53、66.77；均值 69.65 |
| Task 69 A2A/MCP | 84.92、77.70；均值 81.31 |
| 平均端到端时间 | 960 秒 |
| 最终报告 | 平均 29,872 字符、31.5 个直接 URL |
| Workflow completion | passed 1，warning 3 |
| 实际 evaluator rounds | 4/4 都运行 2 轮 |
| Writer revision | 2/4 |
| 整条 run retry / ActionAttempt retry | 0 / 0 |
| Grader contract repair | 0 |

累计 research usage 为 25,382,718 input、504,484 output，其中 23,739,328 input
命中缓存，整体 cache-read ratio 为 93.5%；真正未缓存的 input 为 1,643,390。四次 run
累计记录 649 个模型/工具步骤和 126 个最终报告直接 URL。

| Provider 路径 | Model steps | Input | Cached input | Cache read |
|---|---:|---:|---:|---:|
| DeepSeek Researcher | 166 | 24,466,483 | 23,526,784 | 96.2% |
| Aeroides GLM（Planner/Evaluator/Writer） | 24 | 916,235 | 212,544 | 23.2% |
| Aeroides GLM Grader | 4 Actions | 39,716 | 5,312 | 13.4% |

DeepSeek 路线的高 cache ratio 来自同一个 persistent Researcher Session 中的长工具轨迹和
第二轮复用；GLM 角色通常只有一到三次模型调用，而且每个角色的 instruction 与 evidence
handoff 不同，天然没有同样长的共同前缀。这里没有 Responses continuation hit：两个
provider 都走 HTTP SSE + implicit cache；continuation 与 cached input 是两个独立机制。

### Workflow 控制语义是否真的发生

- 8 个 evaluator follow-up 都回到原有 Researcher Session，没有为第二轮创建替代路线。
- A2A/MCP 的一个初始 evidence packet 违反语义契约，Workflow 在同一 Researcher 中执行
  一次局部修复；没有重跑整个 Workflow。
- 4 个 draft audit 最终全部通过，其中 2 个先失败并触发 Writer 修订。
- `max_rounds=2` 后仍有 3 个 run 的 evidence evaluation 未完全闭合；它们按
  `deliver_with_warning` 交付，并在结构化 completion 中保留原因，没有把 warning 冒充
  passed。
- GLM 的 Planner、Evaluator、Writer 和 Grader 全部返回了可被 DSL validator 接受的
  结构；grader 不需要 shape normalization 或 semantic repair。

## 发现的问题与设计判断

### 1. Provider capability 必须显式化

接受 Responses 请求或 function-tool schema，不代表实现了 OpenAI hosted tools。能力应
属于 provider 配置，并在 model route 后检查；不能从 URL、provider 名字或请求成功推断。
本轮已先实现 `hosted_web_search`，后续出现新的 hosted tool 时沿用同一模式。

### 2. 按 Agent 选模型应该只是 DSL 的普通参数

混合模型不需要新的调度概念。`Agent(..., model=...)` 已经是 Session 的固有属性；
`evidence-loop` 只把 `planner_model`、`research_model`、`evaluator_model` 和
`writer_model` 暴露成类型化参数。benchmark runner 新增对应 CLI 参数，只负责把配置传给
Workflow，不参与模型选择逻辑。

### 3. 缓存问题已经从“是否命中”转为“轨迹为何这么长”

两次 Kubernetes run 的 search/step 轨迹差异很大；更长的一次达到 10,313,276 input，
但 9,761,792 命中缓存。缓存本身健康，代价方差来自模型主动搜索和读取大量来源。
删除全局 `max_steps`、搜索次数和总 token budget 后，runtime 不应暗中恢复硬截断；但
Workflow 作者需要能够表达可检查的完成契约、Evaluator stop policy、人工引导和取消。
预算若以后重新出现，也应是可选 Workflow policy，而不是 provider 的全局默认行为。

### 4. 更多搜索没有自动变成更高分

Kubernetes repeat 2 比 repeat 1 使用更多 input、步骤和 URL，却从 72.53 降到 66.77。
这不是证明“搜索有害”，但再次说明证据选择、handoff 压缩和 Writer 聚焦比单纯扩大轨迹
更重要。A2A/MCP 同样存在 7.21 分 repeat gap。下一轮优化应针对 evidence ledger 的去重、
来源优先级和 claim-to-source 保真，而不是默认增加 Agent 或轮次。

### 5. 这不是模型排行榜

本实验只有两题，而且研究是混合模型、grader 也是 GLM；与旧 DeepSeek-only 结果之间还
同时变化了 runtime、prompt 和 completion 逻辑。75.48 只能证明框架路径可用并暴露新的
工程问题，不能据此宣称 GLM 或 DeepSeek 谁更强。要比较模型，必须固定 Workflow、工具、
grader 和时间窗口，并交换角色后重复更多题。

本地完整自动报告、文章、grader 输出和 state 位于被忽略的
`benchmarks/deep-research-mini/runs/mixed-glm-deepseek-68-69-r2x2-2026-08-07/`；仓库只提交
本综合结论，不提交 credential、瞬时网页内容或旧 runtime shape。
