use super::ast::*;
use super::lexer::Span;
use papermachine_protocol::{
    DiagnosticSeverity, WorkflowActionDeclaration, WorkflowAgentDeclaration, WorkflowDiagnostic,
    WorkflowProgramId, WorkflowProgramManifest, WorkflowValidation,
};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

const PURE_BUILTINS: &[(&str, usize, usize)] = &[
    ("len", 1, 1),
    ("range", 1, 3),
    ("enumerate", 1, 1),
    ("zip", 2, usize::MAX),
    ("min", 1, usize::MAX),
    ("max", 1, usize::MAX),
    ("clamp", 3, 3),
    ("get", 2, 3),
    ("append", 2, 2),
    ("extend", 2, 2),
    ("update", 2, 2),
    ("slice", 2, 3),
    ("trim", 1, 1),
    ("string", 1, 1),
    ("int", 1, 1),
    ("number", 1, 1),
    ("is_null", 1, 1),
    ("is_bool", 1, 1),
    ("is_int", 1, 1),
    ("is_number", 1, 1),
    ("is_string", 1, 1),
    ("is_list", 1, 1),
    ("is_object", 1, 1),
    ("assert", 1, 2),
    ("fail", 1, 1),
];

const EFFECT_BUILTINS: &[(&str, usize, usize)] = &[
    ("ask_human", 1, 3),
    ("wait", 0, 3),
    ("publish_artifact", 2, 6),
    ("publish_home", 1, 2),
];

#[derive(Clone, Debug)]
pub struct CompiledWorkflow {
    pub program: Program,
    pub manifest: WorkflowProgramManifest,
    pub validation: WorkflowValidation,
    pub ir_sha256: String,
}

pub fn compile_source(
    source: &str,
    known_tools: &BTreeSet<String>,
) -> Result<CompiledWorkflow, Box<WorkflowValidation>> {
    let program = match super::parse_source(source) {
        Ok(program) => program,
        Err(error) => {
            return Err(Box::new(WorkflowValidation {
                valid: false,
                manifest: None,
                agents: Vec::new(),
                diagnostics: vec![diagnostic(error.message, error.span)],
            }));
        }
    };

    let manifest = manifest(&program);
    let agents = agent_declarations(&program);
    let mut validator = Validator::new(&program, known_tools);
    validator.validate();
    let validation = WorkflowValidation {
        valid: validator.diagnostics.is_empty(),
        manifest: Some(manifest.clone()),
        agents,
        diagnostics: validator.diagnostics,
    };
    if !validation.valid {
        return Err(Box::new(validation));
    }
    let ir_sha256 = canonical_ir_sha256(&program).map_err(|message| {
        Box::new(WorkflowValidation {
            valid: false,
            manifest: Some(manifest.clone()),
            agents: validation.agents.clone(),
            diagnostics: vec![diagnostic(message, Some(program.workflow.span))],
        })
    })?;
    Ok(CompiledWorkflow {
        program,
        manifest,
        validation,
        ir_sha256,
    })
}

pub fn validate_source(source: &str, known_tools: &BTreeSet<String>) -> WorkflowValidation {
    match compile_source(source, known_tools) {
        Ok(compiled) => compiled.validation,
        Err(validation) => *validation,
    }
}

fn manifest(program: &Program) -> WorkflowProgramManifest {
    let workflow = &program.workflow;
    WorkflowProgramManifest {
        id: WorkflowProgramId::from_uuid(uuid::Uuid::new_v5(
            &uuid::Uuid::NAMESPACE_URL,
            format!("papermachine:workflow:{}", workflow.slug).as_bytes(),
        )),
        slug: workflow.slug.clone(),
        name: workflow.name.clone(),
        description: workflow.description.clone(),
        language_version: program.version,
        request_mode: workflow.request_mode,
        params_schema: super::schema::BoundarySchema::object(
            workflow
                .params
                .iter()
                .map(|parameter| super::schema::SchemaField {
                    name: parameter.name.clone(),
                    schema: parameter.schema.clone(),
                    optional: parameter.optional,
                })
                .collect(),
        )
        .to_json_schema(),
    }
}

fn agent_declarations(program: &Program) -> Vec<WorkflowAgentDeclaration> {
    program
        .agents
        .values()
        .map(|agent| WorkflowAgentDeclaration {
            class_name: agent.name.clone(),
            actions: agent
                .actions
                .values()
                .map(|action| WorkflowActionDeclaration {
                    name: action.name.clone(),
                    tools: action.tools.clone(),
                })
                .collect(),
            access: agent.access,
        })
        .collect()
}

fn canonical_ir_sha256(program: &Program) -> Result<String, String> {
    let mut value = serde_json::to_value(program).map_err(|error| error.to_string())?;
    remove_spans(&mut value);
    let bytes = serde_json::to_vec(&CanonicalIr {
        format: "papermachine-workflow-ir-v1",
        program: value,
    })
    .map_err(|error| error.to_string())?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

#[derive(Serialize)]
struct CanonicalIr<'a> {
    format: &'a str,
    program: Value,
}

