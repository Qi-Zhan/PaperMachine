# Workflow 语言语义

本文只描述当前 clean-break runtime 已实现的 Python DSL，不记录已删除的
兼容接口或未来设想。

## 领域模型

> Project 是 PaperMachine 持久管理的研究世界；Workspace 是 Agent 获准操作的
> 用户文件系统；两者通过结构化 runtime API 相连，永远不共享存储和安全边界。

- `Project` 拥有 Session、Workflow run、prompt、Skill、Artifact 与 journal；这些
  状态全部位于 PaperMachine managed data root。
- `Workspace` 是挂到 Project 上的一个用户绝对目录，只作为 Agent cwd 与写入边界，
  从来不是 Project 存储。
- `Session` 是 Project 直属的、持久多轮模型会话。
- `Workflow` 是某份不可变 Python source snapshot 的一次执行。
- 一个 `Agent` instance 对应一个普通 Project Session，生命周期等于所属 Workflow。
- `ActionInvocation` 是一次逻辑 Action 调用；它的 `ActionAttempt` 恰好拥有一个
  Turn。interrupt 可以为同一 Invocation 新建 Attempt。
- `Turn` 是一次用户请求或 Workflow Action 的模型执行边界。普通聊天由 built-in
  `interactive-agent` Workflow 实现，因此两者走同一条内核路径。
- `WorkflowEffect` 在确定性逻辑路径上记录一次精确 host operation。

所有请求都通过 Project-scoped route 访问实体；不存在全局 entity lookup 或 ownership
index。

## Program 与 launch

一个 source 必须且只能有一个 async `@workflow(...)` entrypoint。literal manifest
包含 `slug`、`name`、`description`、`request_mode` 与 `params_schema`。validator 还会
记录 Agent class 及每个 Action 的静态工具声明；有 error diagnostic 的 source 不可运行。

launch 时一次性冻结：

- source、manifest、source SHA-256 与 Python runtime ABI SHA-256；
- `request_mode="required"` 下的一条具体 `request`；
- 通过校验的 `params`、可选 run `instructions` 与 launch provenance；
- 显式选择的 model profile、skills、access ceiling 与 Agent overrides；
- fresh context 或一个有界、不可变 Project snapshot。

runner 分别暴露 `ctx.request`、`ctx.params`、`ctx.instructions`、`ctx.trigger` 和
`ctx.context`。Workflow 必须把 Action 真正需要的数据显式传入；runtime 不会把 request
或 Project data 偷偷升级成 system instructions。

`request_mode="none"` 用于通过 `ask_human` 取得消息的持久交互。New Session 就是这条
路径；内核没有另一套直接 submit Session 的接口。

## 公共 DSL

完整公共表面有意保持很小：

```python
Agent
@action(...)
@workflow(...)
await together(...)
await ask_human(...)
await wait(seconds=... | minutes=..., name=...)
await ctx.project.snapshot(...)
await publish_artifact(...)
await publish_project_home(action=...)
```

其他控制流直接使用 Python `if`、`for`、`while`。周期执行就是普通 loop 加 durable
`wait`。

构造 `Agent(...)` 只产生本地对象。第一次 remote operation 才创建 participant Session。
`await agent.set_access(profile)` 也会先 materialize Session，因此升级不能伪装成构造
参数修改。Participant 是不可变 membership record，没有独立 lifecycle 状态。

## Action 与 Turn

`@action` method 是声明：prompt/docstring、bound arguments、model options 与 tool list
共同描述一个模型 Turn；method body 不作为 Agent logic 执行。

await Action 后运行统一 sample/tool/follow-up loop：

1. 创建不可变 Turn；
2. 采样模型；
3. 执行该 Turn Registry 中、模型实际请求的本地工具；
4. 追加 tool output 并继续采样；
5. 模型给出 terminal assistant message 或 runtime control 时结束。

`dict`、`list`、`bool`、`int`、`float` typed return 请求 JSON parsing。JSON repair 与
`finalize="after_search"` 使用独立 no-tool Action Turn，不会获得隐藏工具。

`ask_human` 返回的字符串带有 `HumanRequestId`，类型为 `HumanMessage`。只有把它传给
标注为 `HumanMessage` 的参数，Workflow 才能创建 user-origin Turn。Rust 会验证 request、
answer、Session 与 exact text。

每个 Turn 冻结四份互相独立的 snapshot：

| Snapshot | 含义 |
|---|---|
| `ModelRouteSnapshot` | provider、upstream model、capabilities、context window、reasoning effort、非秘密配置 hash |
| `TurnEnvironmentSnapshot` | Workspace revision 与 materialized authorization |
| `ToolSetSnapshot` | 精确排序后的本地工具定义与 SHA-256 |
| `PromptSnapshot` | 有序 resolved prompt layers 与 SHA-256 |

恢复时任何 snapshot 无法精确重建都会 fail closed。

## Tool 与权限

