use async_trait::async_trait;
use futures::future::BoxFuture;
use futures::stream::{FuturesUnordered, StreamExt};
use papermachine_protocol::{
    AccessPreset, AgentId, ModelResponseFormat, ReasoningEffort, SessionId, WebSearchContextSize,
};
use papermachine_store::StoreHandle;
use serde_json::{Map, Number, Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::{Mutex, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::language::{
    ActionDefinition, AgentTemplate, AssignOperator, BinaryOperator, Block, CallArgument,
    Expression, ExpressionKind, FinalizePolicy, Function, MatchBody, Pattern, Program, SchemaKind,
    Statement, StatementKind, UnaryOperator, compile_source,
};
use crate::runtime::{SessionEffectContext, SessionExecutionError};
use crate::{SessionExecution, SessionExecutor};

const PURE_FUEL: u64 = 1_000_000;
const MAX_PARALLEL_BRANCHES: usize = 64;

#[derive(Clone)]
pub struct WorkflowInterpreter {
    store: StoreHandle,
    known_tools: BTreeSet<String>,
}

impl WorkflowInterpreter {
    pub fn new(store: StoreHandle, known_tools: impl IntoIterator<Item = String>) -> Self {
        Self {
            store,
            known_tools: known_tools.into_iter().collect(),
        }
    }

    async fn execute_inner(
        &self,
        session_id: SessionId,
        cancellation: CancellationToken,
    ) -> Result<RuntimeValue, SessionExecutionError> {
        let session = self
            .store
            .call(move |store| store.get_session(session_id))
            .await?;
        let source_sha256 = hex::encode(Sha256::digest(session.program.source_code.as_bytes()));
        if source_sha256 != session.program.sha256 {
            return Err(SessionExecutionError::Snapshot(
                "Workflow source hash does not match its durable snapshot".to_string(),
            ));
        }
        let compiled = compile_source(&session.program.source_code, &self.known_tools).map_err(
            |validation| {
                let message = validation
                    .diagnostics
                    .iter()
                    .map(|item| {
                        format!(
                            "{}:{}: {}",
                            item.line.unwrap_or(0),
                            item.column.unwrap_or(0),
                            item.message
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("; ");
                SessionExecutionError::Snapshot(format!(
                    "frozen Workflow source no longer compiles: {message}"
                ))
            },
        )?;
        if compiled.program.version != session.program.manifest.language_version {
            return Err(SessionExecutionError::Snapshot(
                "Workflow language version differs from the durable Session snapshot".to_string(),
            ));
        }
        if compiled.manifest != session.program.manifest {
            return Err(SessionExecutionError::Snapshot(
                "Workflow manifest differs from the durable Session snapshot".to_string(),
            ));
        }
        if compiled.ir_sha256 != session.program.ir_sha256 {
            return Err(SessionExecutionError::Snapshot(
                "canonical Workflow IR differs from the durable Session snapshot".to_string(),
            ));
        }

        let context = Arc::new(SessionEffectContext::new(
            self.store.clone(),
            session_id,
            cancellation.clone(),
        ));
        let runtime = Runtime {
            program: Arc::new(compiled.program),
            effects: context.clone(),
            cancellation,
            effect_permits: Arc::new(Semaphore::new(MAX_PARALLEL_BRANCHES)),
            active_agents: Arc::new(Mutex::new(HashSet::new())),
        };
        let mut env = Environment::default();
        env.declare(
            runtime.program.workflow.run_parameter.clone(),
            false,
            RuntimeValue::Context(Arc::new(ContextValue {
                session_id: session.id,
                request: session.request,
                instructions: session.instructions,
                params: RuntimeValue::from_json(session.params),
                trigger: RuntimeValue::from_json(serde_json::to_value(session.trigger)?),
            })),
        )?;
        let mut state = EvalState::new();
        match runtime
            .eval_block(&runtime.program.workflow.body, &mut env, &mut state)
            .await
        {
            Ok(Flow::Return(value) | Flow::Value(value)) => Ok(value),
            Ok(Flow::Next) => Ok(RuntimeValue::Null),
            Ok(Flow::Break | Flow::Continue) => Err(SessionExecutionError::Protocol(
                "compiler allowed loop control to escape workflow run".to_string(),
            )),
            Err(SessionExecutionError::Suspended(_)) => Err(SessionExecutionError::Suspended(
                context.aggregate_suspension().await?,
            )),
            Err(error) => Err(error),
        }
    }
}

#[async_trait]
impl SessionExecutor for WorkflowInterpreter {
    async fn execute(
        &self,
        session_id: SessionId,
        cancellation: CancellationToken,
    ) -> Result<SessionExecution, String> {
        match self.execute_inner(session_id, cancellation).await {
            Ok(value) => value
                .to_json()
                .map(SessionExecution::Completed)
                .map_err(|error| error.to_string()),
            Err(SessionExecutionError::Suspended(suspension)) => {
                Ok(SessionExecution::Suspended(suspension))
            }
            Err(error) => Err(error.to_string()),
        }
    }
}

#[derive(Clone)]
struct Runtime {
    program: Arc<Program>,
    effects: Arc<SessionEffectContext>,
    cancellation: CancellationToken,
    effect_permits: Arc<Semaphore>,
    active_agents: Arc<Mutex<HashSet<AgentId>>>,
}

#[derive(Clone, Debug)]
enum RuntimeValue {
    Null,
    Bool(bool),
    Int(i64),
    Number(f64),
    String(String),
    List(Arc<Vec<RuntimeValue>>),
    Object(Arc<BTreeMap<String, RuntimeValue>>),
    Context(Arc<ContextValue>),
    Project,
    Agent(Arc<AgentHandle>),
    ActionResult {
        value: Box<RuntimeValue>,
        invocation_id: String,
    },
    HumanMessage {
        value: Box<RuntimeValue>,
        request_id: String,
    },
    ArtifactRef(Arc<BTreeMap<String, RuntimeValue>>),
}

#[derive(Clone, Debug)]
struct ContextValue {
    session_id: SessionId,
    request: String,
    instructions: String,
    params: RuntimeValue,
    trigger: RuntimeValue,
}

#[derive(Clone, Debug)]
struct AgentHandle {
    template: String,
    identity_key: String,
    name: String,
    role: String,
    system: String,
    model: String,
    skills: Vec<String>,
    access: AccessPreset,
    current_access: Arc<std::sync::RwLock<AccessPreset>>,
    remote: Arc<std::sync::RwLock<Option<AgentId>>>,
}

impl RuntimeValue {
    fn from_json(value: Value) -> Self {
        match value {
            Value::Null => Self::Null,
            Value::Bool(value) => Self::Bool(value),
            Value::Number(value) if value.is_i64() => Self::Int(value.as_i64().unwrap_or_default()),
            Value::Number(value) if value.is_u64() => value.as_u64().map_or_else(
                || Self::Number(value.as_f64().unwrap_or_default()),
                |value| i64::try_from(value).map_or(Self::Number(value as f64), Self::Int),
            ),
            Value::Number(value) => Self::Number(value.as_f64().unwrap_or_default()),
            Value::String(value) => Self::String(value),
            Value::Array(values) => {
                Self::List(Arc::new(values.into_iter().map(Self::from_json).collect()))
            }
            Value::Object(values) => Self::Object(Arc::new(
                values
                    .into_iter()
                    .map(|(key, value)| (key, Self::from_json(value)))
                    .collect(),
            )),
        }
    }

    fn to_json(&self) -> Result<Value, SessionExecutionError> {
        match self.transparent() {
            Self::Null => Ok(Value::Null),
            Self::Bool(value) => Ok(Value::Bool(*value)),
            Self::Int(value) => Ok(Value::Number(Number::from(*value))),
            Self::Number(value) => Number::from_f64(*value)
                .map(Value::Number)
                .ok_or_else(|| runtime_error("non-finite number cannot cross a JSON boundary")),
            Self::String(value) => Ok(Value::String(value.clone())),
            Self::List(values) => values
                .iter()
                .map(Self::to_json)
                .collect::<Result<Vec<_>, _>>()
                .map(Value::Array),
            Self::Object(values) | Self::ArtifactRef(values) => values
                .iter()
                .map(|(key, value)| Ok((key.clone(), value.to_json()?)))
                .collect::<Result<Map<_, _>, SessionExecutionError>>()
                .map(Value::Object),
            Self::Context(_) | Self::Project | Self::Agent(_) => Err(runtime_error(
                "opaque Workflow value cannot cross a JSON boundary",
            )),
            Self::ActionResult { .. } | Self::HumanMessage { .. } => unreachable!("transparent"),
        }
    }

    fn transparent(&self) -> &Self {
        match self {
            Self::ActionResult { value, .. } | Self::HumanMessage { value, .. } => {
                value.transparent()
            }
            value => value,
        }
    }

    fn type_name(&self) -> &'static str {
        match self.transparent() {
            Self::Null => "null",
            Self::Bool(_) => "bool",
            Self::Int(_) => "int",
            Self::Number(_) => "number",
            Self::String(_) => "string",
            Self::List(_) => "list",
            Self::Object(_) => "object",
            Self::Context(_) => "context",
            Self::Project => "project",
            Self::Agent(_) => "AgentHandle",
            Self::ArtifactRef(_) => "ArtifactRef",
            Self::ActionResult { .. } | Self::HumanMessage { .. } => unreachable!("transparent"),
        }
    }
}

#[derive(Clone, Debug)]
struct Cell {
    mutable: bool,
    value: RuntimeValue,
}

#[derive(Clone, Debug, Default)]
struct Environment {
    scopes: Vec<HashMap<String, Cell>>,
}

impl Environment {
    fn push(&mut self) {
        self.scopes.push(HashMap::new());
    }

    fn pop(&mut self) {
        self.scopes.pop();
    }

    fn declare(
        &mut self,
        name: String,
        mutable: bool,
        value: RuntimeValue,
    ) -> Result<(), SessionExecutionError> {
        if self.scopes.is_empty() {
            self.push();
        }
        let scope = self.scopes.last_mut().expect("scope exists");
        if scope.contains_key(&name) {
            return Err(runtime_error(format!("binding `{name}` already exists")));
        }
        scope.insert(name, Cell { mutable, value });
        Ok(())
    }

    fn get(&self, name: &str) -> Result<RuntimeValue, SessionExecutionError> {
        self.scopes
            .iter()
            .rev()
            .find_map(|scope| scope.get(name))
            .map(|cell| cell.value.clone())
            .ok_or_else(|| runtime_error(format!("undefined binding `{name}`")))
    }

    fn assign(&mut self, name: &str, value: RuntimeValue) -> Result<(), SessionExecutionError> {
        let cell = self
            .scopes
            .iter_mut()
            .rev()
            .find_map(|scope| scope.get_mut(name))
            .ok_or_else(|| runtime_error(format!("undefined binding `{name}`")))?;
        if !cell.mutable {
            return Err(runtime_error(format!(
                "cannot rebind immutable `let` binding `{name}`"
            )));
        }
        cell.value = value;
        Ok(())
    }
}

#[derive(Clone, Debug)]
struct EvalState {
    fuel: u64,
    path: Vec<String>,
    durable_effects: u64,
}

impl EvalState {
    fn new() -> Self {
        Self {
            fuel: PURE_FUEL,
            path: vec!["root".to_string()],
            durable_effects: 0,
        }
    }

    fn step(&mut self, cancellation: &CancellationToken) -> Result<(), SessionExecutionError> {
        if cancellation.is_cancelled() {
            return Err(SessionExecutionError::Cancelled);
        }
        self.fuel = self.fuel.checked_sub(1).ok_or_else(|| {
            runtime_error(format!(
                "pure execution exceeded the public {PURE_FUEL} IR-step fuel between durable effects"
            ))
        })?;
        Ok(())
    }

    fn reset_fuel(&mut self) {
        self.fuel = PURE_FUEL;
    }

    fn effect_key(&self, node_id: u32, suffix: &str) -> String {
        let mut path = self.path.clone();
        path.push(format!("node:{node_id}"));
        path.push(suffix.to_string());
        path.join("/")
    }
}

#[derive(Debug)]
enum Flow {
    Next,
    Value(RuntimeValue),
    Return(RuntimeValue),
    Break,
    Continue,
}

impl Runtime {
    fn eval_block<'a>(
        &'a self,
        block: &'a Block,
        env: &'a mut Environment,
        state: &'a mut EvalState,
    ) -> BoxFuture<'a, Result<Flow, SessionExecutionError>> {
        Box::pin(async move {
            state.step(&self.cancellation)?;
            env.push();
            for statement in &block.statements {
                match self.eval_statement(statement, env, state).await? {
                    Flow::Next | Flow::Value(_) => {}
                    flow @ (Flow::Return(_) | Flow::Break | Flow::Continue) => {
                        env.pop();
                        return Ok(flow);
                    }
                }
            }
            let value = if let Some(tail) = &block.tail {
                Flow::Value(self.eval_expression(tail, env, state).await?)
            } else {
                Flow::Next
            };
            env.pop();
            Ok(value)
        })
    }

    fn eval_statement<'a>(
        &'a self,
        statement: &'a Statement,
        env: &'a mut Environment,
        state: &'a mut EvalState,
    ) -> BoxFuture<'a, Result<Flow, SessionExecutionError>> {
        Box::pin(async move {
            state.step(&self.cancellation)?;
            match &statement.kind {
                StatementKind::Let {
                    name,
                    mutable,
                    value,
                } => {
                    let value = self.eval_expression(value, env, state).await?;
                    env.declare(name.clone(), *mutable, value)?;
                    Ok(Flow::Next)
                }
                StatementKind::Assign {
                    name,
                    operator,
                    value,
                } => {
                    let right = self.eval_expression(value, env, state).await?;
                    let value = if *operator == AssignOperator::Set {
                        right
                    } else {
                        let left = env.get(name)?;
                        let binary = match operator {
                            AssignOperator::Add => BinaryOperator::Add,
                            AssignOperator::Subtract => BinaryOperator::Subtract,
                            AssignOperator::Multiply => BinaryOperator::Multiply,
                            AssignOperator::Divide => BinaryOperator::Divide,
                            AssignOperator::Set => unreachable!(),
                        };
                        apply_binary(binary, left, right)?
                    };
                    env.assign(name, value)?;
                    Ok(Flow::Next)
                }
                StatementKind::Expression { expression } => {
                    self.eval_expression(expression, env, state).await?;
                    Ok(Flow::Next)
                }
                StatementKind::If {
                    condition,
                    then_block,
                    else_block,
                } => {
                    if expect_bool(self.eval_expression(condition, env, state).await?, "if")? {
                        self.eval_block(then_block, env, state).await
                    } else if let Some(block) = else_block {
                        self.eval_block(block, env, state).await
                    } else {
                        Ok(Flow::Next)
                    }
                }
                StatementKind::While { condition, body } => {
                    let mut iteration = 0_u64;
                    loop {
                        state.step(&self.cancellation)?;
                        if !expect_bool(
                            self.eval_expression(condition, env, state).await?,
                            "while",
                        )? {
                            return Ok(Flow::Next);
                        }
                        state
                            .path
                            .push(format!("loop:{}:{iteration}", statement.id));
                        let flow = self.eval_block(body, env, state).await;
                        state.path.pop();
                        match flow? {
                            Flow::Return(value) => return Ok(Flow::Return(value)),
                            Flow::Break => return Ok(Flow::Next),
                            Flow::Next | Flow::Value(_) | Flow::Continue => {}
                        }
                        iteration = iteration
                            .checked_add(1)
                            .ok_or_else(|| runtime_error("loop iteration counter overflow"))?;
                    }
                }
                StatementKind::Loop { body } => {
                    let mut iteration = 0_u64;
                    loop {
                        state.step(&self.cancellation)?;
                        state
                            .path
                            .push(format!("loop:{}:{iteration}", statement.id));
                        let flow = self.eval_block(body, env, state).await;
                        state.path.pop();
                        match flow? {
                            Flow::Return(value) => return Ok(Flow::Return(value)),
                            Flow::Break => return Ok(Flow::Next),
                            Flow::Next | Flow::Value(_) | Flow::Continue => {}
                        }
                        iteration = iteration
                            .checked_add(1)
                            .ok_or_else(|| runtime_error("loop iteration counter overflow"))?;
                    }
                }
                StatementKind::For {
                    binding,
                    iterable,
                    body,
                } => {
                    let values = finite_collection(
                        self.eval_expression(iterable, env, state).await?,
                        "for",
                    )?;
                    for (iteration, value) in values.into_iter().enumerate() {
                        state.step(&self.cancellation)?;
                        env.push();
                        env.declare(binding.clone(), false, value)?;
                        state.path.push(format!("for:{}:{iteration}", statement.id));
                        let flow = self.eval_block(body, env, state).await;
                        state.path.pop();
                        env.pop();
                        match flow? {
                            Flow::Return(value) => return Ok(Flow::Return(value)),
                            Flow::Break => break,
                            Flow::Next | Flow::Value(_) | Flow::Continue => {}
                        }
                    }
                    Ok(Flow::Next)
                }
                StatementKind::Match { value, arms } => {
                    let value = self.eval_expression(value, env, state).await?;
                    for arm in arms {
                        if arm.patterns.iter().any(|pattern| match pattern {
                            Pattern::Wildcard => true,
                            Pattern::Literal(pattern) => {
                                runtime_equal(&value, &RuntimeValue::from_json(pattern.clone()))
                                    .unwrap_or(false)
                            }
                        }) {
                            return match &arm.body {
                                MatchBody::Block(block) => self.eval_block(block, env, state).await,
                                MatchBody::Statement(statement) => {
                                    self.eval_statement(statement, env, state).await
                                }
                                MatchBody::Expression(expression) => {
                                    self.eval_expression(expression, env, state).await?;
                                    Ok(Flow::Next)
                                }
                            };
                        }
                    }
                    Err(runtime_error("dynamic match value had no matching arm"))
                }
                StatementKind::Break => Ok(Flow::Break),
                StatementKind::Continue => Ok(Flow::Continue),
                StatementKind::Return { value } => {
                    Ok(Flow::Return(self.eval_expression(value, env, state).await?))
                }
            }
        })
    }

    fn eval_expression<'a>(
        &'a self,
        expression: &'a Expression,
        env: &'a mut Environment,
        state: &'a mut EvalState,
    ) -> BoxFuture<'a, Result<RuntimeValue, SessionExecutionError>> {
        Box::pin(async move {
            state.step(&self.cancellation)?;
            match &expression.kind {
                ExpressionKind::Literal { value } => Ok(RuntimeValue::from_json(value.clone())),
                ExpressionKind::Variable { name } => env.get(name),
                ExpressionKind::List { values } => {
                    let mut output = Vec::with_capacity(values.len());
                    for value in values {
                        output.push(self.eval_expression(value, env, state).await?);
                    }
                    Ok(RuntimeValue::List(Arc::new(output)))
                }
                ExpressionKind::Object { fields } => {
                    let mut output = BTreeMap::new();
                    for (name, value) in fields {
                        output.insert(name.clone(), self.eval_expression(value, env, state).await?);
                    }
                    Ok(RuntimeValue::Object(Arc::new(output)))
                }
                ExpressionKind::Unary { operator, value } => {
                    let value = self.eval_expression(value, env, state).await?;
                    match operator {
                        UnaryOperator::Not => Ok(RuntimeValue::Bool(!expect_bool(value, "!")?)),
                        UnaryOperator::Negate => match value.transparent() {
                            RuntimeValue::Int(value) => value
                                .checked_neg()
                                .map(RuntimeValue::Int)
                                .ok_or_else(|| runtime_error("integer negation overflow")),
                            RuntimeValue::Number(value) => Ok(RuntimeValue::Number(-value)),
                            _ => Err(type_error("unary -", "number", &value)),
                        },
                    }
                }
                ExpressionKind::Binary {
                    operator,
                    left,
                    right,
                } => {
                    if *operator == BinaryOperator::And {
                        let left =
                            expect_bool(self.eval_expression(left, env, state).await?, "&&")?;
                        return if left {
                            Ok(RuntimeValue::Bool(expect_bool(
                                self.eval_expression(right, env, state).await?,
                                "&&",
                            )?))
                        } else {
                            Ok(RuntimeValue::Bool(false))
                        };
                    }
                    if *operator == BinaryOperator::Or {
                        let left =
                            expect_bool(self.eval_expression(left, env, state).await?, "||")?;
                        return if left {
                            Ok(RuntimeValue::Bool(true))
                        } else {
                            Ok(RuntimeValue::Bool(expect_bool(
                                self.eval_expression(right, env, state).await?,
                                "||",
                            )?))
                        };
                    }
                    let left = self.eval_expression(left, env, state).await?;
                    if *operator == BinaryOperator::Coalesce
                        && !matches!(left.transparent(), RuntimeValue::Null)
                    {
                        return Ok(left);
                    }
                    let right = self.eval_expression(right, env, state).await?;
                    apply_binary(*operator, left, right)
                }
                ExpressionKind::Member { value, name } => {
                    let value = self.eval_expression(value, env, state).await?;
                    member(value, name)
                }
                ExpressionKind::Index { value, index } => {
                    let value = self.eval_expression(value, env, state).await?;
                    let index = self.eval_expression(index, env, state).await?;
                    index_value(value, index)
                }
                ExpressionKind::Call { callee, arguments } => {
                    self.eval_call(expression, callee, arguments, env, state)
                        .await
                }
                ExpressionKind::Await { value } => self.eval_expression(value, env, state).await,
                ExpressionKind::Parallel { branches } => {
                    self.eval_parallel(expression, branches, env, state).await
                }
                ExpressionKind::ParallelFor {
                    binding,
                    iterable,
                    key,
                    body,
                } => {
                    self.eval_parallel_for(expression, binding, iterable, key, body, env, state)
                        .await
                }
            }
        })
    }

    fn eval_call<'a>(
        &'a self,
        expression: &'a Expression,
        callee: &'a Expression,
        arguments: &'a [CallArgument],
        env: &'a mut Environment,
        state: &'a mut EvalState,
    ) -> BoxFuture<'a, Result<RuntimeValue, SessionExecutionError>> {
        Box::pin(async move {
            let values = self.eval_arguments(arguments, env, state).await?;
            match &callee.kind {
                ExpressionKind::Variable { name } if self.program.agents.contains_key(name) => {
                    self.construct_agent(&self.program.agents[name], values)
                }
                ExpressionKind::Variable { name } if self.program.functions.contains_key(name) => {
                    let function = self.program.functions[name].clone();
                    state.path.push(format!("call:{}:{name}", expression.id));
                    let result = self.call_function(&function, values, state).await;
                    state.path.pop();
                    result
                }
                ExpressionKind::Variable { name } if is_pure_builtin(name) => {
                    eval_pure_builtin(name, values, state, &self.cancellation)
                }
                ExpressionKind::Variable { name } => {
                    self.eval_effect_builtin(expression.id, name, values, state)
                        .await
                }
                ExpressionKind::Member { value, name } => {
                    let receiver = self.eval_expression(value, env, state).await?;
                    match receiver.transparent() {
                        RuntimeValue::Agent(agent) if name == "set_access" => {
                            self.set_agent_access(expression.id, agent.clone(), values, state)
                                .await
                        }
                        RuntimeValue::Agent(agent) => {
                            self.run_action(expression.id, agent.clone(), name, values, state)
                                .await
                        }
                        RuntimeValue::Project if name == "changes" => {
                            self.project_changes(expression.id, values, state).await
                        }
                        _ => Err(runtime_error(format!(
                            "{} has no callable member `{name}`",
                            receiver.type_name()
                        ))),
                    }
                }
                _ => Err(runtime_error(
                    "higher-order and dynamically selected calls are not supported",
                )),
            }
        })
    }

    async fn eval_arguments(
        &self,
        arguments: &[CallArgument],
        env: &mut Environment,
        state: &mut EvalState,
    ) -> Result<Vec<(Option<String>, RuntimeValue)>, SessionExecutionError> {
        let mut output = Vec::with_capacity(arguments.len());
        for argument in arguments {
            output.push((
                argument.name.clone(),
                self.eval_expression(&argument.value, env, state).await?,
            ));
        }
        Ok(output)
    }

    fn construct_agent(
        &self,
        template: &AgentTemplate,
        values: Vec<(Option<String>, RuntimeValue)>,
    ) -> Result<RuntimeValue, SessionExecutionError> {
        let options = named_values(values, "Agent constructor")?;
        let identity = options
            .get("key")
            .map(canonical_scalar_key)
            .transpose()?
            .unwrap_or_else(|| "string:main".to_string());
        let access = options
            .get("access")
            .map(access_value)
            .transpose()?
            .unwrap_or(template.access);
        let handle = AgentHandle {
            template: template.name.clone(),
            identity_key: identity,
            name: option_string(&options, "name")?.unwrap_or_else(|| template.name.clone()),
            role: option_string(&options, "role")?.unwrap_or_else(|| template.role.clone()),
            system: option_string(&options, "system")?.unwrap_or_else(|| template.system.clone()),
            model: option_string(&options, "model")?.unwrap_or_else(|| template.model.clone()),
            skills: option_string_list(&options, "skills")?
                .unwrap_or_else(|| template.skills.clone()),
            access,
            current_access: Arc::new(std::sync::RwLock::new(access)),
            remote: Arc::new(std::sync::RwLock::new(None)),
        };
        Ok(RuntimeValue::Agent(Arc::new(handle)))
    }

    async fn call_function(
        &self,
        function: &Function,
        values: Vec<(Option<String>, RuntimeValue)>,
        state: &mut EvalState,
    ) -> Result<RuntimeValue, SessionExecutionError> {
        let values = bind_arguments(&function.name, &function.parameters, values)?;
        let mut env = Environment::default();
        env.push();
        for (name, value) in function.parameters.iter().zip(values) {
            env.declare(name.clone(), false, value)?;
        }
        match self.eval_block(&function.body, &mut env, state).await? {
            Flow::Return(value) | Flow::Value(value) => Ok(value),
            Flow::Next => Ok(RuntimeValue::Null),
            Flow::Break | Flow::Continue => {
                Err(runtime_error("loop control escaped a local function"))
            }
        }
    }

    async fn ensure_agent(
        &self,
        agent: &AgentHandle,
        state: &mut EvalState,
    ) -> Result<AgentId, SessionExecutionError> {
        if let Some(agent_id) = *agent
            .remote
            .read()
            .map_err(|_| runtime_error("Agent identity lock is poisoned"))?
        {
            return Ok(agent_id);
        }
        let identity_hash = hex::encode(Sha256::digest(
            format!("{}\0{}", agent.template, agent.identity_key).as_bytes(),
        ));
        let result = self
            .effect(
                format!("root/agent:{identity_hash}/create_agent"),
                "create_agent",
                json!({
                    "class_name": agent.template,
                    "identity_key": agent.identity_key,
                    "name": agent.name,
                    "role": agent.role,
                    "system_prompt": agent.system,
                    "model": agent.model,
                    "skills": agent.skills,
                    "access": agent.access,
                }),
                state,
            )
            .await?;
        let id = result
            .get("agent_id")
            .and_then(Value::as_str)
            .ok_or_else(|| runtime_error("create_agent returned no Agent id"))?;
        let agent_id = AgentId::from_str(id).map_err(|error| runtime_error(error.to_string()))?;
        if let Some(access) = result.get("access").and_then(Value::as_str) {
            *agent
                .current_access
                .write()
                .map_err(|_| runtime_error("Agent access lock is poisoned"))? =
                access_value(&RuntimeValue::String(access.to_string()))?;
        }
        *agent
            .remote
            .write()
            .map_err(|_| runtime_error("Agent identity lock is poisoned"))? = Some(agent_id);
        Ok(agent_id)
    }

    async fn set_agent_access(
        &self,
        node_id: u32,
        agent: Arc<AgentHandle>,
        values: Vec<(Option<String>, RuntimeValue)>,
        state: &mut EvalState,
    ) -> Result<RuntimeValue, SessionExecutionError> {
        let values = bind_arguments("set_access", &["access".to_string()], values)?;
        let access = access_value(&values[0])?;
        let agent_id = self.ensure_agent(&agent, state).await?;
        let result = self
            .effect(
                state.effect_key(node_id, "set_agent_access"),
                "set_agent_access",
                json!({"agent_id": agent_id, "access": access}),
                state,
            )
            .await?;
        *agent
            .current_access
            .write()
            .map_err(|_| runtime_error("Agent access lock is poisoned"))? = access;
        Ok(RuntimeValue::from_json(result))
    }

    async fn run_action(
        &self,
        node_id: u32,
        agent: Arc<AgentHandle>,
        action_name: &str,
        values: Vec<(Option<String>, RuntimeValue)>,
        state: &mut EvalState,
    ) -> Result<RuntimeValue, SessionExecutionError> {
        let action = self.program.agents[&agent.template]
            .actions
            .get(action_name)
            .cloned()
            .ok_or_else(|| {
                runtime_error(format!(
                    "Agent template {} has no Action `{action_name}`",
                    agent.template
                ))
            })?;
        let values = bind_arguments(action_name, &action.parameters, values)?;
        let mut arguments = Map::new();
        let mut human = None;
        for (name, value) in action.parameters.iter().zip(values) {
            if let RuntimeValue::HumanMessage { request_id, .. } = &value {
                if human.is_some() || action.parameters.len() != 1 {
                    return Err(runtime_error(
                        "HumanMessage must be the Action's only argument",
                    ));
                }
                human = Some((name.clone(), request_id.clone()));
            }
            arguments.insert(name.clone(), value.to_json()?);
        }
        let agent_id = self.ensure_agent(&agent, state).await?;
        let _active = ActiveAgentGuard::acquire(self.active_agents.clone(), agent_id).await?;
        let base_prompt = action_prompt(&action);
        let mut invocation = self
            .invoke_action(
                node_id,
                "work",
                agent_id,
                action_name,
                &base_prompt,
                Value::Object(arguments),
                if action.finalize == FinalizePolicy::IfNeeded {
                    None
                } else {
                    action
                        .result
                        .as_ref()
                        .map(|schema| response_format(action_name, schema))
                },
                action.tools.clone(),
                action.search_context,
                action.reasoning_effort,
                human,
                state,
            )
            .await?;
        let mut output = invocation.output.clone();

        if action.finalize == FinalizePolicy::IfNeeded {
            if let Some(schema) = &action.result
                && parse_and_validate_action_json(&output, schema).is_err()
            {
                invocation = self
                    .invoke_action(
                        node_id,
                        "finalize",
                        agent_id,
                        &format!("{action_name}_finalize"),
                        &structured_finalizer_prompt(schema),
                        json!({"original_action": action_name, "finalization_policy": "if_needed"}),
                        Some(response_format(action_name, schema)),
                        Some(Vec::new()),
                        None,
                        action.reasoning_effort,
                        None,
                        state,
                    )
                    .await?;
                output = invocation.output.clone();
            }
        } else if action.finalize == FinalizePolicy::AfterSearch
            && invocation.hosted_search_calls > 0
        {
            invocation = self
                .invoke_action(
                    node_id,
                    "finalize",
                    agent_id,
                    &format!("{action_name}_finalize"),
                    &deliverable_finalizer_prompt(action.result.as_ref()),
                    json!({"original_action": action_name, "finalization_policy": "after_search"}),
                    action
                        .result
                        .as_ref()
                        .map(|schema| response_format(action_name, schema)),
                    Some(Vec::new()),
                    None,
                    action.reasoning_effort,
                    None,
                    state,
                )
                .await?;
            output = invocation.output.clone();
        }

        let value = if let Some(schema) = &action.result {
            let mut parsed = parse_and_validate_action_json(&output, schema);
            for attempt in 1..=2 {
                if parsed.is_ok() {
                    break;
                }
                let error = parsed.as_ref().expect_err("checked").clone();
                invocation = self
                    .invoke_action(
                        node_id,
                        &format!("repair:{attempt}"),
                        agent_id,
                        &format!("{action_name}_json_repair"),
                        &repair_prompt(schema),
                        json!({
                            "original_action": action_name,
                            "expected_schema": schema.to_json_schema(),
                            "parser_error": error,
                            "repair_attempt": attempt,
                        }),
                        Some(response_format(action_name, schema)),
                        Some(Vec::new()),
                        None,
                        Some(ReasoningEffort::Low),
                        None,
                        state,
                    )
                    .await?;
                output = invocation.output.clone();
                parsed = parse_and_validate_action_json(&output, schema);
            }
            RuntimeValue::from_json(parsed.map_err(|error| {
                runtime_error(format!(
                    "Action {}.{action_name} returned invalid structured output: {error}",
                    agent.template
                ))
            })?)
        } else {
            RuntimeValue::String(output)
        };
        Ok(RuntimeValue::ActionResult {
            value: Box::new(value),
            invocation_id: invocation.id,
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn invoke_action(
        &self,
        node_id: u32,
        suffix: &str,
        agent_id: AgentId,
        action_name: &str,
        prompt: &str,
        arguments: Value,
        response_format: Option<ModelResponseFormat>,
        tools: Option<Vec<String>>,
        search_context: Option<WebSearchContextSize>,
        reasoning: Option<ReasoningEffort>,
        human: Option<(String, String)>,
        state: &mut EvalState,
    ) -> Result<ActionInvocationResult, SessionExecutionError> {
        let (human_argument, human_request_id) = human.unzip();
        let result = self
            .effect(
                state.effect_key(node_id, &format!("invoke_action:{suffix}")),
                "invoke_action",
                json!({
                    "agent_id": agent_id,
                    "action_name": action_name,
                    "prompt": prompt,
                    "arguments": arguments,
                    "response_format": response_format,
                    "tool_policy": tools,
                    "web_search_context_size": search_context,
                    "reasoning_effort": reasoning,
                    "human_request_id": human_request_id,
                    "human_message_argument": human_argument,
                }),
                state,
            )
            .await?;
        Ok(ActionInvocationResult {
            id: expect_json_string(&result, "action_invocation_id")?.to_string(),
            output: result
                .get("output")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            hosted_search_calls: result
                .get("hosted_search_calls_used")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        })
    }

    async fn eval_effect_builtin(
        &self,
        node_id: u32,
        name: &str,
        values: Vec<(Option<String>, RuntimeValue)>,
        state: &mut EvalState,
    ) -> Result<RuntimeValue, SessionExecutionError> {
        match name {
            "ask_human" => {
                let values = flexible_arguments(
                    "ask_human",
                    &["question", "response_schema", "agent"],
                    values,
                )?;
                let question = required_string(&values, "question")?;
                let response_schema = values
                    .get("response_schema")
                    .map(RuntimeValue::to_json)
                    .transpose()?
                    .unwrap_or_else(|| json!({"type":"string"}));
                crate::language::validate_json_schema_definition(
                    &response_schema,
                    "HumanRequest response schema",
                )
                .map_err(runtime_error)?;
                let agent_id = match values.get("agent") {
                    Some(RuntimeValue::Agent(agent)) => {
                        Some(self.ensure_agent(agent, state).await?)
                    }
                    Some(value) => return Err(type_error("ask_human agent", "AgentHandle", value)),
                    None => None,
                };
                let result = self
                    .effect(
                        state.effect_key(node_id, "ask_human"),
                        "ask_human",
                        json!({
                            "question": question,
                            "response_schema": response_schema,
                            "agent_id": agent_id,
                        }),
                        state,
                    )
                    .await?;
                let request_id = expect_json_string(&result, "human_request_id")?.to_string();
                let answer = result.get("answer").cloned().unwrap_or(Value::Null);
                Ok(RuntimeValue::HumanMessage {
                    value: Box::new(RuntimeValue::from_json(answer)),
                    request_id,
                })
            }
            "wait" => {
                let values = flexible_arguments("wait", &["seconds", "minutes", "name"], values)?;
                let seconds = values.get("seconds").map(expect_number_ref).transpose()?;
                let minutes = values.get("minutes").map(expect_number_ref).transpose()?;
                if seconds.is_some() == minutes.is_some() {
                    return Err(runtime_error(
                        "wait requires exactly one of `seconds` or `minutes`",
                    ));
                }
                let milliseconds =
                    (seconds.unwrap_or(0.0) * 1_000.0 + minutes.unwrap_or(0.0) * 60_000.0).round();
                if !milliseconds.is_finite()
                    || milliseconds <= 0.0
                    || milliseconds > u64::MAX as f64
                {
                    return Err(runtime_error(
                        "wait duration must be a positive finite duration",
                    ));
                }
                let result = self
                    .effect(
                        state.effect_key(node_id, "wait"),
                        "wait",
                        json!({"interval_ms": milliseconds as u64}),
                        state,
                    )
                    .await?;
                Ok(RuntimeValue::from_json(result))
            }
            "publish_artifact" => {
                let values = flexible_arguments(
                    "publish_artifact",
                    &["name", "content", "kind", "media_type", "metadata", "agent"],
                    values,
                )?;
                let agent_id = match values.get("agent") {
                    Some(RuntimeValue::Agent(agent)) => {
                        Some(self.ensure_agent(agent, state).await?)
                    }
                    Some(value) => {
                        return Err(type_error("publish_artifact agent", "AgentHandle", value));
                    }
                    None => None,
                };
                let result = self
                    .effect(
                        state.effect_key(node_id, "publish_artifact"),
                        "publish_artifact",
                        json!({
                            "name": required_string(&values, "name")?,
                            "content": required_string(&values, "content")?,
                            "kind": optional_string_map(&values, "kind")?.unwrap_or_else(|| "other".to_string()),
                            "media_type": optional_string_map(&values, "media_type")?.unwrap_or_else(|| "text/plain; charset=utf-8".to_string()),
                            "metadata": values.get("metadata").map(RuntimeValue::to_json).transpose()?.unwrap_or_else(|| json!({})),
                            "agent_id": agent_id,
                        }),
                        state,
                    )
                    .await?;
                Ok(RuntimeValue::ArtifactRef(Arc::new(artifact_from_json(
                    result,
                )?)))
            }
            "publish_home" => {
                let values = flexible_arguments("publish_home", &["action", "metadata"], values)?;
                let action = values
                    .get("action")
                    .ok_or_else(|| runtime_error("publish_home requires `action`"))?;
                let RuntimeValue::ActionResult { invocation_id, .. } = action else {
                    return Err(type_error("publish_home action", "ActionHandle", action));
                };
                let result = self
                    .effect(
                        state.effect_key(node_id, "publish_project_home"),
                        "publish_project_home",
                        json!({
                            "action_invocation_id": invocation_id,
                            "metadata": values.get("metadata").map(RuntimeValue::to_json).transpose()?.unwrap_or_else(|| json!({})),
                        }),
                        state,
                    )
                    .await?;
                Ok(RuntimeValue::ArtifactRef(Arc::new(artifact_from_json(
                    result,
                )?)))
            }
            other => Err(runtime_error(format!("unknown effect builtin `{other}`"))),
        }
    }

    async fn project_changes(
        &self,
        node_id: u32,
        values: Vec<(Option<String>, RuntimeValue)>,
        state: &mut EvalState,
    ) -> Result<RuntimeValue, SessionExecutionError> {
        let values = flexible_arguments(
            "project.changes",
            &["after_cursor", "exclude_current_program"],
            values,
        )?;
        let after_cursor = optional_string_map(&values, "after_cursor")?;
        let exclude = match values.get("exclude_current_program") {
            Some(value) => expect_bool(value.clone(), "exclude_current_program")?,
            None => false,
        };
        let result = self
            .effect(
                state.effect_key(node_id, "project_changes"),
                "project_changes",
                json!({
                    "after_cursor": after_cursor,
                    "exclude_current_program": exclude,
                }),
                state,
            )
            .await?;
        Ok(RuntimeValue::from_json(result))
    }

    async fn effect(
        &self,
        key: String,
        kind: &str,
        payload: Value,
        state: &mut EvalState,
    ) -> Result<Value, SessionExecutionError> {
        let permit = tokio::select! {
            permit = self.effect_permits.clone().acquire_owned() => {
                permit.map_err(|_| runtime_error("effect concurrency limiter closed"))?
            }
            _ = self.cancellation.cancelled() => return Err(SessionExecutionError::Cancelled),
        };
        let result = self.effects.handle(key, kind, payload).await;
        drop(permit);
        state.durable_effects = state.durable_effects.saturating_add(1);
        state.reset_fuel();
        result
    }

    async fn eval_parallel(
        &self,
        expression: &Expression,
        branches: &[crate::language::ParallelBranch],
        env: &Environment,
        state: &mut EvalState,
    ) -> Result<RuntimeValue, SessionExecutionError> {
        if branches.len() > MAX_PARALLEL_BRANCHES {
            return Err(runtime_error(format!(
                "parallel has {} branches; maximum is {MAX_PARALLEL_BRANCHES}",
                branches.len()
            )));
        }
        let mut futures = FuturesUnordered::new();
        for (index, branch) in branches.iter().cloned().enumerate() {
            let runtime = self.clone();
            let mut branch_env = env.clone();
            let mut branch_state = state.clone();
            branch_state.path.push(format!(
                "parallel:{}:{}:{}",
                expression.id,
                index,
                short_hash(&branch.name)
            ));
            futures.push(async move {
                let result = runtime
                    .eval_block(&branch.body, &mut branch_env, &mut branch_state)
                    .await;
                (index, branch.name, result, branch_state)
            });
        }
        let mut results = vec![None; branches.len()];
        let mut suspended = None;
        let mut branch_states = Vec::with_capacity(branches.len());
        while let Some((index, name, result, branch_state)) = futures.next().await {
            branch_states.push(branch_state);
            match result {
                Ok(flow) => {
                    results[index] = Some((name, flow_value(flow)?));
                }
                Err(SessionExecutionError::Suspended(value)) => suspended = Some(value),
                Err(error) => return Err(error),
            }
        }
        if let Some(suspension) = suspended {
            return Err(SessionExecutionError::Suspended(suspension));
        }
        merge_parallel_fuel(state, &branch_states);
        Ok(RuntimeValue::Object(Arc::new(
            results
                .into_iter()
                .map(|value| {
                    value.ok_or_else(|| runtime_error("parallel branch produced no result"))
                })
                .collect::<Result<BTreeMap<_, _>, _>>()?,
        )))
    }

    #[allow(clippy::too_many_arguments)]
    async fn eval_parallel_for(
        &self,
        expression: &Expression,
        binding: &str,
        iterable: &Expression,
        key: &Expression,
        body: &Block,
        env: &Environment,
        state: &mut EvalState,
    ) -> Result<RuntimeValue, SessionExecutionError> {
        let mut source_env = env.clone();
        let values = finite_collection(
            self.eval_expression(iterable, &mut source_env, state)
                .await?,
            "parallel for",
        )?;
        if values.len() > MAX_PARALLEL_BRANCHES {
            return Err(runtime_error(format!(
                "parallel for has {} branches; maximum is {MAX_PARALLEL_BRANCHES}",
                values.len()
            )));
        }
        let mut keyed = Vec::with_capacity(values.len());
        let mut seen = HashSet::new();
        for value in values {
            let mut key_env = env.clone();
            key_env.push();
            key_env.declare(binding.to_string(), false, value.clone())?;
            let key_value = self.eval_expression(key, &mut key_env, state).await?;
            let canonical = canonical_scalar_key(&key_value)?;
            if !seen.insert(canonical.clone()) {
                return Err(runtime_error(format!(
                    "parallel for key is not unique: {canonical}"
                )));
            }
            keyed.push((value, canonical));
        }
        let mut futures = FuturesUnordered::new();
        for (index, (value, canonical)) in keyed.into_iter().enumerate() {
            let runtime = self.clone();
            let mut branch_env = env.clone();
            branch_env.push();
            branch_env.declare(binding.to_string(), false, value)?;
            let mut branch_state = state.clone();
            branch_state.path.push(format!(
                "parallel_for:{}:{}:{}",
                expression.id,
                index,
                short_hash(&canonical)
            ));
            let branch = body.clone();
            futures.push(async move {
                let result = runtime
                    .eval_block(&branch, &mut branch_env, &mut branch_state)
                    .await;
                (index, result, branch_state)
            });
        }
        let mut results = vec![None; values_len_hint(&futures)];
        let mut suspended = None;
        let mut branch_states = Vec::with_capacity(results.len());
        while let Some((index, result, branch_state)) = futures.next().await {
            branch_states.push(branch_state);
            match result {
                Ok(flow) => results[index] = Some(flow_value(flow)?),
                Err(SessionExecutionError::Suspended(value)) => suspended = Some(value),
                Err(error) => return Err(error),
            }
        }
        if let Some(suspension) = suspended {
            return Err(SessionExecutionError::Suspended(suspension));
        }
        merge_parallel_fuel(state, &branch_states);
        Ok(RuntimeValue::List(Arc::new(
            results
                .into_iter()
                .map(|value| {
                    value.ok_or_else(|| runtime_error("parallel branch produced no result"))
                })
                .collect::<Result<Vec<_>, _>>()?,
        )))
    }
}

fn values_len_hint<T>(futures: &FuturesUnordered<T>) -> usize {
    futures.len()
}

fn merge_parallel_fuel(parent: &mut EvalState, branches: &[EvalState]) {
    let maximum_effects = branches
        .iter()
        .map(|branch| branch.durable_effects)
        .max()
        .unwrap_or(parent.durable_effects);
    if maximum_effects > parent.durable_effects {
        parent.durable_effects = maximum_effects;
        parent.reset_fuel();
    } else if let Some(minimum_fuel) = branches.iter().map(|branch| branch.fuel).min() {
        parent.fuel = parent.fuel.min(minimum_fuel);
    }
}

struct ActiveAgentGuard {
    active: Arc<Mutex<HashSet<AgentId>>>,
    agent_id: AgentId,
}

impl ActiveAgentGuard {
    async fn acquire(
        active: Arc<Mutex<HashSet<AgentId>>>,
        agent_id: AgentId,
    ) -> Result<Self, SessionExecutionError> {
        if !active.lock().await.insert(agent_id) {
            return Err(runtime_error(
                "the same Agent cannot run two Actions concurrently",
            ));
        }
        Ok(Self { active, agent_id })
    }
}

impl Drop for ActiveAgentGuard {
    fn drop(&mut self) {
        if let Ok(mut active) = self.active.try_lock() {
            active.remove(&self.agent_id);
        } else {
            let active = self.active.clone();
            let agent_id = self.agent_id;
            tokio::spawn(async move {
                active.lock().await.remove(&agent_id);
            });
        }
    }
}

#[derive(Debug)]
struct ActionInvocationResult {
    id: String,
    output: String,
    hosted_search_calls: u64,
}

fn flow_value(flow: Flow) -> Result<RuntimeValue, SessionExecutionError> {
    match flow {
        Flow::Value(value) | Flow::Return(value) => Ok(value),
        Flow::Next => Ok(RuntimeValue::Null),
        Flow::Break | Flow::Continue => Err(runtime_error(
            "parallel branch cannot return loop control to its parent",
        )),
    }
}

fn member(value: RuntimeValue, name: &str) -> Result<RuntimeValue, SessionExecutionError> {
    match value.transparent() {
        RuntimeValue::Object(values) | RuntimeValue::ArtifactRef(values) => values
            .get(name)
            .cloned()
            .ok_or_else(|| runtime_error(format!("object has no field `{name}`"))),
        RuntimeValue::Context(context) => match name {
            "request" => Ok(RuntimeValue::String(context.request.clone())),
            "instructions" => Ok(RuntimeValue::String(context.instructions.clone())),
            "params" => Ok(context.params.clone()),
            "trigger" => Ok(context.trigger.clone()),
            "session_id" => Ok(RuntimeValue::String(context.session_id.to_string())),
            "project" => Ok(RuntimeValue::Project),
            _ => Err(runtime_error(format!("context has no field `{name}`"))),
        },
        RuntimeValue::Agent(agent) if name == "access" => {
            let access = *agent
                .current_access
                .read()
                .map_err(|_| runtime_error("Agent access lock is poisoned"))?;
            Ok(RuntimeValue::String(access.as_str().to_string()))
        }
        _ => Err(runtime_error(format!(
            "{} has no field `{name}`",
            value.type_name()
        ))),
    }
}

fn index_value(
    value: RuntimeValue,
    index: RuntimeValue,
) -> Result<RuntimeValue, SessionExecutionError> {
    match (value.transparent(), index.transparent()) {
        (RuntimeValue::List(values), RuntimeValue::Int(index)) => {
            let index = usize::try_from(*index)
                .map_err(|_| runtime_error("list index must be non-negative"))?;
            values
                .get(index)
                .cloned()
                .ok_or_else(|| runtime_error(format!("list index {index} is out of bounds")))
        }
        (RuntimeValue::Object(values), RuntimeValue::String(index))
        | (RuntimeValue::ArtifactRef(values), RuntimeValue::String(index)) => values
            .get(index)
            .cloned()
            .ok_or_else(|| runtime_error(format!("object has no field `{index}`"))),
        _ => Err(runtime_error(format!(
            "cannot index {} with {}",
            value.type_name(),
            index.type_name()
        ))),
    }
}

fn apply_binary(
    operator: BinaryOperator,
    left: RuntimeValue,
    right: RuntimeValue,
) -> Result<RuntimeValue, SessionExecutionError> {
    if matches!(operator, BinaryOperator::Equal | BinaryOperator::NotEqual) {
        let equal = runtime_equal(&left, &right)?;
        return Ok(RuntimeValue::Bool(if operator == BinaryOperator::Equal {
            equal
        } else {
            !equal
        }));
    }
    if operator == BinaryOperator::Coalesce {
        return Ok(right);
    }
    match (left.transparent(), right.transparent()) {
        (RuntimeValue::Int(left), RuntimeValue::Int(right)) => match operator {
            BinaryOperator::Add => left
                .checked_add(*right)
                .map(RuntimeValue::Int)
                .ok_or_else(|| runtime_error("integer addition overflow")),
            BinaryOperator::Subtract => left
                .checked_sub(*right)
                .map(RuntimeValue::Int)
                .ok_or_else(|| runtime_error("integer subtraction overflow")),
            BinaryOperator::Multiply => left
                .checked_mul(*right)
                .map(RuntimeValue::Int)
                .ok_or_else(|| runtime_error("integer multiplication overflow")),
            BinaryOperator::Divide => {
                if *right == 0 {
                    Err(runtime_error("division by zero"))
                } else if left % right == 0 {
                    Ok(RuntimeValue::Int(left / right))
                } else {
                    Ok(RuntimeValue::Number(*left as f64 / *right as f64))
                }
            }
            BinaryOperator::Remainder => {
                if *right == 0 {
                    Err(runtime_error("remainder by zero"))
                } else {
                    Ok(RuntimeValue::Int(left % right))
                }
            }
            BinaryOperator::Less => Ok(RuntimeValue::Bool(left < right)),
            BinaryOperator::LessEqual => Ok(RuntimeValue::Bool(left <= right)),
            BinaryOperator::Greater => Ok(RuntimeValue::Bool(left > right)),
            BinaryOperator::GreaterEqual => Ok(RuntimeValue::Bool(left >= right)),
            _ => Err(runtime_error("invalid integer operator")),
        },
        (RuntimeValue::Int(left), RuntimeValue::Number(right)) => {
            numeric_binary(operator, *left as f64, *right)
        }
        (RuntimeValue::Number(left), RuntimeValue::Int(right)) => {
            numeric_binary(operator, *left, *right as f64)
        }
        (RuntimeValue::Number(left), RuntimeValue::Number(right)) => {
            numeric_binary(operator, *left, *right)
        }
        (RuntimeValue::String(left), RuntimeValue::String(right)) => match operator {
            BinaryOperator::Add => Ok(RuntimeValue::String(format!("{left}{right}"))),
            BinaryOperator::Less => Ok(RuntimeValue::Bool(left < right)),
            BinaryOperator::LessEqual => Ok(RuntimeValue::Bool(left <= right)),
            BinaryOperator::Greater => Ok(RuntimeValue::Bool(left > right)),
            BinaryOperator::GreaterEqual => Ok(RuntimeValue::Bool(left >= right)),
            _ => Err(runtime_error("invalid string operator")),
        },
        _ => Err(runtime_error(format!(
            "operator {operator:?} does not accept {} and {}",
            left.type_name(),
            right.type_name()
        ))),
    }
}

fn numeric_binary(
    operator: BinaryOperator,
    left: f64,
    right: f64,
) -> Result<RuntimeValue, SessionExecutionError> {
    let value = match operator {
        BinaryOperator::Add => RuntimeValue::Number(left + right),
        BinaryOperator::Subtract => RuntimeValue::Number(left - right),
        BinaryOperator::Multiply => RuntimeValue::Number(left * right),
        BinaryOperator::Divide if right != 0.0 => RuntimeValue::Number(left / right),
        BinaryOperator::Remainder if right != 0.0 => RuntimeValue::Number(left % right),
        BinaryOperator::Less => RuntimeValue::Bool(left < right),
        BinaryOperator::LessEqual => RuntimeValue::Bool(left <= right),
        BinaryOperator::Greater => RuntimeValue::Bool(left > right),
        BinaryOperator::GreaterEqual => RuntimeValue::Bool(left >= right),
        BinaryOperator::Divide | BinaryOperator::Remainder => {
            return Err(runtime_error("division by zero"));
        }
        _ => return Err(runtime_error("invalid number operator")),
    };
    if let RuntimeValue::Number(value) = value
        && !value.is_finite()
    {
        return Err(runtime_error(
            "number operation produced a non-finite value",
        ));
    }
    Ok(value)
}

fn runtime_equal(left: &RuntimeValue, right: &RuntimeValue) -> Result<bool, SessionExecutionError> {
    Ok(left.to_json()? == right.to_json()?)
}

fn eval_pure_builtin(
    name: &str,
    values: Vec<(Option<String>, RuntimeValue)>,
    state: &mut EvalState,
    cancellation: &CancellationToken,
) -> Result<RuntimeValue, SessionExecutionError> {
    if values.iter().any(|(name, _)| name.is_some()) {
        return Err(runtime_error(format!(
            "pure builtin `{name}` accepts positional arguments only"
        )));
    }
    let values = values
        .into_iter()
        .map(|(_, value)| value)
        .collect::<Vec<_>>();
    match name {
        "len" => match values[0].transparent() {
            RuntimeValue::String(value) => Ok(RuntimeValue::Int(value.chars().count() as i64)),
            RuntimeValue::List(value) => Ok(RuntimeValue::Int(value.len() as i64)),
            RuntimeValue::Object(value) => Ok(RuntimeValue::Int(value.len() as i64)),
            value => Err(type_error("len", "string, list, or object", value)),
        },
        "range" => {
            let integers = values
                .iter()
                .map(expect_int_ref)
                .collect::<Result<Vec<_>, _>>()?;
            let (start, end, step) = match integers.as_slice() {
                [end] => (0, *end, 1),
                [start, end] => (*start, *end, 1),
                [start, end, step] => (*start, *end, *step),
                _ => return Err(runtime_error("range expects 1 to 3 arguments")),
            };
            if step == 0 {
                return Err(runtime_error("range step cannot be zero"));
            }
            let mut output = Vec::new();
            let mut current = start;
            while if step > 0 {
                current < end
            } else {
                current > end
            } {
                state.step(cancellation)?;
                output.push(RuntimeValue::Int(current));
                current = current
                    .checked_add(step)
                    .ok_or_else(|| runtime_error("range overflow"))?;
            }
            Ok(RuntimeValue::List(Arc::new(output)))
        }
        "enumerate" => {
            let collection = finite_collection(values[0].clone(), "enumerate")?;
            Ok(RuntimeValue::List(Arc::new(
                collection
                    .into_iter()
                    .enumerate()
                    .map(|(index, value)| {
                        RuntimeValue::List(Arc::new(vec![RuntimeValue::Int(index as i64), value]))
                    })
                    .collect(),
            )))
        }
        "zip" => {
            let collections = values
                .into_iter()
                .map(|value| finite_collection(value, "zip"))
                .collect::<Result<Vec<_>, _>>()?;
            let length = collections.iter().map(Vec::len).min().unwrap_or(0);
            Ok(RuntimeValue::List(Arc::new(
                (0..length)
                    .map(|index| {
                        RuntimeValue::List(Arc::new(
                            collections
                                .iter()
                                .map(|values| values[index].clone())
                                .collect(),
                        ))
                    })
                    .collect(),
            )))
        }
        "min" | "max" => {
            let candidates = if values.len() == 1 {
                finite_collection(values[0].clone(), name)?
            } else {
                values
            };
            let mut iter = candidates.into_iter();
            let mut selected = iter
                .next()
                .ok_or_else(|| runtime_error(format!("{name} requires a non-empty collection")))?;
            for candidate in iter {
                let order = compare_values(&candidate, &selected)?;
                if (name == "min" && order.is_lt()) || (name == "max" && order.is_gt()) {
                    selected = candidate;
                }
            }
            Ok(selected)
        }
        "clamp" => {
            if let (
                RuntimeValue::Int(value),
                RuntimeValue::Int(minimum),
                RuntimeValue::Int(maximum),
            ) = (
                values[0].transparent(),
                values[1].transparent(),
                values[2].transparent(),
            ) {
                if minimum > maximum {
                    return Err(runtime_error("clamp minimum exceeds maximum"));
                }
                return Ok(RuntimeValue::Int((*value).clamp(*minimum, *maximum)));
            }
            let value = expect_number_ref(&values[0])?;
            let minimum = expect_number_ref(&values[1])?;
            let maximum = expect_number_ref(&values[2])?;
            if minimum > maximum {
                return Err(runtime_error("clamp minimum exceeds maximum"));
            }
            Ok(RuntimeValue::Number(value.clamp(minimum, maximum)))
        }
        "get" => {
            let default = values.get(2).cloned().unwrap_or(RuntimeValue::Null);
            match (values[0].transparent(), values[1].transparent()) {
                (RuntimeValue::Object(object), RuntimeValue::String(key)) => {
                    Ok(object.get(key).cloned().unwrap_or(default))
                }
                (RuntimeValue::List(list), RuntimeValue::Int(index)) => usize::try_from(*index)
                    .ok()
                    .and_then(|index| list.get(index).cloned())
                    .map_or(Ok(default), Ok),
                _ => Err(runtime_error("get expects object/string or list/int")),
            }
        }
        "append" => {
            let RuntimeValue::List(list) = values[0].transparent() else {
                return Err(type_error("append", "list", &values[0]));
            };
            let mut output = list.as_ref().clone();
            output.push(values[1].clone());
            Ok(RuntimeValue::List(Arc::new(output)))
        }
        "extend" => {
            let RuntimeValue::List(left) = values[0].transparent() else {
                return Err(type_error("extend", "list", &values[0]));
            };
            let RuntimeValue::List(right) = values[1].transparent() else {
                return Err(type_error("extend", "list", &values[1]));
            };
            let mut output = left.as_ref().clone();
            output.extend(right.iter().cloned());
            Ok(RuntimeValue::List(Arc::new(output)))
        }
        "update" => {
            let RuntimeValue::Object(left) = values[0].transparent() else {
                return Err(type_error("update", "object", &values[0]));
            };
            let RuntimeValue::Object(right) = values[1].transparent() else {
                return Err(type_error("update", "object", &values[1]));
            };
            let mut output = left.as_ref().clone();
            output.extend(
                right
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone())),
            );
            Ok(RuntimeValue::Object(Arc::new(output)))
        }
        "slice" => {
            let start = usize::try_from(expect_int_ref(&values[1])?)
                .map_err(|_| runtime_error("slice start must be non-negative"))?;
            let end = values
                .get(2)
                .map(expect_int_ref)
                .transpose()?
                .map(usize::try_from)
                .transpose()
                .map_err(|_| runtime_error("slice end must be non-negative"))?;
            match values[0].transparent() {
                RuntimeValue::List(list) => {
                    let end = end.unwrap_or(list.len());
                    if start > end || end > list.len() {
                        return Err(runtime_error("slice bounds are invalid"));
                    }
                    Ok(RuntimeValue::List(Arc::new(list[start..end].to_vec())))
                }
                RuntimeValue::String(string) => {
                    let chars = string.chars().collect::<Vec<_>>();
                    let end = end.unwrap_or(chars.len());
                    if start > end || end > chars.len() {
                        return Err(runtime_error("slice bounds are invalid"));
                    }
                    Ok(RuntimeValue::String(chars[start..end].iter().collect()))
                }
                value => Err(type_error("slice", "list or string", value)),
            }
        }
        "trim" => Ok(RuntimeValue::String(
            expect_string_ref(&values[0])?.trim().to_string(),
        )),
        "string" => value_to_string(&values[0]).map(RuntimeValue::String),
        "int" => convert_int(&values[0]).map(RuntimeValue::Int),
        "number" => convert_number(&values[0]).map(RuntimeValue::Number),
        "is_null" => Ok(RuntimeValue::Bool(matches!(
            values[0].transparent(),
            RuntimeValue::Null
        ))),
        "is_bool" => Ok(RuntimeValue::Bool(matches!(
            values[0].transparent(),
            RuntimeValue::Bool(_)
        ))),
        "is_int" => Ok(RuntimeValue::Bool(matches!(
            values[0].transparent(),
            RuntimeValue::Int(_)
        ))),
        "is_number" => Ok(RuntimeValue::Bool(matches!(
            values[0].transparent(),
            RuntimeValue::Int(_) | RuntimeValue::Number(_)
        ))),
        "is_string" => Ok(RuntimeValue::Bool(matches!(
            values[0].transparent(),
            RuntimeValue::String(_)
        ))),
        "is_list" => Ok(RuntimeValue::Bool(matches!(
            values[0].transparent(),
            RuntimeValue::List(_)
        ))),
        "is_object" => Ok(RuntimeValue::Bool(matches!(
            values[0].transparent(),
            RuntimeValue::Object(_)
        ))),
        "assert" => {
            if expect_bool(values[0].clone(), "assert")? {
                Ok(RuntimeValue::Null)
            } else {
                Err(runtime_error(
                    values
                        .get(1)
                        .map(value_to_string)
                        .transpose()?
                        .unwrap_or_else(|| "assertion failed".to_string()),
                ))
            }
        }
        "fail" => Err(runtime_error(value_to_string(&values[0])?)),
        other => Err(runtime_error(format!("unknown pure builtin `{other}`"))),
    }
}

