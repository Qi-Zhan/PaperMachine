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
materializes the Session access preset into a `TurnEnvironmentSnapshot`:

- Workspace attachment ID, revision, path, and cwd;
- exact filesystem, tool, network, and child-environment policy;
- protected PaperMachine-managed roots; and
- a SHA-256 authorization hash.

创建 Turn 前，Store 会确认挂载路径仍是 recorded canonical path 上的真实目录，
再把 Session access preset 实体化成 `TurnEnvironmentSnapshot`，包括 Workspace
ID/revision/path/cwd、精确的文件/工具/网络/子进程环境策略、受保护的 managed
roots 与权限 SHA-256。

`read_only`, `workspace`, and `research` may read ordinary host files, while
only `workspace`/`research` may write inside the Workspace; non-`full_access`
never writes outside it. Every profile is denied PaperMachine managed roots,
and non-`full_access` also denies credential paths. Relative paths resolve from
the Workspace cwd, and direct file tools and command sandboxes share this rule.

`read_only`、`workspace` 与 `research` 可以读取普通宿主机文件，但只有
`workspace`/`research` 能在 Workspace 内写入；非 `full_access` 永远不能在
Workspace 外写入。所有档位都不能读写 PaperMachine managed roots，非
`full_access` 还会拒绝 credential 路径。相对路径始终以 Workspace cwd 解析，直接
文件工具与 command sandbox 使用同一规则。

At the same boundary, the host ToolCatalog creates one exact ToolRegistry. For
a Workflow Action, `@action(tools=[...])` supplies the candidates and access
filters its Workspace tools; declared Project tools are admitted only on that
path. Sorted definitions and a SHA-256 are persisted in the Turn's
`ToolSetSnapshot`. Model exposure, dispatch, pause/resume, and recovery rebuild
from that immutable snapshot; direct tools and process sandboxes still enforce
the authorization context internally. Prompt text cannot change either
boundary. Later relocation, permission changes, or Action calls affect only
later Turns.

在同一创建边界，host ToolCatalog 会生成一个精确 ToolRegistry。Workflow Action
以 `@action(tools=[...])` 提供候选工具，再由 access 过滤其中的 Workspace 工具；
Project 工具只允许通过这条路径进入。排序后的 definitions 与 SHA-256 固化在 Turn 的
`ToolSetSnapshot` 中。model exposure、dispatch、pause/resume 和 recovery 都从这份
不可变快照重建；direct tool 与 process sandbox 仍在内部执行 authorization 检查。
prompt 文本无法改变任一边界，后续 relocation、权限变化或 Action 调用只影响后续 Turn。

Project Summary is ordinary Agent work but uses Project tools, not filesystem
access. Its source, draft, canonical page, and history remain in managed Project
state. Other Agents do not see them through Workspace and never receive those
tools automatically; Workflow code must explicitly request a bounded Project
snapshot or declare the relevant Project tools on an Action.

Project Summary 是普通 Agent 工作，但它使用 Project 工具，而不是文件系统权限。
其 source、draft、canonical page 与历史留在 managed Project state。其他 Agent
不会通过 Workspace 看到这些内容，也不会自动获得这些工具；Workflow 必须显式读取
有界 Project snapshot，或在 Action 上声明相应 Project 工具。

## Recovery and inspection

The Session API exposes each Turn environment, ModelRouteSnapshot,
ToolSetSnapshot, Tool Step status, canonical rollout sequence, and SQLite
projection sequence. A canonical FunctionCall without output recovers as
`"aborted"` and is never dispatched again. Every Turn is recovered only through
its owning Workflow.

Session API 会暴露每个 Turn 的 environment、ModelRouteSnapshot、ToolSetSnapshot、
Tool Step 状态、canonical rollout 序号和 SQLite projection 序号。canonical
FunctionCall 若缺少 output，恢复时会得到 `"aborted"`，旧 call 永不再次 dispatch。
每个 Turn 只通过其所属 Workflow 恢复。
