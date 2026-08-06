# Prompt model / Prompt 机制

PaperMachine does not keep one mutable, opaque `system_prompt` string for a
Session. Every Turn stores an immutable `PromptSnapshot` containing the exact
ordered layers, their origins, individual SHA-256 hashes, the rendered provider
instructions, and a hash of that rendered value.

PaperMachine 不把所有指令藏进一个不断变化、无法追溯的字符串里。每个 Turn 都会
保存不可变的 `PromptSnapshot`：包括各层内容、来源、各自的 SHA-256、最终发给
provider 的完整 instructions，以及该最终文本的 hash。

## Resolution order / 解析顺序

| Order | Layer | User control | Source |
|---:|---|---|---|
| 1 | `runtime` | no | PaperMachine built-in runtime contract |
| 2 | `project` | yes | `<project-root>/.papermachine/prompts/system.md` |
| 3 | `workflow` | yes | Workflow objective, launch-time system prompt, immutable launch context, action contract, and relevant relations |
| 4 | `agent` or `session` | yes | Agent class/constructor `system_prompt`, or an interactive Session system prompt |
| 5 | `skills` | yes | enabled Project Skill snapshots |
| 6 | `control` | yes | explicit runtime or human attempt guidance |

Layers are rendered in this order into the single `instructions` field used by
the Responses-compatible provider API. Later layers specialize the context but
do not grant permissions. Filesystem, command, network, budget, and approval
rules are always enforced by runtime code, never by prompt text.

这些层按表中顺序渲染到 Responses-compatible API 的单一 `instructions` 字段。
后面的层可以细化任务语境，但不能提升权限。文件、命令、网络、预算与人工授权都由
runtime 代码执行，不能靠 prompt 获得或绕过。

## Editing semantics / 修改语义

- Project Page edits the Project system prompt file.
- A Session inspector edits that Session's system prompt between active Turns.
- Starting a Workflow may supply a Workflow-wide system prompt.
- Starting from a Session records provenance and may focus captured research
  context, but does not inherit or copy that Session's system prompt.
- An Agent class uses `system_prompt = "..."`; a constructor may override it.
- A completed or queued Turn never changes when any source prompt is edited.
  Only subsequently created Turns receive a new snapshot.
- Workflow source and skill packages already use immutable source/package
  snapshots; their prompt layers record those snapshot origins.

- Project Page 负责修改项目级 system prompt 文件。
- Session 右侧 inspector 可以在没有 active Turn 时修改该 Session 的 system prompt。
- 启动 Workflow 时可以提供该 Workflow 共享的 system prompt。
- 从 Session 启动会记录来源，并可在研究上下文中优先包含该 Session，但不会继承或
  复制它的 system prompt。
- Agent class 使用 `system_prompt = "..."`，构造实例时也可以覆盖。
- 任意源 prompt 的修改都不会追溯改变已有 Turn，只影响之后创建的 Turn。

`project-summary` makes this split concrete. Its reviewed Agent base prompt and
Action contract stay in the built-in Workflow source. The Project Page edits
the summary run's Workflow system prompt: what to emphasize, preserve, or omit.
The Project system prompt still applies underneath it. Updating a scheduled
summary starts a new snapshotted Workflow run and terminates the old schedule;
previous summary Turns and HTML Artifacts remain attributable to their original
prompt snapshots.

`project-summary` 把这个分层直接呈现在 UI 中：内置 Workflow 源码保存经审查的
Agent 基础 prompt 与 Action contract；Project Page 编辑的是该 summary run 的
Workflow system prompt，用来说明应优先展示、保留或省略什么；Project system
prompt 仍然位于更前面的共享层。修改定时摘要会启动一个使用新 prompt 快照的
Workflow，并结束旧定时 run；旧 Turn 与 HTML Artifact 仍可追溯到原 prompt。

## Launch context / 启动上下文

Run Workflow offers `fresh` and `project_snapshot`. `fresh` adds no previous
research data. `project_snapshot` stores one bounded Project snapshot on the
Workflow, exposes the same JSON to the Python program as `ctx.context`, and
adds one readable `Workflow launch context` layer to every Agent Turn. The
snapshot is treated as untrusted data, not instructions. It remains byte-stable
for the whole run; only an explicit `await ctx.project.snapshot()` effect reads
newer Project state.

Run Workflow 提供 `fresh` 和 `project_snapshot`。`fresh` 不加入既有研究数据；
`project_snapshot` 在 Workflow 上保存一份有界 Project 快照，通过 `ctx.context`
把同一份 JSON 交给 Python，并在每个 Agent Turn 中加入可查看的
`Workflow launch context` 层。快照只能作为不可信数据，不能作为指令；整个 run
期间内容不变，只有显式调用 `await ctx.project.snapshot()` 才会读取较新的 Project
状态。

## Message origin / 消息来源

`Turn.origin` is `user` for text submitted directly through the Session
composer or for a string HumanRequest answer that Rust has verified and bound
to a `HumanMessage` action parameter. It is `workflow` for an Action dispatched
from program-generated data. Both use the same Agent runtime, history, model,
tools, and Step representation. The distinction is a display and provenance
contract: the UI must never render a Workflow-generated task as if the human
typed it.

`Turn.origin=user` 表示文字来自 Session 输入框，或来自 Rust 已核验并绑定到
`HumanMessage` action 参数的字符串 HumanRequest 回答；`Turn.origin=workflow`
表示 Action 使用的是程序生成的数据。二者共享同一 Agent runtime、history、
model、tools 与 Step 结构，但 UI 必须明确显示来源，不能把 Workflow 生成的任务
伪装成人类消息。

## Cache behavior / 缓存行为

The rendered snapshot is stable for a Turn and usually stable across Turns in
one Session. In particular, a Workflow launch snapshot is captured once rather
than rebuilding the whole Project context before each action. Editing any
mutable layer intentionally changes the provider instruction prefix, so that
next request may miss the old prompt cache. The routing cache key remains
Session-scoped; the provider decides actual reuse by matching the full request
prefix. Prompt transparency therefore makes cache misses explainable instead
of silently reusing stale instructions.

同一 Turn 的渲染结果完全固定，同一 Session 的多个 Turns 通常也共享稳定前缀。
尤其是 Workflow 启动快照只截取一次，不会在每个 action 前重新拼接整个 Project。
修改任意可变层会有意改变下一次请求的 instruction prefix，因此可能无法读取旧缓存。
cache routing key 仍以 Session 为作用域，真正是否复用由 provider 对实际前缀的匹配
决定。这样缓存 miss 可以由 snapshot 差异解释，也不会偷偷复用过时指令。