fn is_pure_builtin(name: &str) -> bool {
    matches!(
        name,
        "len"
            | "range"
            | "enumerate"
            | "zip"
            | "min"
            | "max"
            | "clamp"
            | "get"
            | "append"
            | "extend"
            | "update"
            | "slice"
            | "trim"
            | "string"
            | "int"
            | "number"
            | "is_null"
            | "is_bool"
            | "is_int"
            | "is_number"
            | "is_string"
            | "is_list"
            | "is_object"
            | "assert"
            | "fail"
    )
}

fn finite_collection(
    value: RuntimeValue,
    context: &str,
) -> Result<Vec<RuntimeValue>, SessionExecutionError> {
    match value.transparent() {
        RuntimeValue::List(values) => Ok(values.as_ref().clone()),
        RuntimeValue::Object(values) => {
            Ok(values.keys().cloned().map(RuntimeValue::String).collect())
        }
        RuntimeValue::String(value) => Ok(value
            .chars()
            .map(|value| RuntimeValue::String(value.to_string()))
            .collect()),
        value => Err(type_error(context, "finite list, object, or string", value)),
    }
}

fn named_values(
    values: Vec<(Option<String>, RuntimeValue)>,
    context: &str,
) -> Result<BTreeMap<String, RuntimeValue>, SessionExecutionError> {
    let mut output = BTreeMap::new();
    for (name, value) in values {
        let name =
            name.ok_or_else(|| runtime_error(format!("{context} accepts named arguments only")))?;
        if output.insert(name.clone(), value).is_some() {
            return Err(runtime_error(format!(
                "{context} repeats argument `{name}`"
            )));
        }
    }
    Ok(output)
}

