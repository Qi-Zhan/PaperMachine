# Workflow 语言语义

本文定义当前 PaperMachine Python DSL 的可执行语义。它描述 runtime
实际会做什么，而不是未来的图形化语法设想。

## 1. 领域词汇

| 术语 | 标识 | 语义 |
|---|---|---|
| `Project` | `ProjectId` | 由 PaperMachine 持久管理的研究世界，也是 Session、Workflow、skill、artifact、prompt 与 journal 的所有权根；它不是暴露给 Agent 的文件目录。 |
| `Workspace` | `WorkspaceId` + revision | 挂载到 Project 的用户自有 canonical 文件系统根；Turn 只能通过实体化 runtime 权限操作它。Workspace 不是 Project 存储。 |
| `Session` | `SessionId` | 持久的多轮对话。来源为 `user` 或 `workflow_agent`，不存在 parent Session。 |
| `Turn` | `TurnId` | Session 中的一次用户/模型交互。 |
| `AgentStep` | `StepId` | Turn 下可检查的 model、tool、workflow 或 system 步骤。 |
| `WorkflowProgram` | `(project_id?, slug, sha256)` | 通过校验的 Python 源码及字面量 manifest；无 Project 表示 built-in。 |
| `Workflow` | `WorkflowId` | Project 内对某个不可变 workflow 快照的一次执行，包含一份具体 `request`、校验后的 `params`、可选 run `instructions`、trigger 来源和启动上下文。 |
| `WorkflowEffect` | `(WorkflowId, logical path)` | 对一次精确 Python effect 请求及其可 replay 的结果或错误所做的持久 journal 记录。 |
| starting Session | `started_from_session_id?` | 可选的发起 Session。它表示来源，不表示所有权。 |
| `Agent instance` | `AgentInstanceId` | workflow 中的一个参与者，由且仅由一个 Project-owned Session 承载。 |
| `ActionInvocation` | `ActionInvocationId` | 在某个 Agent 上调用一次已声明 action 的逻辑记录；保存 Action `contract`、绑定参数数据，并可选记录作为已核验 user Turn 来源的 HumanRequest。 |
| `ActionAttempt` | `ActionAttemptId` | invocation 的一次执行尝试；被 interrupt 后可以产生新的 attempt。 |
| `Team` | `TeamId` | 一组可动态修改的具名 Agent instance。 |
| `AgentRelation` | `RelationId` | 会作为 action 上下文注入的有向、带类型关系。 |
| `TaskScope` | `TaskScopeId` | 对相关 action invocation 的持久、可嵌套分组。 |
| `WorkflowTimer` | `TimerId` | 定时触发状态：interval、policy、status、fire count 与 deadline。 |
| `Channel` / `Signal` | `ChannelId` / `SignalId` | 具名数据流及其中有序、持久的值。 |
| `HumanRequest` | `HumanRequestId` | 由 Workflow 控制流创建的带类型问题；回答后恢复暂停的 workflow。 |
| `ControlMessage` | `ControlMessageId` | 发往某个 Session/action 边界的待处理 `guide`、`finish` 或 `interrupt`。 |

## 2. 所有权与标识不变量

| ID | 不变量 |
|---|---|
| I1 | 每个 Session 都直接属于一个 Project。 |
| I2 | Workflow 直接属于一个 Project；starting Session 可选，若存在则必须属于同一 Project。 |
| I3 | run 中每个 Agent instance 恰好拥有一个来源为 `workflow_agent` 的 Session。 |
| I4 | Agent Session 在 retired、completed、failed 或 cancelled 后仍然可导航。 |
| I5 | ActionInvocation 属于一个 Agent instance 及其 Session。 |
| I6 | 每个 ActionAttempt 属于一个 ActionInvocation，并且最多关联一个 Turn。 |
| I7 | run 只能引用同一 Project/run 内的 Session、Agent、scope、channel 与 control。 |
| I8 | run 创建之后，其 workflow 源码快照与 SHA-256 不可变。 |
| I9 | 每个 Project 的每个 slug 最多有一个可编辑 WorkflowProgram；覆盖保存不会改变已创建 Workflow 的快照。 |
| I10 | Python 可以请求 effect，但不能直接、权威地修改领域状态。 |
| I11 | 在同一个 Workflow 内，一个逻辑 effect path 永久绑定到一种精确的 kind 与 payload。 |
| I12 | Workflow 的 `request`、`params`、`instructions`、trigger、启动配置与启动上下文在 run 创建后不可变。 |
| I13 | Project managed path 与挂载的 Workspace root 永不重叠；删除 Project 永不删除 Workspace 文件。 |
| I14 | 每个 Turn 都快照一个 Workspace attachment revision 和实体化权限 hash；后续 relocation 或权限修改只影响之后的 Turn。 |