fn remove_spans(value: &mut Value) {
    match value {
        Value::Array(values) => {
            for value in values {
                remove_spans(value);
            }
        }
        Value::Object(object) => {
            object.remove("span");
            for value in object.values_mut() {
                remove_spans(value);
            }
        }
        _ => {}
    }
}

fn diagnostic(message: String, span: Option<Span>) -> WorkflowDiagnostic {
    WorkflowDiagnostic {
        severity: DiagnosticSeverity::Error,
        message,
        line: span.map(|span| span.line),
        column: span.map(|span| span.column),
    }
}

#[derive(Clone, Debug)]
struct Binding {
    mutable: bool,
    agent_template: Option<String>,
}

struct Validator<'a> {
    program: &'a Program,
    known_tools: &'a BTreeSet<String>,
    diagnostics: Vec<WorkflowDiagnostic>,
    function_calls: BTreeMap<String, BTreeSet<String>>,
    effectful_functions: BTreeSet<String>,
}

impl<'a> Validator<'a> {
    fn new(program: &'a Program, known_tools: &'a BTreeSet<String>) -> Self {
        Self {
            program,
            known_tools,
            diagnostics: Vec::new(),
            function_calls: BTreeMap::new(),
            effectful_functions: BTreeSet::new(),
        }
    }

    fn validate(&mut self) {
        self.validate_declarations();
        self.collect_function_graph();
        self.reject_recursion();
        self.resolve_effectful_functions();

        for function in self.program.functions.values() {
            let mut scopes = vec![HashMap::new()];
            for parameter in &function.parameters {
                scopes[0].insert(
                    parameter.clone(),
                    Binding {
                        mutable: false,
                        agent_template: None,
                    },
                );
            }
            self.validate_block(&function.body, &mut scopes, 0, &function.name);
        }

        let workflow = &self.program.workflow;
        let mut scopes = vec![HashMap::new()];
        scopes[0].insert(
            workflow.run_parameter.clone(),
            Binding {
                mutable: false,
                agent_template: None,
            },
        );
        self.validate_block(&workflow.body, &mut scopes, 0, "run");
    }

    fn validate_declarations(&mut self) {
        for (name, schema) in &self.program.schemas {
            if let Err(message) = super::schema::validate_json_schema_definition(
                &schema.to_json_schema(),
                &format!("schema {name}"),
            ) {
                self.error(message, Some(self.program.workflow.span));
            }
        }
        for parameter in &self.program.workflow.params {
            if let Err(message) = super::schema::validate_json_schema_definition(
                &parameter.schema.to_json_schema(),
                &format!("parameter {}", parameter.name),
            ) {
                self.error(message, Some(parameter.span));
            }
        }
        let mut names = BTreeSet::new();
        for name in self
            .program
            .agents
            .keys()
            .chain(self.program.functions.keys())
        {
            if !names.insert(name.as_str()) || is_builtin(name) {
                self.error(
                    format!(
                        "top-level name `{name}` conflicts with another declaration or builtin"
                    ),
                    self.program
                        .agents
                        .get(name)
                        .map(|agent| agent.span)
                        .or_else(|| {
                            self.program
                                .functions
                                .get(name)
                                .map(|function| function.span)
                        }),
                );
            }
        }
        for agent in self.program.agents.values() {
            for action in agent.actions.values() {
                if let Some(schema) = &action.result
                    && let Err(message) = super::schema::validate_json_schema_definition(
                        &schema.to_json_schema(),
                        &format!("Action {}.{} result", agent.name, action.name),
                    )
                {
                    self.error(message, Some(action.span));
                }
                let mut seen = BTreeSet::new();
                for tool in action.tools.iter().flatten() {
                    let normalized = tool.trim();
                    if normalized.is_empty() {
                        self.error(
                            format!(
                                "Action {}.{} declares an empty tool name",
                                agent.name, action.name
                            ),
                            Some(action.span),
                        );
                    } else if !seen.insert(normalized) {
                        self.error(
                            format!(
                                "Action {}.{} declares duplicate tool {tool:?}",
                                agent.name, action.name
                            ),
                            Some(action.span),
                        );
                    } else if !self.known_tools.contains(normalized) {
                        self.error(
                            format!(
                                "Action {}.{} declares unknown tool {tool:?}",
                                agent.name, action.name
                            ),
                            Some(action.span),
                        );
                    }
                }
            }
        }
    }

    fn collect_function_graph(&mut self) {
        for (name, function) in &self.program.functions {
            let mut calls = BTreeSet::new();
            collect_named_calls_block(&function.body, &self.program.functions, &mut calls);
            if block_contains_await(&function.body) {
                self.effectful_functions.insert(name.clone());
            }
            self.function_calls.insert(name.clone(), calls);
        }
    }

