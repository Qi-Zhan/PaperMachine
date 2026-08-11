# Workflow Language v1 语义

Workflow Language v1 是一个由 Rust 原生编译、解释的 Rust-like 动态工作流语言。
普通变量和函数参数不声明类型；只有启动 params、Action structured result 与
HumanRequest response 是显式 schema 边界。

## 程序结构

一个文件以 `version 1;` 开始，可声明 schema、Agent template、顶层函数，并且
必须且只能声明一个 Workflow。源码为 UTF-8，最大 128 KiB。v1 只有 `//` 注释、
普通字符串和三引号多行字符串；没有 import、module、宏、反射、eval、时钟、随机数
或任意 I/O。

Schema 支持 `any/string/bool/int/number/list/map/object/enum`、optional field、
default、长度和数值范围。`model_profile` 与 `access` 是启动表单边界的受控形式。
声明顺序就是 UI 顺序。

## 值与控制流

值包括 `null/bool/int/number/string/list/object`，以及不可伪造的 Agent、Action、
HumanMessage、Artifact 引用。`let` 不能重新绑定，`var` 可以；List/Object 本身不可变，
`append/extend/update` 返回新值。

条件必须严格为 bool，没有 truthiness 或隐式类型转换。缺字段、越界、除零、溢出和
错误运算立即失败。显式转换使用 `string/int/number`。

控制流包括 `if/else`、带 `_` 的穷尽 `match`、有限 `for`、`while`、`loop`、
`break/continue/return/await`。顶层函数可以 await effect，但禁止递归、闭包和高阶
函数。编译器检查调用图与 arity。

每条 `while/loop` 回边都必须可证明经过 durable `await`。每个 effect 之间还有公开的
1,000,000 IR-step fuel，effect 后重置；因此纯计算死循环不会占住 runtime。

## Agent 世界线与 Action

Agent 构造参数为 `key/name/role/system/model/skills/access`，默认 key 为 `"main"`。
身份是 `(Session, template, canonical key)`；首次 durable 配置被冻结，后续不一致会
fail closed。access override 按 template 名匹配，并受 Session ceiling 限制。

Action 无 result schema 时返回 terminal text；有 result 时返回同一 schema 校验后的
JSON 动态值。`finalize = if_needed` 的 work Turn 仍然正常使用工具、输出完整报告；
runtime 只是在 prompt 末尾自动附加 typed trailer。它先解析完整 JSON、单一 fenced
JSON 或首个 object/array；失败后至多执行一次无工具 finalizer 和两次 low-reasoning
repair。模型不是“只返回 active”，而是在正常工作报告之外提交结构化终态判断。

`finalize = after_search` 只在 hosted search 确实发生后执行无工具最终稿。`await`
返回底层 text/JSON，同时保留 exact Action provenance，供 `publish_home(action=...)`
校验。

`ask_human` 返回 opaque HumanMessage。它作为 Action 的唯一参数时，host 校验 exact
answered HumanRequest、Session、Agent、参数名和内容，再建立 direct human Turn。

## 并行

~~~rust
let reports = parallel for route in plan.routes key route.key {
    let worker = Researcher(key = route.key, name = route.name);
    await worker.research(question = ctx.request, objective = route.objective)
};
~~~

固定 `parallel` 返回以 branch name 为键的 object；`parallel for` 按输入顺序返回 list。
动态 key 必须是唯一 scalar。effect path 包含 IR NodeId、函数调用点、loop iteration、
branch index 与 canonical key hash，不受完成顺序影响。同一个 Agent 不能并行执行两个
Action，不同 Agent 可以。分支只通过返回值汇合，但仍共享 Workspace，不提供文件系统
隔离。

## Durable runtime

Session 冻结 source、source SHA、manifest、language version 与 canonical IR SHA。
恢复时从 root 重编译、重放；本地环境不持久化。每个 effect 的确定性 path、kind、
payload hash、status 和 result 存在 `session_effects`。同一路径换请求会 fail closed。

human/deadline 分支全部稳定后才聚合 suspension：human 优先，否则选择最早 deadline。
分支硬失败会取消并 join 兄弟分支。Workflow 的 `return` 直接成为 Session output，不存在
专用 completion effect。