UI 可以在视觉上把 Agent Session 分组到某个 Workflow 下面，但这不会
建立 Session 的父子关系。

## 3. Definition 与 run 创建

源码中必须恰好有一个 async 函数带字面量 `@workflow(...)` metadata。
校验会产出 manifest 和 AST 结构摘要。只有不存在 error diagnostic 的源码
才可执行。

启动 run 时会完成以下操作，从而暴露一个一致的 created run：

1. 按 `slug` 在 Project 可见 catalog 中解析；同 slug 的 Project source 覆盖 built-in。
2. 按 `params_schema` 校验可复用的 run `params`；对声明为 `format: "model-profile"` 的字段同时校验其模型 profile 是否存在。
3. 把源码、manifest、owner、path 与 SHA-256 复制进 WorkflowProgramSnapshot。
4. 校验显式选择且非空的 model profile、skills、Workflow 权限上限与 Agent class override。starting Session
   必须属于该 Project，并构成不可越过的外层权限上限。
5. 按 manifest 的 `request_mode` 校验后，保存具体的用户任务或不保存启动任务，
   再保存可选 run `instructions`、trigger 来源，以及
   `fresh` 启动上下文或一份有界且不可变的 Project 快照。在上下文
   构造中，starting Session 用于确定快照焦点和记录来源，而不是复制 Session prompt。
6. 创建 Project-owned、状态为 `created` 的 Workflow，并可选记录 starting Session。
7. 调度该 run；worker 在解释 effect 前把它改为 `running`。

runner 分别通过 `ctx.request`、`ctx.params`、`ctx.trigger` 和 `ctx.context`
暴露这些值。WorkflowProgram 面向一类任务，必须显式决定把具体 request 或哪部分
context 传给哪个 Agent Action；`request_mode="none"` 的程序得到空的
`ctx.request`，并应通过显式 `ask_human` effect 接收用户消息；runtime 不会自动把
这些值提升为 instructions。
当前 HTTP launcher 对 Project 级启动记录 `manual`，对 Session-origin 启动记录
`user`；`workflow` 与 `timer` 是为内部启动路径保留的 domain value。唤醒一个已有的
timer-backed run 不会创建新 Workflow，也不会改变它的 trigger。
run 的输出就是 Python entrypoint 的返回值，runner 通过 `complete` effect 把它交给 Rust。

Workflow 进入终态并不会删除或归档其 Agent Sessions；它们仍是持久的 Project 历史。
但每个新 Turn 都必须由 Workflow Action 创建。继续工作时应从 Project 或 Session
启动新的 Workflow，不存在独立的 Session submit 路径。

关闭 Session 是独立且显式的生命周期操作：它会取消所有仍拥有该 Session 的 active
Workflow，把 Session 记为 `archived`，并从普通 UI 列表隐藏，但不会删除 Turn 或
provenance。通用 Workflow cancel 对 `interactive-agent` 与其他 Workflow 具有相同语义。

## 4. Agent 语义

`Agent(...)` 构造函数只在 Python 本地同步执行，不会立刻创建 Session。
第一次 action、Team activate、relation、channel send、针对该 Agent 的人工请求，
或显式 retire 会触发 `create_agent`。随后 Rust：

1. 检查 run 状态；
2. 在该 run 的 Project 下创建 Session；
3. 从 Agent override 或 Workflow defaults 解析 model 与 skills；
4. 创建 WorkflowParticipant 映射；
5. 发出 Session-created、workflow-attached 与 participant-created 事件。

每个 Session 有一个面向用户的权限档位，runtime 会把它展开为细粒度能力：