fn bind_arguments(
    name: &str,
    parameters: &[String],
    values: Vec<(Option<String>, RuntimeValue)>,
) -> Result<Vec<RuntimeValue>, SessionExecutionError> {
    if values.iter().all(|(name, _)| name.is_none()) {
        if values.len() != parameters.len() {
            return Err(runtime_error(format!(
                "{name} expects {} arguments, got {}",
                parameters.len(),
                values.len()
            )));
        }
        return Ok(values.into_iter().map(|(_, value)| value).collect());
    }
    let values = named_values(values, name)?;
    parameters
        .iter()
        .map(|parameter| {
            values
                .get(parameter)
                .cloned()
                .ok_or_else(|| runtime_error(format!("{name} is missing argument `{parameter}`")))
        })
        .collect()
}

fn flexible_arguments(
    name: &str,
    parameters: &[&str],
    values: Vec<(Option<String>, RuntimeValue)>,
) -> Result<BTreeMap<String, RuntimeValue>, SessionExecutionError> {
    let mut output = BTreeMap::new();
    let mut positional = 0;
    for (argument_name, value) in values {
        let key = match argument_name {
            Some(name) => name,
            None => {
                let key = parameters
                    .get(positional)
                    .ok_or_else(|| runtime_error(format!("{name} has too many arguments")))?;
                positional += 1;
                (*key).to_string()
            }
        };
        if !parameters.contains(&key.as_str()) {
            return Err(runtime_error(format!(
                "{name} has unknown argument `{key}`"
            )));
        }
        if output.insert(key.clone(), value).is_some() {
            return Err(runtime_error(format!("{name} repeats argument `{key}`")));
        }
    }
    Ok(output)
}