    fn reject_recursion(&mut self) {
        fn visit(
            name: &str,
            calls: &BTreeMap<String, BTreeSet<String>>,
            visiting: &mut Vec<String>,
            done: &mut HashSet<String>,
        ) -> Option<Vec<String>> {
            if let Some(index) = visiting.iter().position(|item| item == name) {
                let mut cycle = visiting[index..].to_vec();
                cycle.push(name.to_string());
                return Some(cycle);
            }
            if !done.insert(name.to_string()) {
                return None;
            }
            visiting.push(name.to_string());
            for callee in calls.get(name).into_iter().flatten() {
                if let Some(cycle) = visit(callee, calls, visiting, done) {
                    return Some(cycle);
                }
            }
            visiting.pop();
            None
        }

        let mut done = HashSet::new();
        for function in self.program.functions.values() {
            let mut visiting = Vec::new();
            if let Some(cycle) = visit(
                &function.name,
                &self.function_calls,
                &mut visiting,
                &mut done,
            ) {
                self.error(
                    format!(
                        "recursive function calls are forbidden: {}",
                        cycle.join(" -> ")
                    ),
                    Some(function.span),
                );
            }
        }
    }

    fn resolve_effectful_functions(&mut self) {
        loop {
            let mut changed = false;
            for (caller, callees) in &self.function_calls {
                if !self.effectful_functions.contains(caller)
                    && callees
                        .iter()
                        .any(|callee| self.effectful_functions.contains(callee))
                {
                    changed |= self.effectful_functions.insert(caller.clone());
                }
            }
            if !changed {
                break;
            }
        }
    }

    fn validate_block(
        &mut self,
        block: &Block,
        scopes: &mut Vec<HashMap<String, Binding>>,
        loop_depth: usize,
        owner: &str,
    ) {
        scopes.push(HashMap::new());
        for statement in &block.statements {
            self.validate_statement(statement, scopes, loop_depth, owner);
        }
        if let Some(tail) = &block.tail {
            self.validate_expression(tail, scopes, owner, false);
        }
        scopes.pop();
    }

    fn validate_statement(
        &mut self,
        statement: &Statement,
        scopes: &mut Vec<HashMap<String, Binding>>,
        loop_depth: usize,
        owner: &str,
    ) {
        match &statement.kind {
            StatementKind::Let {
                name,
                mutable,
                value,
            } => {
                self.validate_expression(value, scopes, owner, false);
                let agent_template = constructor_template(value, &self.program.agents);
                let current = scopes.last_mut().expect("scope exists");
                if current.contains_key(name) {
                    self.error(
                        format!("duplicate local binding `{name}`"),
                        Some(statement.span),
                    );
                } else {
                    current.insert(
                        name.clone(),
                        Binding {
                            mutable: *mutable,
                            agent_template,
                        },
                    );
                }
            }
            StatementKind::Assign { name, value, .. } => {
                self.validate_expression(value, scopes, owner, false);
                match lookup_binding(scopes, name) {
                    Some(binding) if binding.mutable => {}
                    Some(_) => self.error(
                        format!("cannot rebind immutable `let` binding `{name}`"),
                        Some(statement.span),
                    ),
                    None => self.error(format!("undefined binding `{name}`"), Some(statement.span)),
                }
            }
            StatementKind::Expression { expression } => {
                self.validate_expression(expression, scopes, owner, false);
            }
            StatementKind::If {
                condition,
                then_block,
                else_block,
            } => {
                self.validate_expression(condition, scopes, owner, false);
                self.validate_block(then_block, scopes, loop_depth, owner);
                if let Some(block) = else_block {
                    self.validate_block(block, scopes, loop_depth, owner);
                }
            }
            StatementKind::While { condition, body } => {
                self.validate_expression(condition, scopes, owner, false);
                self.validate_block(body, scopes, loop_depth + 1, owner);
                if !back_edges_await(body, expression_guarantees_await(condition)) {
                    self.error(
                        "every `while` back edge must pass through a durable `await`".to_string(),
                        Some(statement.span),
                    );
                }
            }
            StatementKind::Loop { body } => {
                self.validate_block(body, scopes, loop_depth + 1, owner);
                if !back_edges_await(body, false) {
                    self.error(
                        "every `loop` back edge must pass through a durable `await`".to_string(),
                        Some(statement.span),
                    );
                }
            }
            StatementKind::For {
                binding,
                iterable,
                body,
            } => {
                self.validate_expression(iterable, scopes, owner, false);
                scopes.push(HashMap::from([(
                    binding.clone(),
                    Binding {
                        mutable: false,
                        agent_template: None,
                    },
                )]));
                self.validate_block(body, scopes, loop_depth + 1, owner);
                scopes.pop();
            }
            StatementKind::Match { value, arms } => {
                self.validate_expression(value, scopes, owner, false);
                let mut wildcard = false;
                for arm in arms {
                    wildcard |= arm
                        .patterns
                        .iter()
                        .any(|pattern| matches!(pattern, Pattern::Wildcard));
                    match &arm.body {
                        MatchBody::Block(block) => {
                            self.validate_block(block, scopes, loop_depth, owner)
                        }
                        MatchBody::Statement(statement) => {
                            self.validate_statement(statement, scopes, loop_depth, owner)
                        }
                        MatchBody::Expression(expression) => {
                            self.validate_expression(expression, scopes, owner, false)
                        }
                    }
                }
                if !wildcard {
                    self.error(
                        "`match` must include a `_` arm so unmatched dynamic values fail at compile time"
                            .to_string(),
                        Some(statement.span),
                    );
                }
            }
            StatementKind::Break | StatementKind::Continue => {
                if loop_depth == 0 {
                    self.error(
                        "`break` and `continue` are valid only inside a loop".to_string(),
                        Some(statement.span),
                    );
                }
            }
            StatementKind::Return { value } => {
                self.validate_expression(value, scopes, owner, false);
            }
        }
    }

