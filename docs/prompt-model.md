# Prompt and model snapshots / Prompt 与模型快照

Every Turn stores the exact instructions and model route it used. Prompt text,
model routing, tool membership, and authorization are independent surfaces;
none can grant another.

每个 Turn 都保存实际使用的完整 instructions 与模型路由。prompt、model route、
tool membership 与 authorization 是相互独立的边界，任何一项都不能为另一项扩权。

## PromptSnapshot

The host renders these ordered layers:

| Order | Kind | Source |
| ---: | --- | --- |
| 1 | runtime | PaperMachine's model-facing runtime contract |
| 2 | project | Project-managed system prompt |
| 3 | session | immutable Session instructions and current Action contract |
| 4 | agent | Agent class or constructor system prompt |
| 5 | skills | complete resolved instructions of enabled Project Skills |
| 6 | control | explicit retry guidance for the current ActionAttempt |

Each layer records its kind, name, stable source, content, and SHA-256.
PromptSnapshot also stores the rendered provider instructions and their hash.
The internal kind is **session**, because Session is the runtime owner; the
WorkflowProgram remains only the definition that supplied the Action contract.

host 按表中顺序渲染 prompt。每层保存 kind、name、稳定 source、content 与 SHA-256，
PromptSnapshot 还保存最终 provider instructions 及其 hash。第三层叫 **session**：
Session 是运行时 owner，WorkflowProgram 只是提供 Action contract 的定义。

Later layers specialize the task but never change filesystem, tool, network,
model, or approval authority. Workspace files do not become prompt layers
implicitly. Their content reaches a model only when a tool reads it or program
data is explicitly passed to an Action.

后面的层可以细化任务，但不能改变文件、工具、网络、模型或人工授权边界。
Workspace 文件不会隐式成为 prompt；只有工具读取，或程序显式传给 Action 时才会
进入模型上下文。

## Editing and immutability / 修改与不可变性

- The Project prompt is managed below the Project data root.
- Session instructions are fixed when that Session is launched.
- Agent system prompts are fixed when each Agent is created.
- WorkflowProgram source is snapshotted into the Session.
- Skill instructions are resolved in full into each Turn; recovery never reads
  a live Skill file.
- Claimed AgentInput is appended to canonical model context at the next safe
  checkpoint; retry guidance may become a control prompt layer.
- Editing any source affects only future Sessions or Turns, never an existing
  Turn snapshot.

- Project prompt 位于 Project managed root。
- Session instructions 在启动时固定。
- Agent system prompt 在创建 Agent 时固定。
- WorkflowProgram source 会完整固化到 Session。
- Skill instructions 会解析进 Turn，恢复时不会读取 live Skill。
- claimed AgentInput 会在下一个安全 checkpoint 进入 canonical model context；
  retry guidance 可以成为 control prompt layer。
- 修改任何来源都只影响之后创建的 Session 或 Turn，不会改写已有快照。

The New Session dialog launches **interactive-agent**. Its optional “Agent
instructions” value becomes that Agent's system prompt; it is not a mutable
Session-level chat property.

New Session 对话框启动 **interactive-agent**。其中可选的 “Agent instructions”
会成为该 Agent 的 system prompt，而不是可变的 Session chat 属性。

## Data versus instructions / 数据与指令

Session launch stores two different inputs:

- **request**: a concrete task exposed as **ctx.request**;
- **instructions**: policy shared by the Session's Actions and also exposed as
  **ctx.instructions**.

Program code must explicitly pass request data to an Action; it is never
promoted into provider instructions. Session instructions are the deliberate
instruction layer.

Session launch 保存两种不同输入：**request** 是具体任务，**instructions** 是该
Session Action 共享的策略。request 必须由程序显式传入 Action，不会被提升为 system
instruction；instructions 则刻意进入 Session prompt layer。

Project content is never injected at launch. `ctx.project.changes()` returns
bounded current entity snapshots to Workflow Python. Content reaches a model
only when Workflow code passes those snapshots as Action arguments; it remains
message data, not a prompt layer.

Project 内容不会在启动时注入。`ctx.project.changes()` 向 Workflow Python 返回有界的
当前 entity snapshot；只有 Workflow 把它们作为 Action argument 传入时才进入模型，
并且仍是 message data，不是 prompt layer。

For interactive input, an answered HumanRequest is passed as a HumanMessage
argument. Provenance is stored on HumanRequest and ActionInvocation; Turn no
longer has a separate user/workflow origin enum.

Session `instructions` have deliberately different semantics. The exact string is
available to Python as `ctx.instructions`, and the runtime also snapshots it as
a Session instruction layer on every Action Turn. Use it for
cross-Agent rules such as language, evidence policy, output conventions, or a
summary policy. Use `ctx.request` for the concrete task and explicitly pass
selected Project snapshots as Action data. This prevents task data or old model
output from becoming system-level authority.

Session `instructions` 的语义刻意不同：Python 可通过 `ctx.instructions` 读取原文，
runtime 也会把它作为每个 Action Turn 的 Session instruction layer 固化。
它适合跨 Agent 的语言、证据、输出或摘要策略；具体任务放在 `ctx.request`，选择的
Project snapshot 作为 Action data 显式传入。这样不会误把任务数据或旧模型输出提升成
system 级指令。

交互输入通过 HumanMessage 参数传递已回答的 HumanRequest。来源记录在
HumanRequest 与 ActionInvocation 上；Turn 不再维护另一套 user/workflow origin。

## ModelRouteSnapshot

Before Turn creation, routing resolves:

- model profile and provider;
- concrete upstream model;
- context window and declared capabilities;
- final reasoning effort;
- SHA-256 of relevant non-secret provider/model configuration.

The API key never enters the snapshot. Sampling, hosted-tool decisions, resume,
and recovery use the immutable route. If the configured route no longer
matches, recovery fails closed.

Turn 创建前会固化 profile、provider、upstream model、context window、
capabilities、最终 reasoning effort 与非秘密配置 hash。API key 不进入快照。
sampling、hosted tool、resume 与 recovery 都使用这条不可变路由；配置漂移时
fail closed。

## Cache behavior / 缓存行为

The cache identity is Agent-scoped. Stable runtime, Project, Session, Agent,
and Skill layers form a reusable prefix across that Agent's Turns. Concrete
Action input and AgentInput remain message data after the stable instruction
prefix. Action-contract or Skill changes intentionally change that prefix. The
provider ultimately decides cache reuse from the request it receives.

cache identity 以 Agent 为单位。同一 Agent 的 runtime、Project、Session、Agent 与
Skill 层通常形成稳定前缀；具体 Action input 与 AgentInput 保留为该前缀之后的 message
data。Action contract 或 Skill 改变会有意改变前缀，最终是否命中由 provider 根据
实际请求决定。

Project Summary follows the same rules: one ordinary Agent, one ordinary
no-tool Action receiving Project snapshots, and optional Session instructions
describing what to emphasize. It has no hidden reviewer prompt or special
kernel prompt path.