| 档位 | 文件 | 命令 | 网络 | Model 可见的资源工具 |
|---|---|---|---|---|
| `model_only` | 无 | 无 | 无 | 无。 |
| `read_only` | 只读该 Turn 获准操作的挂载 Workspace | 无 | 无 | `read_file`。 |
| `workspace` | 读写该 Turn 获准操作的挂载 Workspace | 沙箱执行，子进程禁止联网 | 无 | `read_file`、`write_file`、`exec_command`。 |
| `research` | 读写该 Turn 获准操作的挂载 Workspace | 沙箱执行，子进程禁止联网 | server-hosted web search 与受控公共 HTTPS fetch | Workspace 工具、`fetch_url`、托管 web search。 |
| `full_access` | 除 PaperMachine managed state 外的宿主机文件系统 | 仍使用平台 sandbox | 子进程网络及 server-hosted 工具 | 所有已注册工具与托管 web search。 |

Agent class 用 `access = "research"` 声明权限，也可以在构造函数中用
`access=` 覆盖；默认值是 `research`。启动器选择的 Workflow 档位是整个 run
不可越过的权限上限；若从 Session 启动，该上限还不得高于来源 Session。按 Python
Agent class 设置的本次运行 override 优先于 class 声明，最终结果再限制到 Workflow
上限。启动时选择的上限内权限已经得到授权，创建 Agent 不会再次发起 HumanRequest。

`await agent.set_access(profile)` 修改现有 Agent。降级无需批准；上限内的任何升级
都会创建 HumanRequest，超过 Workflow 上限则直接失败。只有 Session 没有 active Turn
时才能修改权限。
每个 Turn 在创建时保存不可变的权限快照，所以之后修改 Session 只影响后续
Turn。Session UI 遵守同一规则，并在选择 `full_access` 时二次确认。

Model sample 前会过滤 tool definitions，但这不是唯一安全边界。registry 与
每个 built-in tool 会按 Turn 快照再次检查；路径解析遵循对应文件策略；命令
执行层会独立选择 sandbox 与 network policy。Model provider API 的网络流量
属于 runtime transport，不等同于 Agent 的 Project 网络权限。

| Participant 状态 | 含义 | 可否继续执行 action |
|---|---|---|
| `active` | Session 已存在，Agent 可以被调度。 | 可以。 |
| `retired` | workflow 主动移除了该 Agent。 | 不可以。 |
| `failed` | Agent 无法继续。 | 不可以。 |

`await agent.retire()` 会保留完整 Session 历史，但拒绝之后的 action。

Agent class 或构造函数可以设置 `model`。空值继承 Workflow Run 启动时显式选择的 profile；
非空值会把这个持久 Agent Session 绑定到指定的已配置 profile。因此“一个模型生成，
另一个模型检查”只是普通 DSL：`Generator(model=...)` 与 `Reviewer(model=...)`，
不需要新的 runtime primitive。Workflow 可在任意 `params_schema` 字段上使用
`format: "model-profile"`，向用户暴露模型选择。
这种继承只属于启动后的 DSL 语义；HTTP launch 本身必须显式提供非空 model profile
与 access ceiling。

## 5. Action、Attempt 与 Turn 语义

`@action` 方法声明 prompt 和参数签名。方法体不会作为 agent 逻辑执行。
调用方法会产生一个 awaitable；await 它会请求 `invoke_action`。
之后 Agent 执行与 Codex 相同的主循环：采样模型、执行模型请求的工具、追加工具输出，
再继续采样。模型返回不带 tool call 的 terminal assistant message、用户执行
finish/interrupt/cancel，或 runtime/provider 基础设施失败时，Action 才结束。
`reasoning_effort`（`none`、`low`、`medium`、`high`、`xhigh` 或 `max`）可覆盖
该 action 的服务端默认推理强度；这个值会固化到 Turn，并出现在 model step 的
输入元数据里。
`search_context_size`（`low`、`medium` 或 `high`）控制每次 hosted search 附带
多少检索上下文；探索 route 可先用 `low`，只有确实需要更丰富页面
上下文时再提高。
`finalize="after_search"` 为必须交付最终结果的 action 建立显式完成边界：若第一次
Turn 使用过 hosted search，同一个持久 Agent Session 会再收到一个
显式禁用工具的 Action Turn，把前面的研究结果或过程播报转换成真正的最终交付物。
`finalize="always"` 即使第一次 Turn 没用 hosted tools 也会执行该无工具 Turn。
finalizer 是独立持久化、可见的 ActionInvocation/Turn，因此可以恢复和检查，
而不是隐藏的后处理。typed-action 的 JSON repair 使用同一个内部无工具策略。
```text
ActionInvocation
  Attempt 1 -> Turn 1 -> model/tool Steps
  Attempt 2 -> Turn 2 -> model/tool Steps   # 只在 interrupt/retry 后出现
```

