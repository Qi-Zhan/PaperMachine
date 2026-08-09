# Per-Action ToolRegistry 重构与发布验证

日期：2026-08-09

## 结果

PaperMachine 已删除 Agent 级工具旁路，改为由 host 在每个 Turn 创建前从可信
`ToolCatalog` 构造精确、不可变的 `ToolRegistry`：

- Workflow Action 通过 `@action(tools=[...])` 声明全部本地工具，裸 `@action`
  等价于 `tools=[]`；
- Workspace 工具按 Turn 的 materialized access 上限过滤；
- Project 工具只能经 Workflow Action 声明进入，普通用户 Session 永远不会自动获得；
- hosted web search 不属于该列表，仍由 access、`search_context_size` 与 provider
  capability 独立决定；
- `ActionInvocation.requested_tools` 保存声明请求，Turn 原子保存排序后的 definitions
  与 SHA-256 `ToolSetSnapshot`；
- model exposure、dispatch、pause/resume 与 crash recovery 都从该快照重建；executor
  缺失、definition 漂移或 hash 损坏时 fail closed；
- ToolRegistry 只处理 membership、dispatch、parallel policy 与 reconciliation，文件、
  网络、managed-state deny 和 sandbox 检查仍留在工具内部形成 defense in depth。

当前数据库 schema 为 5。本轮没有迁移或兼容读取；旧 data dir 未改动，全部真实验证
使用新建的临时 data dir。

## Clean break

公共 DSL 现在是：

```python
@action(
    tools=[
        "read_project_home",
        "patch_project_home",
        "preview_project_home",
    ]
)
async def maintain_project_home(...):
    ...
```

代码、测试、Web wire types 与文档中均不再存在被删除的两个旧标识。Action
declaration、Rust catalog validation 与 Turn materialization 都会拒绝未知、非法或重复
名称；finalize 与 JSON repair Turn 强制使用空 Registry。

## 自动验证

在 macOS 当前 checkout 上完成：

- `cargo fmt --all -- --check`；
- `cargo test --workspace`：133 个 Rust tests 全部通过，包括真实子进程 SIGKILL
  recovery matrix；
- `cargo clippy --workspace --all-targets -- -D warnings`；
- Python DSL/built-in tests：55 个通过；
- benchmark runtime、BrowseComp、Deep Research、LiveDR 四组 runner tests：共 43 个通过；
- development launcher tests：2 个通过；
- Web tests：7 个通过；
- Web production build；
- `git diff --check`。

覆盖的关键边界包括 catalog 注册冲突、未知工具、access 过滤、Project 工具只限
Workflow Action、普通 Session 排除 Project 工具、同一持久 Agent 的不同 Action 使用
不同 Registry、full access 仍拒绝 managed state、ToolSet 缺失/definition 漂移/hash
损坏 fail closed，以及 prepared/executing tool recovery。

## GLM 首次写入与 DeepSeek 刷新

可重复入口：`scripts/project_summary_toolset_dogfood.py`。脱敏原始证据：
[`project-summary-toolset-dogfood-2026-08-09.json`](project-summary-toolset-dogfood-2026-08-09.json)。

同一 fresh Project 中：

1. Aeroides GLM 5.2 从空白页完成首次写入；
2. 一个无 Agent 的确定性 Workflow 发布新的 verified-note Artifact；
3. DeepSeek V4 Flash 读取 GLM 页面作为 base，先 preview，再 patch，并再次 preview 后发布。

两次 Summary 均为 1 个 ActionInvocation、1 个 ActionAttempt、1 个 completed Turn，
没有 model retry、Action retry 或 terminal failure；最终 preview diagnostics 均为 0，
tool trial failure 也均为 0。两次 Turn 的 definitions 都严格等于：

```text
patch_project_home
preview_project_home
read_project_home
```

它们共享同一个 ToolSet SHA-256：
`785c8cc1064f4db53f99614a5f706f183e28547e0a63e6c61ab3657abc4c0ef5`，没有任何
Workspace 工具。DeepSeek 首次 `read_project_home` 返回的 `base_artifact_id` 精确等于
GLM 发布的 page Artifact ID。

GLM Turn 使用 7,071 input / 1,068 output tokens，其中 5,952 input tokens 命中缓存；
DeepSeek Turn 使用 16,033 input / 2,535 output tokens，其中 11,776 input tokens 命中
缓存。Summary access 为 `model_only`，两次 hosted search 调用均为 0。

最初两次混合 runner 在 DeepSeek 采样前收到 401，因为宿主进程与 ignored `.env` 都
仍指向旧 key；换成用户提供的有效 key 后从第三个全新 data dir 完整重跑通过。该认证
准备失败没有进入成功 run 的 Workflow retry 或 ActionAttempt 统计。

## DeepSeek SIGKILL 与权限 dogfood

可重复入口：`scripts/deepseek_recovery_dogfood.py`。脱敏原始证据：
[`deepseek-recovery-dogfood-2026-08-09.json`](deepseek-recovery-dogfood-2026-08-09.json)。

同一个 DeepSeek Agent Session 连续执行四个 Action：

- 首个 create/verify Turn 只有 `read_file` 与 `write_file`，ToolSet hash 为
  `90085d06c4ebbe5293146181ce381aa09bfef74722fa320b5f42eab6d5d1ca71`；
- 后续三个 denial Turn 都只有 `read_file`，ToolSet hash 为
  `0392ceb315a757a87cdf8436f9effea004d2eb277bfd760c58cbde483ddfe69f`。

Server 在首个 `write_file` 已持久进入 `executing` 后收到 SIGKILL。重启后同一个
Step ID、Turn ID、provider call ID 与 effect identity 被保留，幂等工具完成为
`completed`，proof 文件内容和 SHA-256 正确。四个 ActionInvocation 各只有一个
ActionAttempt；Workflow completed，rollout sequence 与 SQLite projection 都为 106。

真实模型随后逐一触发并观察到三类拒绝：Workspace `.env`、Workspace 外文件、
PaperMachine managed database。Project Workspace relocation 只增加 attachment revision，
managed state 与 proof 文件均保留；删除另一个 Project 后，其 managed state 删除而
用户 Workspace 文件保持不变。

## 范围

本轮只验证 macOS，不测试 Windows；未引入 deferred discovery、Code Mode、MCP、
plugins 或 connectors。Provider key 只存在于 ignored `.env` 和子进程环境，没有进入
Git、证据 JSON、测试 fixture 或 server 日志。

核心边界仍是：Project 是 PaperMachine 持久管理的研究世界；Workspace 是 Agent 获准
操作的用户文件系统；两者通过结构化 runtime API 相连，永远不共享存储和安全边界。
