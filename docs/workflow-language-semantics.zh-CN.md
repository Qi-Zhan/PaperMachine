# Workflow 语言语义

本文描述当前 clean-break Python DSL。

## 运行时模型

~~~text
WorkflowProgram 定义
  -> Session 执行
       -> 一个或多个 Agent
            -> ActionInvocation
                 -> ActionAttempt
                      -> Turn
~~~

Project 同时拥有 WorkflowProgram 定义与 Session 执行。Session 是一次不可变 program
snapshot，加上输入、配置、状态、effect journal、Agent、event、human request、
AgentInput、usage、output 与 Artifact。Agent 是 Session 内模型身份的公开名称。没有
Participant entity，也没有每个 Agent 再套一层 Session。

每个 Turn 都由 ActionAttempt 创建。交互聊天也遵循同一规则：
**interactive-agent** 先获得持久 HumanRequest 的回答，再把经过验证的回答传给普通
Action。来源记录在 trigger、HumanRequest 与 ActionInvocation 上，不再维护
Turn-origin enum。

## Program 与启动

source 必须且只能有一个 async **@workflow** entrypoint。literal manifest 包含 slug、
name、description、request_mode 与 params_schema。validator 同时记录 Agent class 和
Action 的静态工具声明；存在 error diagnostic 的 source 不可启动。

启动时一次性冻结：

- source、manifest、source SHA-256 与 Python runtime ABI SHA-256；
- `request_mode="required"` 下的一条具体 `request`；
- 通过校验的 `params`、可选 Session `instructions` 与 launch provenance；
- 显式选择的 model profile、skills、access ceiling 与 Agent overrides。

runner 暴露 `ctx.session_id`、`ctx.request`、`ctx.params`、`ctx.instructions` 与 `ctx.trigger`。Workflow
必须把 Action 真正需要的数据显式传入；runtime 不会把 request 或 Project data 偷偷
升级成 system instructions。

**request_mode="required"** 要求启动任务；**request_mode="none"** 可以无任务启动，
交互程序之后通过 **ask_human** 取得消息。

## 公共 DSL

~~~python
Agent
@action(...)
@workflow(...)
await together(...)
await ask_human(...)
await wait(seconds=... | minutes=..., name=...)
await ctx.project.changes(...)
await publish_artifact(...)
await publish_project_home(action=...)
~~~

控制流直接使用 Python **if**、**for**、**while**。周期 Session 就是普通 loop 加一次
durable wait。

构造 Agent 只创建本地 descriptor。第一次 remote operation 才在当前 Session 下创建
持久 Agent row。它的 class、name、role、system prompt、model、access、skills 与
rollout 在该 Session 生命周期内保持同一身份。

## Action 与 Turn

**@action** method 是声明。prompt/docstring、bound arguments、model options、return
type 与 tool list 共同描述一次模型 Turn；Python method body 不承载模型逻辑。

await Action 后运行一次统一 sample/tool/follow-up loop：

1. 创建 ActionInvocation、ActionAttempt 与不可变 Turn；
2. 对 Agent model 采样；
3. 只执行 Turn ToolRegistry 中模型实际发出的调用；
4. checkpoint output，需要时继续采样；
5. 在 terminal assistant result 或 runtime control 时结束。

interrupt 结束当前 Attempt，程序可以为同一个 Invocation 开始新的 Attempt。retry 不会
伪装成第二个逻辑 ActionInvocation。

dict、list、bool、int、float typed return 请求 JSON parsing。JSON repair 与
**finalize="after_search"** 使用无工具模型工作，不会获得隐藏 Registry。

**ask_human** 返回带 HumanRequest provenance 的 HumanMessage。只有 HumanMessage
类型的 Action 参数能把这份已验证字符串作为直接用户输入；Rust 会验证 Session、
Agent、request status 与 exact text。

## Tool 与权限

bare **@action** 获得协作工具和 access 允许的 native tools；
**@action(tools=[])** 表示空 Registry，非空静态列表选择精确子集。host 拒绝未知或
重复名称。

- native tools 是 `exec_command`、`write_stdin`、`apply_patch`，由 access 过滤；
- 协作工具是 `list_agents`、`send_message`、`wait_agent`、`spawn_agent`、
  `interrupt_agent`，child Agent 不获得 spawn；