普通 action 的 docstring/decorator 会成为名为 `Action contract` 的、可检查的
Workflow instruction layer；绑定参数会单独序列化为来源为 workflow 的 Turn input。
`ctx.request`、`ctx.params` 或 `ctx.context` 只有在 Workflow 把所选值作为 Action 参数
传入时才会到达模型。`ask_human` 返回的字符串则是 `HumanMessage`；当它传给标注为
`HumanMessage` 的 action 参数时，Python 会同时提交 request ID 与参数名。只有该
direct HumanRequest 已回答、属于当前 Workflow 与 Agent Session、answer 为字符串，
并且与绑定参数完全一致时，Rust 才允许创建来源为 user 的 Turn。人类原文逐字成为
Turn input；Action contract 与其余 Workflow 提供的上下文进入可检查、明确标为 data
的 Workflow layer；ActionInvocation 会保留来源 HumanRequest ID。

每个 Turn 都会快照实际使用且顺序固定的 prompt layers：runtime、Project、
Workflow、Agent/Session、Skills、runtime control。Workflow layer 可包含本次 run
的 `instructions`、Action contract 与该 Agent 有关的有向关系，但不会隐式包含
run request 或启动上下文快照；interrupt/retry guidance 属于 control layer。详见
[prompt 模型](prompt-model.md)。

| Invocation/Attempt 状态 | 含义 |
|---|---|
| `scheduled` | invocation 已持久化，但尚未获得执行许可。 |
| `running` | Attempt 与 Turn 正在执行。 |
| `completed` | Turn 输出已保存并返回 Python。 |
| `interrupted` | 当前 attempt 结束；runtime 会为同一个 invocation 创建新 attempt。 |
| `failed` | runtime/model/tool 失败终止了 invocation。 |
| `cancelled` | run 或 Turn cancellation 终止了 invocation。 |

完成的 action 返回 assistant 输出字符串。token/cache usage、Step 数、hosted-search
次数与耗时会累加到 Workflow telemetry。provider 对 incomplete sample 返回的 usage 会跨重试
累积；如果所有重试都失败，最后一个 model Step 会以 failed 状态持久化，已经
消耗的 token 仍然记录到该 run。若 output limit 或只有 reasoning 的 completion
没有产生 message/tool call，固定的瞬时错误重试会降低 reasoning effort，并明确要求按原始
格式交付 final answer。

Action 完成和整个 Workflow 的结果是否被接受是两层语义。例如内置
`evidence-loop` 暴露 `audit_policy`：`deliver_with_warning` 返回带显式 warning
的结果；`fail_run` 在 evidence/draft gate 失败时终止；`wait_for_human` 会持久等待
用户输入 `/deliver`、`/fail` 或修订意见。自由文本修订意见会作为已核验的
`HumanMessage` 传给持久 Writer Session，因此 UI 中是来源真实的 user Turn。

## 6. 并发

`await together(a(), b(), ...)` 是唯一特殊的并发 combinator。它使用
`asyncio.gather`，并按参数顺序返回 tuple。

启动前，`together` 会检查直接传入的 `_ActionCall`。如果两个 call 指向同一
Agent 对象，它会抛出 `ValueError`，并且该组 action 一个也不会启动。
Rust runtime 还会为每个 Agent instance 应用一个 mutex，串行化其 Session 中的
Turn，并应用 Session runtime 的服务端全局 concurrent-Turn 上限。

因此，不同 Agent Session 可以同步工作。同一 Agent 的 call 即使通过普通
Python task 并发创建，也会在 per-Agent gate 排队。

`background(awaitable)` 会创建普通 asyncio task 并登记到 runner。
`join()` 观察结果，`cancel()` 取消任务。entrypoint 退出时，所有未结束的
background task 都会被取消。

## 7. Team 与 Relation

`Team(name, *members)` 在 `activate`、`add` 或 `remove` 前只存在于 Python
本地。Team membership 会持久化，但没有隐式调度语义：它是分组与控制
primitive，不是隐藏的执行循环。