    fn validate_expression(
        &mut self,
        expression: &Expression,
        scopes: &[HashMap<String, Binding>],
        owner: &str,
        in_await: bool,
    ) {
        match &expression.kind {
            ExpressionKind::Literal { .. } => {}
            ExpressionKind::Variable { name } => {
                if lookup_binding(scopes, name).is_none()
                    && !self.program.functions.contains_key(name)
                    && !self.program.agents.contains_key(name)
                    && !is_builtin(name)
                {
                    self.error(format!("undefined name `{name}`"), Some(expression.span));
                }
            }
            ExpressionKind::List { values } => {
                for value in values {
                    self.validate_expression(value, scopes, owner, false);
                }
            }
            ExpressionKind::Object { fields } => {
                for (_, value) in fields {
                    self.validate_expression(value, scopes, owner, false);
                }
            }
            ExpressionKind::Unary { value, .. } => {
                self.validate_expression(value, scopes, owner, false);
            }
            ExpressionKind::Binary { left, right, .. } => {
                self.validate_expression(left, scopes, owner, false);
                self.validate_expression(right, scopes, owner, false);
            }
            ExpressionKind::Member { value, .. } => {
                self.validate_expression(value, scopes, owner, false);
            }
            ExpressionKind::Index { value, index } => {
                self.validate_expression(value, scopes, owner, false);
                self.validate_expression(index, scopes, owner, false);
            }
            ExpressionKind::Call { callee, arguments } => {
                for argument in arguments {
                    self.validate_expression(&argument.value, scopes, owner, false);
                }
                self.validate_call(expression, callee, arguments, scopes, owner, in_await);
            }
            ExpressionKind::Await { value } => {
                if !matches!(value.kind, ExpressionKind::Call { .. }) {
                    self.error(
                        "`await` requires an effect or effectful function call".to_string(),
                        Some(value.span),
                    );
                }
                self.validate_expression(value, scopes, owner, true);
            }
            ExpressionKind::Parallel { branches } => {
                let mut names = BTreeSet::new();
                for branch in branches {
                    if !names.insert(branch.name.as_str()) {
                        self.error(
                            format!("duplicate parallel branch `{}`", branch.name),
                            Some(branch.span),
                        );
                    }
                    let mut branch_scopes = scopes.to_vec();
                    self.validate_block(&branch.body, &mut branch_scopes, 0, owner);
                }
            }
            ExpressionKind::ParallelFor {
                binding,
                iterable,
                key,
                body,
            } => {
                self.validate_expression(iterable, scopes, owner, false);
                let mut branch_scopes = scopes.to_vec();
                branch_scopes.push(HashMap::from([(
                    binding.clone(),
                    Binding {
                        mutable: false,
                        agent_template: None,
                    },
                )]));
                self.validate_expression(key, &branch_scopes, owner, false);
                if block_contains_expression_await(key) {
                    self.error(
                        "parallel keys must be pure scalar expressions".to_string(),
                        Some(key.span),
                    );
                }
                self.validate_block(body, &mut branch_scopes, 0, owner);
            }
        }
    }