fn option_string(
    values: &BTreeMap<String, RuntimeValue>,
    name: &str,
) -> Result<Option<String>, SessionExecutionError> {
    match values.get(name).map(RuntimeValue::transparent) {
        None | Some(RuntimeValue::Null) => Ok(None),
        Some(value) => expect_string_ref(value).map(|value| Some(value.to_string())),
    }
}

fn option_string_list(
    values: &BTreeMap<String, RuntimeValue>,
    name: &str,
) -> Result<Option<Vec<String>>, SessionExecutionError> {
    values
        .get(name)
        .map(|value| {
            let RuntimeValue::List(values) = value.transparent() else {
                return Err(type_error(name, "list", value));
            };
            values
                .iter()
                .map(|value| expect_string_ref(value).map(str::to_string))
                .collect()
        })
        .transpose()
}

fn required_string(
    values: &BTreeMap<String, RuntimeValue>,
    name: &str,
) -> Result<String, SessionExecutionError> {
    values
        .get(name)
        .ok_or_else(|| runtime_error(format!("missing argument `{name}`")))
        .and_then(expect_string_ref)
        .map(str::to_string)
}

fn optional_string_map(
    values: &BTreeMap<String, RuntimeValue>,
    name: &str,
) -> Result<Option<String>, SessionExecutionError> {
    option_string(values, name)
}