- hosted web search 独立由 `search_context_size` 与 provider capability 控制，
  不依赖本地 access preset。

tool membership 决定模型可见性与 dispatch。filesystem、command、network、
managed-root 与 credential rule 仍是独立硬约束。

Session access 是硬 ceiling，Agent override 不可超过它。降级在 Turn 之间生效；ceiling
内的升级需要 typed human grant。已有 Turn 保留自己的 authorization snapshot。

## 并发

**await together(a(), b(), ...)** 使用普通 asyncio gather，并保持参数顺序。同一个
Session 内的不同 Agent 可以并发；同一 Agent 的两个 active Action 会被拒绝，因为该
Agent 只有一个 canonical rollout 与一个 active Turn。

这是唯一必要的串行规则；Session 并不是全局单线程。

Agent 创建的 task 与 Workflow Action 使用同一个 durable ActionRunner 和 per-Agent
FIFO。queue-only message 只进入 AgentInput，不启动 Turn；`wait_agent` 只观察 Action
状态，不保存第二种 wait。spawn 会原子创建同 Session child 和首个 Action；child
继承模型身份配置，access 只能保持或降低，不能再次 spawn，并在 Session Closing 时
被 join。

## 人工输入、等待与 AgentInput

**ask_human** 与 **wait** 是 replayable suspension effect。wait 只保存一条 journal，
deadline 由 started_at 加 interval 得出。当所有 live future 都停在可 replay 的等待上，
Rust 会结束空闲 Python process 并释放 permit。合法回答或到期 deadline 会让同一
Session 再次 runnable；重放不可变 source 后会到达已存 effect result。

AgentInput 状态为 **pending -> claimed -> applied**，并记录 Human 或 Agent source：

- message 与 guide 在下一次 sample 前进入 canonical context；
- finish 强制下一次 sample 不使用本地工具；
- interrupt 结束 active Attempt；
- pause 在 checkpoint 停止，resume 继续，cancel 终止 Session。

只有消费 input 的 checkpoint 或 terminal transaction 才会把 claim 变成 applied；
checkpoint 前 crash 时，同一 Turn 可以重新 claim。

## Project API

`ctx.project.changes()` 返回 opaque cursor 与发生变化的 Project、Session、Agent、
Turn、Artifact、Project Home 的有界当前 snapshot。把 cursor 作为 `after_cursor`
传回可得到之后的 committed changes。page 从 change log 派生，会去重、返回
tombstone、分块大文本 Artifact、只返回二进制 metadata，并过滤调用 Session。
`exclude_current_program=True` 还会在构造 snapshot 前跳过调用方
WorkflowProgram 的历史运行。`publish_artifact` 写入确定性的 Project-managed
content。

Project Home 同样位于 managed state。普通 Action 返回完整、安全的 HTML fragment，
再把那一个已经 await 的 `_ActionCall` 传给 `publish_project_home`。发布会验证精确
Action provenance、校验 HTML，并原子更新 canonical page。内核不信任任何 Workflow
slug，也没有特殊 Summary Agent 分支。

## 持久化与恢复

Session host effect 使用确定性 path 与 request hash。completed effect replay 保存的
result；同一路径换 input 会 fail closed。effect 之间的纯 Python 可能在 restart 后重跑，
因此相同 source 与 input 必须产生确定的 effect 顺序和 payload。

每个 Agent JSONL 是 canonical model history：

~~~text
TurnCreated
ContextCheckpoint
TurnUpdated
~~~

model FunctionCall 必须先 sync 再 dispatch；FunctionCallOutput 必须先 sync 再继续采样。
恢复时，没有 output 的 call 会得到一次稳定 **"aborted"**，旧 call 永不再次 dispatch。
同一个 Agent 会观察 durable reality，再决定是否发出新 call。host-effect replay 与
model-tool recovery 刻意分层。

## 状态与完成

Session status 为 **created**、**running**、**waiting_for_input**、
**waiting_for_deadline**、**paused**、**completed**、**failed** 或 **cancelled**。
archive 是独立 metadata，不是另一种执行状态。

entrypoint return 通过 completion effect 提交。只有 Python process 正常退出且 final
usage 已记录，scheduler 才 commit completed。未捕获 Python、model、tool、protocol 或
sandbox error 会使 Session 失败。archive 会取消 active Session，同时保留历史。