    fn validate_call(
        &mut self,
        expression: &Expression,
        callee: &Expression,
        arguments: &[CallArgument],
        scopes: &[HashMap<String, Binding>],
        _owner: &str,
        in_await: bool,
    ) {
        match &callee.kind {
            ExpressionKind::Variable { name } if self.program.agents.contains_key(name) => {
                const OPTIONS: &[&str] =
                    &["key", "name", "role", "system", "model", "skills", "access"];
                validate_named_options(self, arguments, OPTIONS, expression.span);
                if arguments.iter().any(|argument| argument.name.is_none()) {
                    self.error(
                        format!("Agent constructor `{name}` accepts named arguments only"),
                        Some(expression.span),
                    );
                }
            }
            ExpressionKind::Variable { name } if self.program.functions.contains_key(name) => {
                let function = &self.program.functions[name];
                validate_parameters(self, name, &function.parameters, arguments, expression.span);
                if self.effectful_functions.contains(name) != in_await {
                    let message = if in_await {
                        format!("pure function `{name}` cannot be awaited")
                    } else {
                        format!("effectful function `{name}` must be awaited")
                    };
                    self.error(message, Some(expression.span));
                }
            }
            ExpressionKind::Variable { name } if name == "ask_human" => {
                validate_arity(self, name, arguments.len(), 1, 3, expression.span);
                validate_named_options(
                    self,
                    arguments,
                    &["question", "response", "agent"],
                    expression.span,
                );
                if !in_await {
                    self.error(
                        "effect `ask_human` must be awaited".to_string(),
                        Some(expression.span),
                    );
                }
            }
            ExpressionKind::Variable { name } if builtin_arity(name).is_some() => {
                let (minimum, maximum, effectful) = builtin_arity(name).expect("checked");
                validate_arity(
                    self,
                    name,
                    arguments.len(),
                    minimum,
                    maximum,
                    expression.span,
                );
                if effectful != in_await {
                    let message = if effectful {
                        format!("effect `{name}` must be awaited")
                    } else {
                        format!("pure builtin `{name}` cannot be awaited")
                    };
                    self.error(message, Some(expression.span));
                }
            }
            ExpressionKind::Member { value, name } => {
                if let Some(template_name) = agent_template_of(value, scopes, &self.program.agents)
                {
                    if name == "set_access" {
                        validate_arity(self, "set_access", arguments.len(), 1, 1, expression.span);
                        if !in_await {
                            self.error(
                                "Agent.set_access must be awaited".to_string(),
                                Some(expression.span),
                            );
                        }
                    } else if let Some(action) =
                        self.program.agents[&template_name].actions.get(name)
                    {
                        validate_parameters(
                            self,
                            &format!("{template_name}.{name}"),
                            &action.parameters,
                            arguments,
                            expression.span,
                        );
                        if !in_await {
                            self.error(
                                format!("Action `{template_name}.{name}` must be awaited"),
                                Some(expression.span),
                            );
                        }
                    } else {
                        self.error(
                            format!("Agent template `{template_name}` has no Action `{name}`"),
                            Some(expression.span),
                        );
                    }
                } else if name == "changes" {
                    validate_named_options(
                        self,
                        arguments,
                        &["after_cursor", "exclude_current_program"],
                        expression.span,
                    );
                    if !in_await {
                        self.error(
                            "project.changes must be awaited".to_string(),
                            Some(expression.span),
                        );
                    }
                } else if name == "set_access" {
                    validate_arity(self, "set_access", arguments.len(), 1, 1, expression.span);
                    if !in_await {
                        self.error(
                            "Agent.set_access must be awaited".to_string(),
                            Some(expression.span),
                        );
                    }
                } else {
                    let candidates = self
                        .program
                        .agents
                        .values()
                        .filter_map(|agent| agent.actions.get(name))
                        .collect::<Vec<_>>();
                    if candidates.is_empty() {
                        self.error(
                            "method calls are limited to Agent Actions, Agent.set_access, and project.changes"
                                .to_string(),
                            Some(expression.span),
                        );
                    } else if candidates
                        .iter()
                        .any(|candidate| candidate.parameters != candidates[0].parameters)
                    {
                        self.error(
                            format!(
                                "Action name `{name}` is ambiguous for a dynamically typed receiver"
                            ),
                            Some(expression.span),
                        );
                    } else {
                        validate_parameters(
                            self,
                            name,
                            &candidates[0].parameters,
                            arguments,
                            expression.span,
                        );
                        if !in_await {
                            self.error(
                                format!("Action `{name}` must be awaited"),
                                Some(expression.span),
                            );
                        }
                    }
                }
            }
            _ => self.error(
                "higher-order and dynamically selected calls are not supported".to_string(),
                Some(expression.span),
            ),
        }
    }

    fn error(&mut self, message: String, span: Option<Span>) {
        self.diagnostics.push(diagnostic(message, span));
    }
}

fn validate_parameters(
    validator: &mut Validator<'_>,
    name: &str,
    parameters: &[String],
    arguments: &[CallArgument],
    span: Span,
) {
    if arguments.iter().all(|argument| argument.name.is_none()) {
        validate_arity(
            validator,
            name,
            arguments.len(),
            parameters.len(),
            parameters.len(),
            span,
        );
        return;
    }
    let mut seen = BTreeSet::new();
    for argument in arguments {
        let Some(argument_name) = &argument.name else {
            continue;
        };
        if !parameters.contains(argument_name) {
            validator.error(
                format!("call to `{name}` has unknown argument `{argument_name}`"),
                Some(argument.span),
            );
        } else if !seen.insert(argument_name) {
            validator.error(
                format!("call to `{name}` repeats argument `{argument_name}`"),
                Some(argument.span),
            );
        }
    }
    for parameter in parameters {
        if !seen.contains(parameter) {
            validator.error(
                format!("call to `{name}` is missing argument `{parameter}`"),
                Some(span),
            );
        }
    }
}

fn validate_named_options(
    validator: &mut Validator<'_>,
    arguments: &[CallArgument],
    options: &[&str],
    span: Span,
) {
    let mut seen = BTreeSet::new();
    for argument in arguments {
        let Some(name) = &argument.name else {
            continue;
        };
        if !options.contains(&name.as_str()) {
            validator.error(format!("unknown option `{name}`"), Some(argument.span));
        } else if !seen.insert(name) {
            validator.error(format!("duplicate option `{name}`"), Some(argument.span));
        }
    }
    if arguments.len() > options.len() {
        validator.error("too many call arguments".to_string(), Some(span));
    }
}