fn expect_string_ref(value: &RuntimeValue) -> Result<&str, SessionExecutionError> {
    match value.transparent() {
        RuntimeValue::String(value) => Ok(value),
        value => Err(type_error("value", "string", value)),
    }
}

fn expect_int_ref(value: &RuntimeValue) -> Result<i64, SessionExecutionError> {
    match value.transparent() {
        RuntimeValue::Int(value) => Ok(*value),
        value => Err(type_error("value", "int", value)),
    }
}

fn expect_number_ref(value: &RuntimeValue) -> Result<f64, SessionExecutionError> {
    match value.transparent() {
        RuntimeValue::Int(value) => Ok(*value as f64),
        RuntimeValue::Number(value) => Ok(*value),
        value => Err(type_error("value", "number", value)),
    }
}

fn expect_bool(value: RuntimeValue, context: &str) -> Result<bool, SessionExecutionError> {
    match value.transparent() {
        RuntimeValue::Bool(value) => Ok(*value),
        value => Err(type_error(context, "bool", value)),
    }
}

fn access_value(value: &RuntimeValue) -> Result<AccessPreset, SessionExecutionError> {
    match expect_string_ref(value)? {
        "model_only" => Ok(AccessPreset::ModelOnly),
        "read_only" => Ok(AccessPreset::ReadOnly),
        "workspace" => Ok(AccessPreset::Workspace),
        "full_access" => Ok(AccessPreset::FullAccess),
        value => Err(runtime_error(format!("invalid access preset `{value}`"))),
    }
}