`relate(source, target, kind, instructions)` 创建一条有向关系。Agent action
执行前，Rust 会收集所有涉及该 Agent 的入边和出边，并注入可读的关系上下文。
relation 不会自动发消息，也不会自动调用 target。

Workflow 处于 active 状态时可以动态创建 Agent 和修改 Team。把 Agent 从 Team
移除不会 retire 它；retire 必须显式执行。

## 8. Task Scope

`async with scope(name, objective)` 会打开 TaskScope，并把 ID 压入 runner
本地的 scope stack。在 block 中创建的 action 会记录这个 scope ID。
嵌套 scope 会把当前 scope 记录为 parent。

正常退出时 scope 变为 `completed`；异常退出时变为 `cancelled`，之后继续
传播 Python exception。scope 状态本身不会取消 action。

## 9. 人工交互与控制

`await ask_human(...)` 是 Workflow 控制流 effect。它挂起 Workflow coroutine，
创建属于指定 Agent Session 或 origin Session 的 request，不要求存在 Action Turn。
模型永远不会收到 `ask_human` tool definition。Action 可以返回 `needs_human`
之类的结构化建议，但只有 Workflow 代码能把建议转成 HumanRequest。

DSL 的字符串回答会以 `HumanMessage` 携带持久 HumanRequest ID。只有把该值传给
对应类型标注的 action，workflow 才能创建显示为人类消息的 `user` Turn；其他
schema value 仍是普通 Python 值，相关 action 仍按 workflow 派发。

response schema 会与 request 一起保存。HTTP API 在 resolve 前校验答案。
只要仍有 open request，`Workflow.attention_required` 就为 true。

`await wait(...)`、`Channel.receive()` 与 workflow 级 `ask_human(...)` 都是可
replay 的挂起点。某个分支先收到“已挂起”的协议确认，而不是异常；只有当所有仍
存活的 Python 分支都停在这类 effect 上时，runner 才声明 quiescent。Rust 保留
这些 effect 的 `started` 状态，终止 Python 进程并释放全局执行 permit。任意等待
条件就绪后 supervisor 会重放源码；已经完成的分支与领域变更直接复用 journal
结果。因此一个分支等待 HumanRequest 时，后台 timer 仍可触发；并发分支先发布的
Signal 也会在 replay 后恰好消费一次。

`ctx.context` 返回不可变的启动快照，`fresh` run 则返回 `{}`；
`ctx.project.snapshot(...)` 另行读取属于当前 Project 的最新、有界持久状态；
长程 Workflow 可以把上次快照的 `captured_at` 作为下一次的 `updated_after`，此时
返回 `mode="delta"`，且只包含游标后更新的 Session/Turn、Workflow 与 Artifact。
捕获时间在数据库读取前确定，因此并发更新最多在相邻快照中重复，不会落入游标空隙；
`publish_artifact(...)` 只接受文本，并由 effect path 派生 Artifact ID，因此 replay
幂等。用户 Workflow 可以用这两个 effect 构建 Project 级视图，而不需要直接访问
SQLite 或 host 文件。

每个 Action 都用 `@action(tools=[...])` 声明它请求的全部本地工具；裸
`@action` 表示不使用本地工具。这份静态声明属于 Workflow 元数据，请求名称持久记录
在 ActionInvocation 上。Rust 在创建 Turn 前拒绝未知名称，按已实体化的 access 上限
过滤 Workspace 工具，并且只在 Workflow Action 路径接纳 Project 工具；最终排序后的
definitions 及其 SHA-256 作为 Turn 的 ToolSetSnapshot 持久保存。Hosted web search 仍由 access、`search_context_size` 和 provider capability
独立控制。

model sampling、dispatch、pause/resume 与 crash recovery 都从该快照重建同一个精确
Registry；executor 缺失或 definition 改变时 fail closed。内置 `project-summary` 只
声明 `read_project_home`、`patch_project_home` 和 `preview_project_home`，Agent 自然
结束后再由 `publish_project_home(...)` 一次性发布。发布以 Project canonical home
revision 做 CAS：过期 draft fail closed，无变化 draft 复用现有 Artifact；普通
`publish_artifact(...)` 不能声明保留的 home role。

Control message 是异步的：

