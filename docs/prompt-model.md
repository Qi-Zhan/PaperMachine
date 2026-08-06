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
| 3 | `workflow` | yes | Workflow objective, launch-time system prompt, and relevant relations |
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
- An Agent class uses `system_prompt = "..."`; a constructor may override it.
- A completed or queued Turn never changes when any source prompt is edited.
  Only subsequently created Turns receive a new snapshot.
- Workflow source and skill packages already use immutable source/package
  snapshots; their prompt layers record those snapshot origins.

- Project Page 负责修改项目级 system prompt 文件。
- Session 右侧 inspector 可以在没有 active Turn 时修改该 Session 的 system prompt。
- 启动 Workflow 时可以提供该 Workflow 共享的 system prompt。
- Agent class 使用 `system_prompt = "..."`，构造实例时也可以覆盖。
- 任意源 prompt 的修改都不会追溯改变已有 Turn，只影响之后创建的 Turn。

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
one Session. Editing any layer intentionally changes the provider instruction
prefix, so that next request may miss the old prompt cache. The routing cache
key remains Session-scoped; the provider decides actual reuse by matching the
full request prefix. Prompt transparency therefore makes cache misses
explainable instead of silently reusing stale instructions.

同一 Turn 的渲染结果完全固定，同一 Session 的多个 Turns 通常也共享稳定前缀。
修改任意一层会有意改变下一次请求的 instruction prefix，因此可能无法读取旧缓存。
cache routing key 仍以 Session 为作用域，真正是否复用由 provider 对实际前缀的匹配
决定。这样缓存 miss 可以由 snapshot 差异解释，也不会偷偷复用过时指令。