fn canonical_scalar_key(value: &RuntimeValue) -> Result<String, SessionExecutionError> {
    match value.transparent() {
        RuntimeValue::Null => Ok("null".to_string()),
        RuntimeValue::Bool(value) => Ok(format!("bool:{value}")),
        RuntimeValue::Int(value) => Ok(format!("int:{value}")),
        RuntimeValue::Number(value) if value.is_finite() => Ok(format!("number:{value:e}")),
        RuntimeValue::String(value) => Ok(format!("string:{value}")),
        value => Err(type_error("key", "scalar", value)),
    }
}

fn value_to_string(value: &RuntimeValue) -> Result<String, SessionExecutionError> {
    match value.transparent() {
        RuntimeValue::Null => Ok("null".to_string()),
        RuntimeValue::Bool(value) => Ok(value.to_string()),
        RuntimeValue::Int(value) => Ok(value.to_string()),
        RuntimeValue::Number(value) => Ok(value.to_string()),
        RuntimeValue::String(value) => Ok(value.clone()),
        RuntimeValue::List(_) | RuntimeValue::Object(_) | RuntimeValue::ArtifactRef(_) => {
            serde_json::to_string(&value.to_json()?).map_err(Into::into)
        }
        value => Err(type_error("string", "JSON value", value)),
    }
}