fn validate_arity(
    validator: &mut Validator<'_>,
    name: &str,
    actual: usize,
    minimum: usize,
    maximum: usize,
    span: Span,
) {
    if actual < minimum || actual > maximum {
        let expected = if minimum == maximum {
            minimum.to_string()
        } else if maximum == usize::MAX {
            format!("at least {minimum}")
        } else {
            format!("{minimum}..={maximum}")
        };
        validator.error(
            format!("call to `{name}` expects {expected} arguments, got {actual}"),
            Some(span),
        );
    }
}

fn constructor_template(
    expression: &Expression,
    agents: &BTreeMap<String, AgentTemplate>,
) -> Option<String> {
    let ExpressionKind::Call { callee, .. } = &expression.kind else {
        return None;
    };
    let ExpressionKind::Variable { name } = &callee.kind else {
        return None;
    };
    agents.contains_key(name).then(|| name.clone())
}

fn agent_template_of(
    expression: &Expression,
    scopes: &[HashMap<String, Binding>],
    agents: &BTreeMap<String, AgentTemplate>,
) -> Option<String> {
    match &expression.kind {
        ExpressionKind::Variable { name } => lookup_binding(scopes, name)
            .and_then(|binding| binding.agent_template.clone())
            .or_else(|| agents.contains_key(name).then(|| name.clone())),
        _ => None,
    }
}

fn lookup_binding<'a>(scopes: &'a [HashMap<String, Binding>], name: &str) -> Option<&'a Binding> {
    scopes.iter().rev().find_map(|scope| scope.get(name))
}

fn is_builtin(name: &str) -> bool {
    builtin_arity(name).is_some()
}

fn builtin_arity(name: &str) -> Option<(usize, usize, bool)> {
    PURE_BUILTINS
        .iter()
        .find(|(candidate, _, _)| *candidate == name)
        .map(|(_, minimum, maximum)| (*minimum, *maximum, false))
        .or_else(|| {
            EFFECT_BUILTINS
                .iter()
                .find(|(candidate, _, _)| *candidate == name)
                .map(|(_, minimum, maximum)| (*minimum, *maximum, true))
        })
}

fn collect_named_calls_block(
    block: &Block,
    functions: &BTreeMap<String, Function>,
    output: &mut BTreeSet<String>,
) {
    walk_block_expressions(block, &mut |expression| {
        if let ExpressionKind::Call { callee, .. } = &expression.kind
            && let ExpressionKind::Variable { name } = &callee.kind
            && functions.contains_key(name)
        {
            output.insert(name.clone());
        }
    });
}

fn block_contains_await(block: &Block) -> bool {
    let mut found = false;
    walk_block_expressions(block, &mut |expression| {
        found |= matches!(expression.kind, ExpressionKind::Await { .. });
    });
    found
}

fn block_contains_expression_await(expression: &Expression) -> bool {
    let mut found = false;
    walk_expression(expression, &mut |expression| {
        found |= matches!(expression.kind, ExpressionKind::Await { .. });
    });
    found
}

fn expression_guarantees_await(expression: &Expression) -> bool {
    match &expression.kind {
        ExpressionKind::Await { .. } => true,
        ExpressionKind::List { values } => values.iter().any(expression_guarantees_await),
        ExpressionKind::Object { fields } => fields
            .iter()
            .any(|(_, value)| expression_guarantees_await(value)),
        ExpressionKind::Unary { value, .. } | ExpressionKind::Member { value, .. } => {
            expression_guarantees_await(value)
        }
        ExpressionKind::Index { value, index } => {
            expression_guarantees_await(value) || expression_guarantees_await(index)
        }
        ExpressionKind::Call { callee, arguments } => {
            expression_guarantees_await(callee)
                || arguments
                    .iter()
                    .any(|argument| expression_guarantees_await(&argument.value))
        }
        ExpressionKind::Binary {
            left,
            operator,
            right,
        } => {
            expression_guarantees_await(left)
                || (!matches!(
                    operator,
                    BinaryOperator::And | BinaryOperator::Or | BinaryOperator::Coalesce
                ) && expression_guarantees_await(right))
        }
        ExpressionKind::Parallel { .. } => false,
        ExpressionKind::ParallelFor { iterable, .. } => expression_guarantees_await(iterable),
        ExpressionKind::Literal { .. } | ExpressionKind::Variable { .. } => false,
    }
}

fn walk_block_expressions(block: &Block, visitor: &mut impl FnMut(&Expression)) {
    for statement in &block.statements {
        walk_statement_expressions(statement, visitor);
    }
    if let Some(tail) = &block.tail {
        walk_expression(tail, visitor);
    }
}