| Control | 精确语义 |
|---|---|
| `guide` | 向某个 Session/action 排队。在下一个 Agent checkpoint，它会作为 user-history item 加入下一次 model sample 前的上下文；不会推翻已完成工作。 |
| `finish` | 在下一个 checkpoint 加入指令，并强制当前 Action 的下一次 model sample 禁用 tools、直接交付最终回答；Workflow 随后继续。 |
| `interrupt` | 在下一个 checkpoint，把当前 Turn/Attempt 标记为 interrupted。action runtime 为同一个 ActionInvocation 创建新 Attempt，并把 control 文本作为重启 guidance。 |
| pause | 把 run 设为 `paused`。workflow 与 Agent checkpoint 等待；已经在途的 provider response 不会回滚。 |
| resume | 把 run 设为 `running`，等待中的 checkpoint 继续。 |
| cancel | 把 run 设为 `cancelled`，并把 cancellation 传播到 Python、model 与 tool。 |
| Stop Turn | 只取消该 active Turn 及其 model/tool Steps，不要求模型生成收尾答案。如果它属于 Workflow Action，对应 effect 会把取消错误交回普通 Workflow 异常处理。 |

guide/finish/interrupt 的 delivery 在 Store 层持久且 at-most-once：checkpoint 消费
pending message 时会把它标记为 applied。

## 10. Channel 与 Signal

`Channel(name, schema)` 会在一个 run 内创建或复用具名 channel。
`publish(value, sender=...)` 追加一个 Signal，其 sequence 在 channel 内单调递增。
`receive()` 等待该 Channel 对象本地 cursor 之后的第一个 Signal，推进 cursor，
并返回 value。

schema 目前会保存供检查，但尚未用于校验 Signal value。publish 不会自动
调用 subscriber；receive 必须显式执行。

## 11. Timer

`@every(seconds=..., policy=..., name=...)` 创建 TimerHandle，并启动一个
background loop：

1. 按名字注册或复用 active timer；
2. 等待 `next_fire_at`；
3. 持久化一次 fire，推进 count/deadline，并更新 usage telemetry；
4. await callback；
5. 重复。

| Policy | 目标调度语义 | 当前 executor 行为 |
|---|---|---|
| `coalesce` | 把错过的多个 tick 合并为一次运行。 | 每次 wait 返回执行一次 callback。 |
| `skip` | 上一次工作未完成时跳过 tick。 | 已记录，行为尚未区分。 |
| `queue` | 把每个 tick 保留为排队工作。 | 已记录，行为尚未区分。 |

timer loop 会 await callback，所以同一个 TimerHandle 的 callback 不会重叠。
callback 中的 action 每次都会创建新 Turn。workflow 完成时，active timer record
会变为 completed。

## 12. 完成、失败与可观测性

| Workflow 状态 | 进入条件 | 是否接受 effect |
|---|---|---|
| `created` | run 已持久化，等待 scheduler。 | checkpoint 可以进入启动阶段。 |
| `running` | worker 正在解释源码。 | 可以，但受校验与权限约束。 |
| `waiting_for_user` | Workflow 正在请求用户输入。 | 用户提交通过校验的回答后继续。 |
| `waiting_for_timer` | Workflow 正在等待 timer deadline。 | timer 唤醒后继续。 |
| `waiting_for_signal` | Workflow 正在等待 Channel Signal。 | 收到匹配 Signal 后继续。 |
| `paused` | 用户暂停 run。 | 已有调用在 checkpoint 等待。 |
| `completed` | `complete` 已保存输出。 | 不再接受新领域工作。 |
| `failed` | Python、action、protocol、sandbox、model 或 provider 失败。 | 不接受。 |
| `cancelled` | 用户/runtime 取消。 | 不接受。 |

Agent/action/step/timer/search 数、provider token/cache usage 和 wall-clock time 是供检查
的持久化观测数据。Action 会在模型给出终态输出、用户显式控制、provider/runtime 失败或
context-window 到达边界时停止。runtime 还会强制权限、sandbox 边界、Session 串行化、
provider request/stream-idle timeout 和服务端全局并发限制。

未捕获的 Python exception 会让 runner 退出，并以有长度限制的 stderr 使 run
失败。action failure 会作为 effect exception 返回 Python；workflow 若不捕获，
run 就会失败。entrypoint 正常 return 会发送 `complete`；未 complete 就退出属于
protocol error。

