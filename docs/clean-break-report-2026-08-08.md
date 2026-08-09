# PaperMachine runtime clean-break 验收报告

日期：2026-08-08  
验证主机：macOS（Windows 按当前产品范围明确不测试）  
真实 provider：`deepseek-flash` → `deepseek` → `deepseek-v4-flash`

## 结论

本轮 clean break 已达到目标：

> Project 是 PaperMachine 持久管理的研究世界；Workspace 是 Agent 获准操作的用户文件系统；两者通过结构化 runtime API 相连，永远不共享存储和安全边界。

旧隐藏状态目录及其代码、测试、文档和 UI 文案已全部删除；旧
provider/Project/Workspace 兼容路径已删除；只证明旧接口不存在的负面测试已清理。
Project Store、Session rollout、工具 effect identity、恢复状态和 Workspace attachment
均只有一个当前语义。`goal` 仍只是普通 built-in Python Workflow，不拥有 Rust runtime
特权。

## 分阶段提交

| 阶段 | Commit | 结果 |
| --- | --- | --- |
| 0 | `3fe39a6` | 固化 runtime kernel 不变量 |
| 1 | `3c3f2ce` | 物化 Workspace authorization |
| 2 | `87c60ba` | 统一 sandbox manager |
| 3 | `f45b82a` | 每个 Project database 成为唯一权威 |
| 4 | `97228c0` | Session rollout 成为 canonical history |
| 5 | `b37ec65` | effect disposition 与进程恢复语义 |
| 6 | `40cf38e` | clean API、UI、文档及显式恢复入口 |
| 7 | `11b62ca` | 真实 server 进程的 SIGKILL fault matrix |

## 全量验证

最终验证均从 `11b62ca` clean worktree 运行：

| 验证项 | 结果 |
| --- | --- |
| `cargo test --workspace --all-targets` | 127 passed；包含真实 server SIGKILL matrix |
| `cargo clippy --workspace --all-targets -- -D warnings` | passed |
| `cargo fmt --all -- --check` | passed |
| Python DSL/runtime tests | 50 passed |
| dev-server tests | 2 passed |
| BrowseComp runner tests | 10 passed |
| Deep Research runner tests | 14 passed |
| LiveDR runner tests | 16 passed |
| Web tests | 7 passed |
| Web production build | passed |
| `git diff --check` | passed |

Python 验证使用标准库 `unittest`；当前系统 Python 没有安装 pytest，因此没有为了测试
改变 Python 环境。Phase 7 还单独通过了 server/store targeted tests 和 release
`cargo check`，确认 fault CLI 只存在于 debug build，release boundary 调用为 no-op。

## 进程级 fault matrix

`crates/server/tests/process_recovery.rs` 启动真实 `papermachine-server` 二进制和本地
Responses-compatible provider，在以下磁盘边界暂停进程并由父进程发送 `SIGKILL`：

| 崩溃边界 | 已验证的恢复结果 |
| --- | --- |
| rollout 已 fsync、SQLite projection 尚未提交 | 重启后 journal replay 与 projection sequence 收敛 |
| terminal answer 已 checkpoint、Turn 尚未 terminal commit | 不重新 sample，直接提交原答案 |
| Workflow model sample 在飞行中 | 恢复同一个 ActionAttempt 并重新 sample |
| unknown-effect tool 仍为 `prepared` | 重启后跨 execution boundary 并且只执行一次 |
| unknown-effect tool 已为 `executing` | 标记 `execution_unknown`，绝不自动重放 |

## 真实 DeepSeek 强制重启

可重复入口为 `scripts/deepseek_recovery_dogfood.py`；脚本只从 ignored `.env` 向 server
子进程传递凭证，不输出或写入凭证。

本次真实对象：

- Project：`019fe165-5d45-7b41-824b-8d26d9ba6e2c`
- Workflow：`019fe165-5ea7-7022-af84-f477cebb011a`
- Session：`2c54a7ba-f49a-50a7-8fdb-3417d1162323`
- 首个 Turn：`019fe165-5efb-78e3-afde-340a15024dbc`
- faulted Tool Step：`019fe165-661d-7e70-89ec-63f0ddca0578`
- provider call ID：`call_00_be6AsS6qFJ5sNIjwjNQe0606`

DeepSeek 按 Action contract 生成的第一个调用是 `write_file`。崩溃前，同一 Step 为：

```text
effect_disposition = idempotent
execution_state    = executing
status             = running
output             = null
```

server PID `67615` 随后收到真实 `SIGKILL`，重启 PID 为 `67644`。重启后没有新建 Tool
Step，也没有更换 Turn、Session 或 call ID；原 Step 变为：

```text
execution_state = completed
status          = completed
bytes_written   = 35
```

最终 Workflow 为 `completed`。4 个 Action invocation 各只有 1 个 ActionAttempt，没有
Workflow retry、Action retry 或 terminal failure。Session rollout
`last_sequence=105`、`projected_sequence=105`。共持久化 9 个真实 provider request
metadata，全部为 `deepseek-flash` / `deepseek` / `deepseek-v4-flash`、HTTP SSE 和同一
Session prompt-cache key。

恢复后的 Workspace 文件内容精确为
`deepseek-recovery-proof-2026-08-08\n`，SHA-256 为
`7128859f542ca8503474cb49b3c94c73a38eee2c7bbfc4c97c65a94823e52422`。

## 真实权限拒绝

同一 DeepSeek Agent 在后续三个 Action 中实际发起了 `read_file`，而不是口头预测权限
结果。三个调用均跨入并完成工具边界，但以 `failed` 返回物化授权错误：

| 探针 | Tool call ID | 结果 |
| --- | --- | --- |
| Workspace 中的 `.env` | `call_00_AYkCuL5JEIgvvlJJTPpd7580` | sensitive Workspace credential denied |
| Workspace 外部文件 | `call_00_cSjymm4080jQxbKiUFqA8704` | outside Session Workspace denied |
| PaperMachine-managed `project.db` | `call_00_2Vo5hWLXAvz7pl6kbh7g2103` | managed Project state denied |

被保护文件的内容没有进入 evidence；对 evidence JSON 的凭证和 sentinel 扫描通过。

## Project / Workspace 生命周期

Recovery Project 的 Workspace attachment ID 始终为
`019fe165-5d56-7be1-bc3a-24e6afe9ebc9`。用户移动目录后，结构化 reattach 将 revision
从 1 增加到 2，根路径从 `workspace-original` 更新为 `workspace-relocated`；Project
database 原路径不变，proof 文件随用户 Workspace 保留。第一个 Turn 的 immutable
environment snapshot 仍记录 revision 1，证明历史授权没有被 retroactive rewrite。

另建 Project `019fe165-a1d9-7fd2-8a18-3e3ed1636dfc` 后执行 DELETE：其 managed
Project state 已移除，Workspace 目录与 `user-owned.txt` 均保留，文件 SHA-256 为
`42556cff3fc6ff7fcf336d4287f4c64f51cb30397c487b664cfbf28349097e40`。

原始 dogfood 数据暂存于 evidence 中记录的 macOS 临时目录；仓库中的 JSON 是经过字段
筛选和 secret scan 的审计证据。Windows 按用户当前范围没有运行，也不据此宣称通过。
