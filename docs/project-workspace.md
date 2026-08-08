# Project and Workspace semantics / Project 与 Workspace 语义

> Project is a research world persistently managed by PaperMachine; Workspace
> is the user filesystem an Agent is authorized to operate; structured runtime
> APIs connect them, and they never share storage or a security boundary.

> Project 是 PaperMachine 持久管理的研究世界；Workspace 是 Agent 获准操作的
> 用户文件系统；两者通过结构化 runtime API 相连，永远不共享存储和安全边界。

## Project

A Project is an identity and ownership boundary, not the directory in which an
Agent happens to run. Its authoritative row, Sessions, Workflows, immutable
program and prompt snapshots, Skills, Artifacts, event projections, effect
journal, and Session rollouts live below
`<data_dir>/projects/<project-id>/`. Python Workflow code and Agent tools cannot
read or write this managed root.

Project 是 identity 与 ownership boundary，不是 Agent 的工作目录。它的权威记录、
Session、Workflow、不可变 program/prompt 快照、Skill、Artifact、event projection、
effect journal 与 Session rollout 都位于 `<data_dir>/projects/<project-id>/`。
Python Workflow 与 Agent 工具都不能读写这个 managed root。

Deleting a Project retires and deletes only that managed world. Its attached
Workspace is never a deletion target. If the Workspace disappears, the Project
and all managed history remain inspectable.

删除 Project 只会退役并删除这份 managed research world，挂载的 Workspace 永远
不是删除目标。即使 Workspace 消失，Project 与全部 managed history 仍可检查。

## Workspace

A Workspace attachment has a stable `WorkspaceId`, a monotonically increasing
revision, canonical absolute roots, and one primary root used as cwd. It
contains user files only. PaperMachine writes no hidden application state,
database, journal, prompt, or Skill package into it.

Workspace attachment 包含稳定的 `WorkspaceId`、单调递增的 revision、canonical
绝对根目录，以及作为 cwd 的 primary root。它只保存用户文件；PaperMachine 不会
在其中写入隐藏应用状态、数据库、journal、prompt 或 Skill package。

Project creation and relocation use the structural payload:

```json
{"workspace":{"roots":["/absolute/user/path"],"primary_root":0}}
```

The server canonicalizes and validates every root, rejects overlap with managed
state or another Project attachment, and returns the attachment object plus
`workspace_available`. Relocation updates only the attachment and its revision;
it does not move either managed Project state or user files.

server 会 canonicalize 并校验每个 root，拒绝它与 managed state 或其他 Project
attachment 重叠，并返回 attachment object 与 `workspace_available`。Relocation
只更新 attachment 及 revision，不移动 managed Project state，也不移动用户文件。

## Runtime connection

Before a Turn is created, the Store verifies that the attached roots still
exist as real directories at their recorded canonical paths. It then
materializes the Session access preset into a `TurnEnvironmentSnapshot`:

- Workspace attachment ID, revision, roots, and cwd;
- exact filesystem, tool, network, and child-environment policy;
- protected PaperMachine-managed roots; and
- a SHA-256 authorization hash.

创建 Turn 前，Store 会确认挂载 root 仍是 recorded canonical path 上的真实目录，
再把 Session access preset 实体化成 `TurnEnvironmentSnapshot`，包括 Workspace
ID/revision/roots/cwd、精确的文件/工具/网络/子进程环境策略、受保护的 managed
roots 与权限 SHA-256。

Model tool exposure, ToolRegistry dispatch, direct tools, and process sandbox
construction all consume this same immutable snapshot. Prompt text cannot
change it. A later Workspace relocation or Session permission change affects
only later Turns.

model tool exposure、ToolRegistry dispatch、direct tool 与 process sandbox
construction 都消费同一份不可变快照；prompt 文本不能改变它。后续 Workspace
relocation 或 Session 权限修改只影响之后创建的 Turn。

## Recovery and inspection

The Session API exposes each Turn environment, Tool Step effect disposition and
execution state, canonical rollout sequence, SQLite projection sequence, and
standalone interrupted Turns eligible for explicit Resume. Resume creates a new
Turn over committed Session context. The old Turn remains `interrupted`; a
Workflow-owned Turn is recovered only by its Workflow runtime.

Session API 会暴露每个 Turn 的 environment、Tool Step effect disposition 与
execution state、canonical rollout 序号、SQLite projection 序号，以及可以显式
Resume 的独立 interrupted Turn。Resume 基于已提交 Session context 创建新 Turn；
旧 Turn 保持 `interrupted`，Workflow-owned Turn 只由其 Workflow runtime 恢复。