## 13. 持久化与 Replay 边界

所有权威 entity、effect 结果与有序 event 都会持久化，workflow 源码也会被
快照。所有非终态 Workflow 都会在 server 重启时进入恢复调度。未完成的独立
Session Turn 则会被收束：已有持久化终态候选时直接提交，否则不再次请求 provider，
而是转为 `interrupted` 并等待用户明确决定。Resume 会基于已提交的 Session rollout
创建一个新的 user Turn，绝不重新打开旧的 interrupted Turn；Workflow-owned Turn
不能由用户手工 Resume。

runtime 不序列化 Python instruction pointer，而是从 entrypoint 重新执行源码。
每个 DSL 操作都有确定性的逻辑 effect path；Store 会持久化该 path、kind、精确
payload hash、`started/completed/failed` 状态、result、error 与时间戳。再次到达
已经 completed 的 path 时，只返回原结果，不重复领域变更。若进程消失时某个
effect 仍是 started，runtime 会用 `(WorkflowId, effect path, resource kind)` 派生
的确定性 ID 重新派发，使 entity 创建、signal 发布、timer 触发和 HumanRequest
收敛到原有对象。同一路径若出现不同请求会 fail closed。

未完成的 Action 会复用原 ActionInvocation、最新的非终态 Attempt 与关联 Turn。
对应 Session 的 append-only rollout 保存追加或显式替换的 model context、累计
usage、已完成 model-step 与 hosted-search 游标，以及可能已经得到的终态候选消息；
Turn 的 SQLite 文档不再复制累计 context。每个本地 Tool Step 还会保存 provider
call ID、effect disposition，以及 `prepared`/`executing` 持久化边界。恢复会 replay
rollout，并复用 completed Tool Step 的真实输出。`prepared` 表示尚未越过外部副作用
边界，恢复时会先持久化为 `executing` 再执行；对于已是 `executing` 的调用，`pure`
与 `idempotent` 可带同一 effect ID 重放，`reconcilable` 必须先检查外部状态，
`unknown` 绝不自动重放，而是产生明确的 `execution_unknown` function result。
workflow 级 `ask_human` 本身是 journaled effect，因此会继续等待同一个确定性
HumanRequest。

human、timer 与 signal wait 还支持不保留进程的挂起。Python effect client 会跟踪
所有 pending future；只有全部 pending effect 都是可 replay wait 时才请求 runtime
suspension。这个 quiescence 规则避免较早出现的 human wait 取消仍在运行的并发
Agent action 或 Signal publisher。恢复时，open direct HumanRequest、active timer 与
started signal wait 会重建唤醒条件。休眠时间不占 scheduler permit；每一段真正运行
的 replay 仍会累计持久化 wall-time usage。

两个 effect 之间的纯 Python 计算可能再次执行。对于同一源码快照和输入，作者
必须保持 effect 序列确定；时间、随机数或其他非确定性不能让已经占用的逻辑路径
产生不同请求。

## 14. 代表性执行轨迹

以内置 parallel-discovery workflow、两个 perspective 为例：

| 时间 | Python 操作 | 持久结果 |
|---|---|---|
| T0 | 构造两个 Researcher、一个 Synthesizer 和一个 Team。 | 第一次使用前没有 effect。 |
| T1 | `await team.activate()` | 三个 Agent instance/Session 与一个 Team 已存在。 |
| T2 | 创建两条 `reports_to` relation。 | 有向关系记录已存在。 |
| T3 | 进入 `scope(...)`。 | open TaskScope 已存在。 |
| T4 | `await together(researcher1.investigate(...), researcher2.investigate(...))` | 两个 ActionInvocation 在两个 Session 中并行执行。 |
| T5 | 两个 action 完成。 | 两个 Turn 与输出已持久化；scope 以 completed 关闭。 |
| T6 | `await synthesizer.synthesize(...)` | 第三个 ActionInvocation/Turn 消费前两个输出字符串。 |
| T7 | `return {"summary": ...}` | runner 发送 `complete`；run output/status 持久化。 |

任何时刻打开 Agent Session，看到的都是与普通 user Session 相同的多轮对话，
model/tool 执行细节也同样折叠在每个 Turn 下。