fn walk_statement_expressions(statement: &Statement, visitor: &mut impl FnMut(&Expression)) {
    match &statement.kind {
        StatementKind::Let { value, .. }
        | StatementKind::Assign { value, .. }
        | StatementKind::Expression { expression: value }
        | StatementKind::Return { value } => walk_expression(value, visitor),
        StatementKind::If {
            condition,
            then_block,
            else_block,
        } => {
            walk_expression(condition, visitor);
            walk_block_expressions(then_block, visitor);
            if let Some(block) = else_block {
                walk_block_expressions(block, visitor);
            }
        }
        StatementKind::While { condition, body } => {
            walk_expression(condition, visitor);
            walk_block_expressions(body, visitor);
        }
        StatementKind::Loop { body } | StatementKind::For { body, .. } => {
            if let StatementKind::For { iterable, .. } = &statement.kind {
                walk_expression(iterable, visitor);
            }
            walk_block_expressions(body, visitor);
        }
        StatementKind::Match { value, arms } => {
            walk_expression(value, visitor);
            for arm in arms {
                match &arm.body {
                    MatchBody::Block(block) => walk_block_expressions(block, visitor),
                    MatchBody::Statement(statement) => {
                        walk_statement_expressions(statement, visitor)
                    }
                    MatchBody::Expression(expression) => walk_expression(expression, visitor),
                }
            }
        }
        StatementKind::Break | StatementKind::Continue => {}
    }
}

fn walk_expression(expression: &Expression, visitor: &mut impl FnMut(&Expression)) {
    visitor(expression);
    match &expression.kind {
        ExpressionKind::Literal { .. } | ExpressionKind::Variable { .. } => {}
        ExpressionKind::List { values } => {
            for value in values {
                walk_expression(value, visitor);
            }
        }
        ExpressionKind::Object { fields } => {
            for (_, value) in fields {
                walk_expression(value, visitor);
            }
        }
        ExpressionKind::Unary { value, .. }
        | ExpressionKind::Member { value, .. }
        | ExpressionKind::Await { value } => walk_expression(value, visitor),
        ExpressionKind::Binary { left, right, .. } => {
            walk_expression(left, visitor);
            walk_expression(right, visitor);
        }
        ExpressionKind::Index { value, index } => {
            walk_expression(value, visitor);
            walk_expression(index, visitor);
        }
        ExpressionKind::Call { callee, arguments } => {
            walk_expression(callee, visitor);
            for argument in arguments {
                walk_expression(&argument.value, visitor);
            }
        }
        ExpressionKind::Parallel { branches } => {
            for branch in branches {
                walk_block_expressions(&branch.body, visitor);
            }
        }
        ExpressionKind::ParallelFor {
            iterable,
            key,
            body,
            ..
        } => {
            walk_expression(iterable, visitor);
            walk_expression(key, visitor);
            walk_block_expressions(body, visitor);
        }
    }
}