fn convert_int(value: &RuntimeValue) -> Result<i64, SessionExecutionError> {
    match value.transparent() {
        RuntimeValue::Int(value) => Ok(*value),
        RuntimeValue::Number(value)
            if value.is_finite()
                && value.fract() == 0.0
                && *value >= i64::MIN as f64
                && *value <= i64::MAX as f64 =>
        {
            Ok(*value as i64)
        }
        RuntimeValue::String(value) => value
            .parse()
            .map_err(|_| runtime_error("string is not an int")),
        value => Err(type_error("int", "int, integral number, or string", value)),
    }
}

fn convert_number(value: &RuntimeValue) -> Result<f64, SessionExecutionError> {
    let value = match value.transparent() {
        RuntimeValue::Int(value) => *value as f64,
        RuntimeValue::Number(value) => *value,
        RuntimeValue::String(value) => value
            .parse()
            .map_err(|_| runtime_error("string is not a number"))?,
        value => return Err(type_error("number", "number or string", value)),
    };
    if value.is_finite() {
        Ok(value)
    } else {
        Err(runtime_error("number must be finite"))
    }
}

fn compare_values(
    left: &RuntimeValue,
    right: &RuntimeValue,
) -> Result<std::cmp::Ordering, SessionExecutionError> {
    match (left.transparent(), right.transparent()) {
        (RuntimeValue::Int(left), RuntimeValue::Int(right)) => Ok(left.cmp(right)),
        (RuntimeValue::String(left), RuntimeValue::String(right)) => Ok(left.cmp(right)),
        (RuntimeValue::Int(left), RuntimeValue::Number(right)) => (*left as f64)
            .partial_cmp(right)
            .ok_or_else(|| runtime_error("numbers are not comparable")),
        (RuntimeValue::Number(left), RuntimeValue::Int(right)) => left
            .partial_cmp(&(*right as f64))
            .ok_or_else(|| runtime_error("numbers are not comparable")),
        (RuntimeValue::Number(left), RuntimeValue::Number(right)) => left
            .partial_cmp(right)
            .ok_or_else(|| runtime_error("numbers are not comparable")),
        _ => Err(runtime_error("values are not mutually comparable")),
    }
}