`@action(tools=[...])` 声明该 Action 请求的全部本地工具；bare `@action` 等于
`tools=[]`。host 拒绝未知名字，并为 Turn 构造精确、不可变 Registry。

- Workspace tools 按 Agent 的 materialized access 过滤；
- Project tools 只有被当前 Workflow Action 明确声明才会进入；
- hosted web search 不在该列表中，由 provider capability、access 与
  `search_context_size` 共同决定；
- 普通 interactive Session 获得 access 允许的 Workspace tools，但永远不自动获得
  Project tools。

Registry membership 与权限是两层独立检查。Registry 决定模型能看到和调用什么；
filesystem、command、network、managed-root 与 credential rule 仍由工具和 sandbox
强制执行。

`model_only`、`read_only`、`workspace`、`research`、`full_access` 构成有序 ceiling。
Workflow launch 固定 run ceiling；Agent override 不可超过它。降级在 Turn 之间直接生效，
ceiling 内的升级会打开 typed HumanRequest。已创建 Turn 保留自己的 access snapshot。

## 并发、人工输入与 durable wait

`await together(a(), b(), ...)` 使用 `asyncio.gather`，按参数顺序返回。同一 Agent 的
两个直接 Action 会被拒绝，因为一个 Session 同时只能有一个 active Turn。不同 Agent
Session 可在 server-wide permit 内并发。

`ask_human` 与 `wait` 是 replayable suspension effects。`wait` 只有一条 journal record；
deadline 由 `WorkflowEffect.started_at + interval` 得出；effect journal 是这次等待唯一的
持久状态。

当所有 live effect future 都停在 replayable wait 时，Rust 结束空闲 Python process 并
释放 permit。合法 human answer 或到期 deadline 会重新执行不可变 source；已完成 effect
直接返回保存结果，因此不会重复已完成的 domain mutation。

Control message 状态为 `pending -> claimed -> applied`：

- `guide` 在下一次 sample 前进入 canonical context；
- `finish` 强制下一次 sample 无工具并给出最终回答；
- `interrupt` 结束当前 Attempt，并允许同一 Invocation 以新 Attempt 继续；
- pause 在 checkpoint 等待，resume 继续，cancel 终止 run。

只有真正消费 control 的 canonical checkpoint 或 terminal transaction 才会把 claim
变为 applied；checkpoint 前崩溃时，同一个 Turn 可以重新领取。

## Project API

`ctx.project.snapshot()` 读取有界 Project-managed state，不读取 Workspace 文件。把旧
`cursor` 作为 `after_cursor` 传回可得到 committed delta。`publish_artifact` 写入
确定性的 Project-managed content。

Project Home 同样位于 managed state。普通 Action 显式声明
`read_project_home`、`patch_project_home`、`preview_project_home`，可反复检查和修正，
再把那一个已经 await 的 `_ActionCall` 传给 `publish_project_home`。发布会验证精确
Action provenance 与 ToolSet membership，并以 draft revision 做 CAS。内核不信任任何
Workflow slug，也没有特殊 Summary Agent 分支。

## 持久化与恢复

Python host effect 与 model tool call 刻意采用不同恢复契约。

Workflow effect 使用确定性 logical path 与 request hash。completed effect replay 保存的
result；同一路径换 payload 会 fail closed。started host effect 只有在其 domain contract
幂等时才重新 dispatch。

每个 Session JSONL 是 canonical model history，并且只包含：

```text
TurnCreated
ContextCheckpoint
TurnUpdated
```

SQLite Step 与 Session event 是 query/UI projection，不是 canonical rollout item；
streaming delta 只存在于实时事件流。

已验证的 `FunctionCall` 必须先进入 `ContextCheckpoint`，之后才允许 dispatch。
`FunctionCallOutput` 必须先 checkpoint，之后才完成 Step 或继续下一次 sample。恢复时：

- call/output pair 修复缺失的 Tool Step projection；
- 没有 output 的 call 恰好补一次 JSON string `"aborted"`；
- 旧 call 永不重新 dispatch；
- 同一个 Agent 继续，并先观察 durable reality，再决定是否发出新 call。

没有 `ModelSampleCommitted` aggregate、effect-disposition enum 或 model-tool
reconciliation API。

## 状态与完成

Workflow status 为 `created`、`running`、`waiting_for_user`、
`waiting_for_deadline`、`paused`、`completed`、`failed`、`cancelled`。
`waiting_for_deadline` 表示 durable `wait` effect 尚未到期。

entrypoint return 通过 `complete` effect 提交。只有 Python process 正常退出且 final usage
已记录，scheduler 才 commit `completed`。未捕获 Python、model、tool、protocol 或 sandbox
错误会 fail run。关闭 Session 会 archive 它并取消拥有它的 active Workflow，但不删除
历史。

effect 之间的纯 Python 代码可能在 restart 后重跑，因此相同 source snapshot 与 inputs
必须产生相同 effect 顺序和 payload。
