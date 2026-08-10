# Project and Workspace semantics / Project 与 Workspace 语义

> Project is a research world persistently managed by PaperMachine; Workspace
> is the user filesystem an Agent is authorized to operate; structured runtime
> APIs connect them, and they never share storage or a security boundary.

> Project 是 PaperMachine 持久管理的研究世界；Workspace 是 Agent 获准操作的
> 用户文件系统；两者通过结构化 runtime API 相连，永远不共享存储和安全边界。

## Project

A Project is an identity and ownership boundary, not the directory in which an
Agent happens to run. Its authoritative row, WorkflowPrograms, Sessions,
Agents, immutable program and prompt snapshots, Skills, Artifacts, event
projections, effect journals, and Agent rollouts live below
`<data_dir>/projects/<project-id>/`. Python Workflow code and Agent tools cannot
read or write this managed root.

Project 是 identity 与 ownership boundary，不是 Agent 的工作目录。它的权威记录、
WorkflowProgram、Session、Agent、不可变 program/prompt 快照、Skill、Artifact、
event projection、effect journal 与 Agent rollout 都位于
`<data_dir>/projects/<project-id>/`。
Python Workflow 与 Agent 工具都不能读写这个 managed root。

Deleting a Project retires and deletes only that managed world. Its attached
Workspace is never a deletion target. If the Workspace disappears, the Project
and all managed history remain inspectable.

删除 Project 只会退役并删除这份 managed research world，挂载的 Workspace 永远
不是删除目标。即使 Workspace 消失，Project 与全部 managed history 仍可检查。

## Workspace

A Workspace attachment has a stable `WorkspaceId`, a monotonically increasing
revision, and one canonical absolute directory used as cwd. It
contains user files only. PaperMachine writes no hidden application state,
database, journal, prompt, or Skill file into it.

Workspace attachment 包含稳定的 `WorkspaceId`、单调递增的 revision，以及一个
作为 cwd 的 canonical 绝对目录。它只保存用户文件；PaperMachine 不会
在其中写入隐藏应用状态、数据库、journal、prompt 或 Skill 文件。

When the user does not choose a Workspace during Project creation, PaperMachine
creates a unique directory below `~/Documents/PaperMachine/<project-name>`.
Existing directories are never claimed implicitly; a numeric suffix is used
instead. This default directory has the same lifecycle as any explicitly
selected Workspace and is preserved when the Project is deleted.

用户创建 Project 时可以不选择 Workspace。PaperMachine 会在
`~/Documents/PaperMachine/<project-name>` 下创建不冲突的默认目录；如果同名目录
已经存在，则使用数字后缀，不会自动接管已有目录。默认目录与用户显式选择的
Workspace 具有相同生命周期，删除 Project 时仍会保留。

An explicitly selected Workspace is represented by the structural payload:

```json
{"workspace":{"path":"/absolute/user/path"}}
```

The local UI can open the operating system directory picker, and also accepts
an absolute path directly. The server canonicalizes and validates the path,
rejects equality or nesting with managed state or another Project attachment, and returns
the attachment object plus `workspace_available`. Relocation updates only the
attachment and its revision; it does not move either managed Project state or
user files.

本地 UI 可以唤起操作系统目录选择器，也允许直接输入绝对路径。server 会
canonicalize 并校验该路径，拒绝它与 managed state 或其他 Project attachment
相同或互相嵌套，并返回 attachment object 与 `workspace_available`。Relocation 只更新
attachment 及 revision，不移动 managed Project state，也不移动用户文件。

## Runtime connection

Before a Turn is created, the Store verifies that the attached path still
exists as a real directory at its recorded canonical path. It then
materializes the Agent access preset, bounded by its Session ceiling, into a
`TurnEnvironmentSnapshot`:

- Workspace attachment ID, revision, path, and cwd;
- exact filesystem, network, and child-environment policy;
- protected PaperMachine-managed roots; and
- a SHA-256 authorization hash.