fn object_from_json(value: Value) -> Result<BTreeMap<String, RuntimeValue>, SessionExecutionError> {
    match RuntimeValue::from_json(value) {
        RuntimeValue::Object(value) => Ok(value.as_ref().clone()),
        value => Err(type_error("effect result", "object", &value)),
    }
}

fn artifact_from_json(
    value: Value,
) -> Result<BTreeMap<String, RuntimeValue>, SessionExecutionError> {
    let mut value = object_from_json(value)?;
    if let Some(id) = value.get("artifact_id").cloned() {
        value.insert("id".to_string(), id);
    }
    Ok(value)
}

fn expect_json_string<'a>(value: &'a Value, field: &str) -> Result<&'a str, SessionExecutionError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| runtime_error(format!("effect result has no string field `{field}`")))
}

fn action_prompt(action: &ActionDefinition) -> String {
    if action.finalize != FinalizePolicy::IfNeeded {
        return action.prompt.clone();
    }
    let schema = action.result.as_ref().expect("compiler requires schema");
    format!(
        "{}\n\nAt the end of your normal report, also provide one machine-readable JSON value satisfying this schema. The report may contain prose; the runtime extracts and validates the JSON value.\n{}",
        action.prompt,
        serde_json::to_string_pretty(&schema.to_json_schema()).unwrap_or_default()
    )
}

fn response_format(name: &str, schema: &crate::language::BoundarySchema) -> ModelResponseFormat {
    ModelResponseFormat {
        name: format!("{name}_result"),
        schema: schema.to_json_schema(),
        strict: matches!(
            &schema.kind,
            SchemaKind::Object {
                allow_extra: false,
                ..
            }
        ),
    }
}

fn structured_finalizer_prompt(schema: &crate::language::BoundarySchema) -> String {
    format!(
        "Extract the final structured result from the immediately preceding action response. Do not do new research or call tools. Preserve all recoverable information and return only one valid JSON value satisfying this schema:\n{}",
        serde_json::to_string_pretty(&schema.to_json_schema()).unwrap_or_default()
    )
}

fn deliverable_finalizer_prompt(schema: Option<&crate::language::BoundarySchema>) -> String {
    let mut prompt = "Turn the immediately preceding action result into the complete, self-contained deliverable requested by that action. Do not do new research or call tools. Preserve verified evidence, source URLs, exact values, and material limitations.".to_string();
    if let Some(schema) = schema {
        prompt.push_str(" Return only one valid JSON value satisfying this schema:\n");
        prompt
            .push_str(&serde_json::to_string_pretty(&schema.to_json_schema()).unwrap_or_default());
    }
    prompt
}

fn repair_prompt(schema: &crate::language::BoundarySchema) -> String {
    format!(
        "Repair the immediately preceding response without doing new research or calling tools. Preserve all recoverable information and return only one complete valid JSON value satisfying this schema:\n{}",
        serde_json::to_string_pretty(&schema.to_json_schema()).unwrap_or_default()
    )
}

fn parse_and_validate_action_json(
    output: &str,
    schema: &crate::language::BoundarySchema,
) -> Result<Value, String> {
    let mut parsed = parse_action_json(output, schema.top_level_kind())?;
    schema.apply_defaults(&mut parsed);
    schema.validate(&parsed, "result")?;
    Ok(parsed)
}

fn parse_action_json(output: &str, top_level: &str) -> Result<Value, String> {
    if let Ok(value) = serde_json::from_str(output) {
        return Ok(value);
    }
    let stripped = output.trim();
    if stripped.starts_with("```")
        && stripped.ends_with("```")
        && let Some(newline) = stripped.find('\n')
    {
        let fenced = stripped[newline + 1..stripped.len() - 3].trim();
        if let Ok(value) = serde_json::from_str(fenced) {
            return Ok(value);
        }
    }
    let delimiter = match top_level {
        "object" => '{',
        "array" => '[',
        _ => return Err("response is not one complete JSON value".to_string()),
    };
    for (index, character) in output.char_indices() {
        if character != delimiter {
            continue;
        }
        let mut values = serde_json::Deserializer::from_str(&output[index..]).into_iter::<Value>();
        if let Some(Ok(value)) = values.next() {
            return Ok(value);
        }
    }
    Err("response contains no decodable structured JSON value".to_string())
}

fn short_hash(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))[..16].to_string()
}

fn runtime_error(message: impl Into<String>) -> SessionExecutionError {
    SessionExecutionError::Protocol(message.into())
}

fn type_error(context: &str, expected: &str, actual: &RuntimeValue) -> SessionExecutionError {
    runtime_error(format!(
        "{context} requires {expected}, got {}",
        actual.type_name()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language::BoundarySchema;

    #[test]
    fn action_json_parser_accepts_whole_fence_and_first_object() {
        let schema = BoundarySchema::object(vec![crate::language::SchemaField {
            name: "status".to_string(),
            schema: BoundarySchema::new(SchemaKind::String),
            optional: false,
        }]);
        for output in [
            r#"{"status":"ok"}"#,
            "```json\n{\"status\":\"ok\"}\n```",
            "report first\n{\"status\":\"ok\"}\nend",
        ] {
            assert_eq!(
                parse_and_validate_action_json(output, &schema).expect("valid"),
                json!({"status":"ok"})
            );
        }
    }

    #[test]
    fn collections_are_immutable_values() {
        let original = RuntimeValue::List(Arc::new(vec![RuntimeValue::Int(1)]));
        let mut state = EvalState::new();
        let appended = eval_pure_builtin(
            "append",
            vec![(None, original.clone()), (None, RuntimeValue::Int(2))],
            &mut state,
            &CancellationToken::new(),
        )
        .expect("append works");
        assert_eq!(original.to_json().expect("json"), json!([1]));
        assert_eq!(appended.to_json().expect("json"), json!([1, 2]));
    }

    #[test]
    fn evidence_plan_schema_accepts_the_builtin_contract() {
        let source = include_str!("../../../workflows/builtin/evidence-loop/workflow.pm");
        let compiled = crate::language::compile_source(source, &BTreeSet::new())
            .expect("Evidence Workflow should compile");
        let schema = compiled.program.agents["Planner"].actions["plan"]
            .result
            .as_ref()
            .expect("Planner result schema");
        let output = r#"{"deliverable":"comparison","acceptance_criteria":["two routes"],"routes":[{"key":"primary","name":"Primary","objective":"Find primary support"},{"key":"challenge","name":"Challenge","objective":"Find counterevidence"}],"verification_notes":["cross-check"]}"#;
        parse_and_validate_action_json(output, schema).expect("plan should match its schema");
    }

    #[test]
    fn pure_fuel_and_dynamic_type_errors_fail_closed() {
        let mut state = EvalState::new();
        state.fuel = 2;
        let error = eval_pure_builtin(
            "range",
            vec![(None, RuntimeValue::Int(5))],
            &mut state,
            &CancellationToken::new(),
        )
        .expect_err("range must consume public fuel");
        assert!(error.to_string().contains("IR-step fuel"));
        assert!(
            apply_binary(
                BinaryOperator::Add,
                RuntimeValue::String("one".to_string()),
                RuntimeValue::Int(1),
            )
            .is_err()
        );
    }
}