#[derive(Clone, Copy)]
struct FlowState {
    awaited: bool,
    control: FlowControl,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum FlowControl {
    Normal,
    Continue,
    Break,
    Return,
}

fn back_edges_await(block: &Block, already_awaited: bool) -> bool {
    flow_block(
        block,
        vec![FlowState {
            awaited: already_awaited,
            control: FlowControl::Normal,
        }],
    )
    .into_iter()
    .filter(|state| matches!(state.control, FlowControl::Normal | FlowControl::Continue))
    .all(|state| state.awaited)
}

fn flow_block(block: &Block, mut states: Vec<FlowState>) -> Vec<FlowState> {
    for statement in &block.statements {
        let mut next = Vec::new();
        for state in states {
            if state.control == FlowControl::Normal {
                next.extend(flow_statement(statement, state));
            } else {
                next.push(state);
            }
        }
        states = next;
    }
    if let Some(tail) = &block.tail {
        for state in &mut states {
            if state.control == FlowControl::Normal {
                state.awaited |= expression_guarantees_await(tail);
            }
        }
    }
    states
}

fn flow_statement(statement: &Statement, mut state: FlowState) -> Vec<FlowState> {
    match &statement.kind {
        StatementKind::Let { value, .. }
        | StatementKind::Assign { value, .. }
        | StatementKind::Expression { expression: value } => {
            state.awaited |= expression_guarantees_await(value);
            vec![state]
        }
        StatementKind::Return { value } => {
            state.awaited |= expression_guarantees_await(value);
            state.control = FlowControl::Return;
            vec![state]
        }
        StatementKind::Break => {
            state.control = FlowControl::Break;
            vec![state]
        }
        StatementKind::Continue => {
            state.control = FlowControl::Continue;
            vec![state]
        }
        StatementKind::If {
            condition,
            then_block,
            else_block,
        } => {
            state.awaited |= expression_guarantees_await(condition);
            let mut values = flow_block(then_block, vec![state]);
            values.extend(
                else_block
                    .as_ref()
                    .map_or_else(|| vec![state], |block| flow_block(block, vec![state])),
            );
            values
        }
        StatementKind::Match { value, arms } => {
            state.awaited |= expression_guarantees_await(value);
            let mut values = Vec::new();
            for arm in arms {
                values.extend(match &arm.body {
                    MatchBody::Block(block) => flow_block(block, vec![state]),
                    MatchBody::Statement(statement) => flow_statement(statement, state),
                    MatchBody::Expression(expression) => vec![FlowState {
                        awaited: state.awaited || expression_guarantees_await(expression),
                        ..state
                    }],
                });
            }
            values
        }
        StatementKind::While { condition, body: _ } => {
            state.awaited |= expression_guarantees_await(condition);
            vec![state]
        }
        StatementKind::Loop { body } => {
            let _ = body;
            vec![state]
        }
        StatementKind::For { iterable, body, .. } => {
            let _ = body;
            state.awaited |= expression_guarantees_await(iterable);
            vec![state]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE: &str = r#"
version 1;
schema Decision = object { message: string, status: enum["active", "complete"] };
agent Worker {
  role = "worker";
  system = "work";
  access = workspace;
  action work(objective) { result = Decision; finalize = if_needed; prompt = "do it"; }
}
workflow goal {
  name = "Goal";
  description = "Keep working";
  request = required;
  params {
    topic: string(title = "Topic");
    title?: string(default = "Goal", title = "Title");
  }
  run(ctx) {
    let worker = Worker(key = "main", name = ctx.params.title);
    loop {
      let decision = await worker.work(objective = ctx.request);
      match decision.status {
        "active" => continue,
        "complete" => return decision.message,
        _ => fail("invalid status"),
      }
    }
  }
}
"#;

    #[test]
    fn compiles_and_hashes_canonical_ir() {
        let compiled = compile_source(SOURCE, &BTreeSet::new()).expect("source compiles");
        assert_eq!(compiled.manifest.slug, "goal");
        assert_eq!(compiled.manifest.language_version, 1);
        assert_eq!(
            compiled.manifest.params_schema["required"],
            serde_json::json!(["topic"])
        );
        assert_eq!(compiled.ir_sha256.len(), 64);
        let with_space = SOURCE.replace("version 1;", "version    1;");
        let second = compile_source(&with_space, &BTreeSet::new()).expect("source compiles");
        assert_eq!(compiled.ir_sha256, second.ir_sha256);
    }

    #[test]
    fn rejects_recursion_and_loop_without_await() {
        let source = r#"
version 1;
fn recurse(value) { return recurse(value); }
workflow bad {
  name = "Bad"; description = "Bad"; request = required;
  run(ctx) { loop { let value = recurse(ctx.request); } }
}
"#;
        let validation = compile_source(source, &BTreeSet::new()).expect_err("must reject");
        assert!(
            validation
                .diagnostics
                .iter()
                .any(|item| item.message.contains("recursive"))
        );
        assert!(
            validation
                .diagnostics
                .iter()
                .any(|item| item.message.contains("back edge"))
        );
        assert!(
            validation
                .diagnostics
                .iter()
                .all(|item| item.line.is_some() && item.column.is_some())
        );
    }

    #[test]
    fn rejects_scope_arity_mutation_and_parallel_key_errors() {
        let source = r#"
version 1;
agent Worker {
  role = "worker"; system = ""; access = model_only;
  action run(value) { tools = []; prompt = "run"; }
}
workflow invalid {
  name = "Invalid"; description = "Invalid"; request = required;
  params {}
  run(ctx) {
    let worker = Worker(key = "main");
    let fixed = parallel { same => 1, same => 2 };
    let dynamic = parallel for value in [1] key await wait(seconds = 1) { value };
    let immutable = 1;
    immutable = 2;
    await worker.run();
    return missing;
  }
}
"#;
        let validation = compile_source(source, &BTreeSet::new()).expect_err("must reject");
        for expected in [
            "duplicate parallel branch",
            "parallel keys must be pure",
            "immutable",
            "expects 1 arguments, got 0",
            "undefined name `missing`",
        ] {
            assert!(
                validation
                    .diagnostics
                    .iter()
                    .any(|item| item.message.contains(expected)),
                "missing diagnostic {expected}: {:?}",
                validation.diagnostics
            );
        }
    }

    #[test]
    fn human_response_schema_is_a_compile_time_language_boundary() {
        let valid = r#"
version 1;
schema HumanText = string(min_len = 1);
agent Worker { role = "worker"; system = ""; access = model_only; }
workflow interactive {
  name = "Interactive"; description = "Ask once"; request = none;
  params {}
  run(ctx) {
    let worker = Worker(key = "main");
    return await ask_human(question = "Continue?", response = HumanText, agent = worker);
  }
}
"#;
        compile_source(valid, &BTreeSet::new()).expect("named response schema should compile");

        let raw_json = valid.replace(
            "response = HumanText",
            "response_schema = {type: \"string\"}",
        );
        let validation = compile_source(&raw_json, &BTreeSet::new())
            .expect_err("raw response schemas should be rejected");
        assert!(
            validation
                .diagnostics
                .iter()
                .any(|item| item.message.contains("unknown option `response_schema`"))
        );

        let missing = valid.replace("response = HumanText", "response = MissingSchema");
        let validation = compile_source(&missing, &BTreeSet::new())
            .expect_err("unknown response schemas should be rejected");
        assert!(validation.diagnostics.iter().any(|item| {
            item.message
                .contains("ask_human references unknown schema `MissingSchema`")
        }));
    }
}