创建 Turn 前，Store 会确认挂载路径仍是 recorded canonical path 上的真实目录，
再把受 Session ceiling 约束的 Agent access preset 实体化成
`TurnEnvironmentSnapshot`，包括 Workspace ID/revision/path/cwd、精确的
文件/网络/子进程环境策略、受保护的 managed roots 与权限 SHA-256。

`read_only`, `workspace`, and `full_access` may read ordinary host files.
`workspace` writes only inside the Workspace; `full_access` may write ordinary
host files. Every profile is denied PaperMachine managed roots, and
non-`full_access` also denies credential paths. Relative paths resolve from the
Workspace cwd. `exec_command`, `write_stdin`, and `apply_patch` share this
materialized policy.

`read_only`、`workspace` 与 `full_access` 可以读取普通宿主机文件；`workspace`
只能写 Workspace，`full_access` 可以写普通宿主机文件。所有档位都不能读写
PaperMachine managed roots，非 `full_access` 还会拒绝 credential 路径。相对路径
始终以 Workspace cwd 解析，`exec_command`、`write_stdin` 与 `apply_patch` 使用
同一份 materialized policy。

At the same boundary, the host ToolCatalog creates one exact ToolRegistry.
Bare Actions receive collaboration tools plus access-allowed native tools;
`tools=[]` means none and a non-empty list selects an exact subset. Sorted
definitions and a SHA-256 are persisted in `ToolSetSnapshot`. Model exposure,
dispatch, pause/resume, and recovery rebuild from that immutable snapshot;
tools and process sandboxes still enforce authorization internally. Prompt
text cannot change either boundary. Later relocation, permission changes, or
Action calls affect only later Turns.

在同一创建边界，host ToolCatalog 会生成一个精确 ToolRegistry。bare Action 获得
协作工具和 access 允许的 native tools；`tools=[]` 表示空 Registry，非空列表选择
精确子集。排序后的 definitions 与 SHA-256 固化在 `ToolSetSnapshot`。model
exposure、dispatch、pause/resume 与 recovery 都从该快照重建；tool 与 process
sandbox 仍在内部执行 authorization。prompt 无法改变任一边界，后续 relocation、
权限变化或 Action 调用只影响后续 Turn。

Project Summary is ordinary Agent work with an empty ToolSet. The Workflow
runtime obtains bounded Project entity snapshots through
`ctx.project.changes(exclude_current_program=True)` and passes them as Action
data. This prevents earlier Summary runs from becoming new evidence. Summary
source, canonical page, and history remain in managed Project state; other
Agents do not see them through Workspace.

Project Summary 是 `tools=[]` 的普通 Agent 工作。Workflow runtime 通过
`ctx.project.changes(exclude_current_program=True)` 获取有界 Project entity
snapshot，再把它们作为 Action data 传入，因此旧 Summary 运行不会反过来成为新
evidence。Summary source、canonical page 与历史留在 managed Project state，其他
Agent 不会通过 Workspace 看到它们。

Agent collaboration exchanges durable messages and Action results, not file
authority. A child inherits the parent's Workspace attachment but may only keep
or lower access. `list_agents` never exposes transcripts or managed files.

Agent 协作交换的是持久消息与 Action result，不是文件权限。child 继承 parent 的
Workspace attachment，但 access 只能保持或降低；`list_agents` 不暴露 transcript
或 managed file。

## Recovery and inspection

The Session API exposes its Agents and each Turn environment, ModelRouteSnapshot,
ToolSetSnapshot, Tool Step status, canonical rollout sequence, and SQLite
projection sequence. A canonical FunctionCall without output recovers as
`"aborted"` and is never dispatched again. Every Turn is recovered through
its owning Agent, ActionAttempt, and Session.

Session API 会暴露其 Agent，以及每个 Turn 的 environment、ModelRouteSnapshot、
ToolSetSnapshot、Tool Step 状态、canonical rollout 序号和 SQLite projection 序号。
canonical FunctionCall 若缺少 output，恢复时会得到 `"aborted"`，旧 call 永不再次
dispatch。每个 Turn 通过所属 Agent、ActionAttempt 与 Session 恢复。
