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
{"workspace":{"roots":["/absolute/user/path"],"primary_root":0}}
```

The local UI can open the operating system directory picker, and also accepts
an absolute path directly. The server canonicalizes and validates every root,
rejects overlap with managed state or another Project attachment, and returns
the attachment object plus `workspace_available`. Relocation updates only the
attachment and its revision; it does not move either managed Project state or
user files.

本地 UI 可以唤起操作系统目录选择器，也允许直接输入绝对路径。server 会
canonicalize 并校验每个 root，拒绝它与 managed state 或其他 Project attachment
重叠，并返回 attachment object 与 `workspace_available`。Relocation 只更新
attachment 及 revision，不移动 managed Project state，也不移动用户文件。

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

At the same boundary, the host ToolCatalog creates one exact ToolRegistry. For
a Workflow Action, `@action(tools=[...])` supplies the candidates and access
filters its Workspace tools; declared Project tools are admitted only on that
path. A standalone user Turn receives all access-allowed Workspace tools and no
Project tools. Sorted definitions and a SHA-256 are persisted in the Turn's
`ToolSetSnapshot`. Model exposure, dispatch, pause/resume, and recovery rebuild
from that immutable snapshot; direct tools and process sandboxes still enforce
the authorization context internally. Prompt text cannot change either
boundary. Later relocation, permission changes, or Action calls affect only
later Turns.

在同一创建边界，host ToolCatalog 会生成一个精确 ToolRegistry。Workflow Action
以 `@action(tools=[...])` 提供候选工具，再由 access 过滤其中的 Workspace 工具；
Project 工具只允许通过这条路径进入。普通用户 Turn 获得 access 允许的全部 Workspace
工具，但不获得 Project 工具。排序后的 definitions 与 SHA-256 固化在 Turn 的
`ToolSetSnapshot` 中。model exposure、dispatch、pause/resume 和 recovery 都从这份
不可变快照重建；direct tool 与 process sandbox 仍在内部执行 authorization 检查。
prompt 文本无法改变任一边界，后续 relocation、权限变化或 Action 调用只影响后续 Turn。

## Recovery and inspection

The Session API exposes each Turn environment and ToolSetSnapshot, Tool Step effect disposition and
execution state, canonical rollout sequence, SQLite projection sequence, and
standalone interrupted Turns eligible for explicit Resume. Resume creates a new
Turn over committed Session context. The old Turn remains `interrupted`; a
Workflow-owned Turn is recovered only by its Workflow runtime.

Session API 会暴露每个 Turn 的 environment 与 ToolSetSnapshot、Tool Step effect disposition 与
execution state、canonical rollout 序号、SQLite projection 序号，以及可以显式
Resume 的独立 interrupted Turn。Resume 基于已提交 Session context 创建新 Turn；
旧 Turn 保持 `interrupted`，Workflow-owned Turn 只由其 Workflow runtime 恢复。
