use super::ast::*;
use super::lexer::Span;
use super::lexer::Token;
use super::lexer::TokenKind;
use super::schema::BoundarySchema;
use super::schema::SchemaField;
use super::schema::SchemaKind;
use papermachine_protocol::AccessPreset;
use papermachine_protocol::ReasoningEffort;
use papermachine_protocol::WebSearchContextSize;
use papermachine_protocol::WorkflowRequestMode;
use serde_json::Map;
use serde_json::Number;
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug)]
pub struct ParseError {
    pub message: String,
    pub span: Span,
}

pub fn parse(tokens: Vec<Token>) -> Result<Program, ParseError> {
    Parser::new(tokens).program()
}

struct Parser {
    tokens: Vec<Token>,
    cursor: usize,
    next_node_id: NodeId,
    schemas: BTreeMap<String, BoundarySchema>,
}

impl Parser {
    fn new(tokens: Vec<Token>) -> Self {
        Self {
            tokens,
            cursor: 0,
            next_node_id: 1,
            schemas: BTreeMap::new(),
        }
    }

    fn program(mut self) -> Result<Program, ParseError> {
        self.expect_keyword("version")?;
        let version = self.expect_int()?;
        if version != 1 {
            return Err(self.error_here("only Workflow Language version 1 is supported"));
        }
        self.expect_simple(TokenKind::Semicolon, "`;`")?;

        let mut agents = BTreeMap::new();
        let mut functions = BTreeMap::new();
        let mut workflow = None;
        while !self.at(&TokenKind::Eof) {
            if self.take_keyword("schema") {
                let (name, schema) = self.schema_declaration()?;
                if self.schemas.insert(name.clone(), schema).is_some() {
                    return Err(self.error_here(&format!("duplicate schema `{name}`")));
                }
            } else if self.take_keyword("agent") {
                let agent = self.agent()?;
                if agents.insert(agent.name.clone(), agent).is_some() {
                    return Err(self.error_here("duplicate Agent template"));
                }
            } else if self.take_keyword("fn") {
                let function = self.function()?;
                if functions.insert(function.name.clone(), function).is_some() {
                    return Err(self.error_here("duplicate function"));
                }
            } else if self.take_keyword("workflow") {
                if workflow.is_some() {
                    return Err(self.error_here("source must contain exactly one workflow"));
                }
                workflow = Some(self.workflow()?);
            } else {
                return Err(
                    self.error_here("expected `schema`, `agent`, `fn`, or `workflow` declaration")
                );
            }
        }
        let workflow = workflow.ok_or_else(|| self.error_here("source must define a workflow"))?;
        Ok(Program {
            version: u32::try_from(version).unwrap_or(1),
            schemas: self.schemas,
            agents,
            functions,
            workflow,
        })
    }

    fn schema_declaration(&mut self) -> Result<(String, BoundarySchema), ParseError> {
        let name = self.expect_ident()?;
        self.expect_simple(TokenKind::Equal, "`=`")?;
        let schema = self.schema_expression()?;
        self.expect_simple(TokenKind::Semicolon, "`;`")?;
        Ok((name, schema))
    }

    fn schema_expression(&mut self) -> Result<BoundarySchema, ParseError> {
        let name = self.expect_ident()?;
        if name == "object" && self.at(&TokenKind::LBrace) {
            return self.object_schema();
        }
        if name == "enum" {
            return self.enum_schema();
        }
        if let Some(schema) = self.schemas.get(&name).cloned() {
            if self.at(&TokenKind::LParen) {
                return Err(self.error_here("named schemas do not accept options"));
            }
            return Ok(schema);
        }
        let mut schema = match name.as_str() {
            "any" => BoundarySchema::new(SchemaKind::Any),
            "string" => BoundarySchema::new(SchemaKind::String),
            "bool" => BoundarySchema::new(SchemaKind::Bool),
            "int" => BoundarySchema::new(SchemaKind::Int),
            "number" => BoundarySchema::new(SchemaKind::Number),
            "model_profile" => BoundarySchema {
                format: Some("model-profile".to_string()),
                ..BoundarySchema::new(SchemaKind::String)
            },
            "access" => BoundarySchema::new(SchemaKind::Enum {
                values: ["model_only", "read_only", "workspace", "full_access"]
                    .into_iter()
                    .map(|value| Value::String(value.to_string()))
                    .collect(),
            }),
            "list" | "map" => {
                self.expect_simple(TokenKind::LParen, "`(`")?;
                let child = self.schema_expression()?;
                let kind = if name == "list" {
                    SchemaKind::List {
                        items: Box::new(child),
                    }
                } else {
                    SchemaKind::Map {
                        values: Box::new(child),
                    }
                };
                let mut value = BoundarySchema::new(kind);
                if self.take(&TokenKind::Comma) {
                    self.schema_options(&mut value)?;
                }
                self.expect_simple(TokenKind::RParen, "`)`")?;
                return Ok(value);
            }
            _ => return Err(self.error_here(&format!("unknown schema `{name}`"))),
        };
        if self.take(&TokenKind::LParen) {
            if !self.at(&TokenKind::RParen) {
                self.schema_options(&mut schema)?;
            }
            self.expect_simple(TokenKind::RParen, "`)`")?;
        }
        self.validate_schema_default(&schema)?;
        Ok(schema)
    }

    fn object_schema(&mut self) -> Result<BoundarySchema, ParseError> {
        self.expect_simple(TokenKind::LBrace, "`{`")?;
        let mut fields = Vec::new();
        while !self.take(&TokenKind::RBrace) {
            let name = self.expect_ident()?;
            let optional = self.take(&TokenKind::Question);
            self.expect_simple(TokenKind::Colon, "`:`")?;
            let schema = self.schema_expression()?;
            if fields.iter().any(|field: &SchemaField| field.name == name) {
                return Err(self.error_here(&format!("duplicate schema field `{name}`")));
            }
            fields.push(SchemaField {
                name,
                schema,
                optional,
            });
            if !self.take(&TokenKind::Comma) {
                self.take(&TokenKind::Semicolon);
            }
        }
        Ok(BoundarySchema::object(fields))
    }

    fn enum_schema(&mut self) -> Result<BoundarySchema, ParseError> {
        self.expect_simple(TokenKind::LBracket, "`[`")?;
        let mut values = Vec::new();
        while !self.take(&TokenKind::RBracket) {
            values.push(self.literal_value()?);
            if !self.take(&TokenKind::Comma) {
                self.expect_simple(TokenKind::RBracket, "`]`")?;
                break;
            }
        }
        if values.is_empty() {
            return Err(self.error_here("enum schema requires at least one value"));
        }
        let kind = json_kind(&values[0]);
        if values.iter().any(|value| json_kind(value) != kind) {
            return Err(self.error_here("enum schema values must use one scalar kind"));
        }
        Ok(BoundarySchema::new(SchemaKind::Enum { values }))
    }

    fn schema_options(&mut self, schema: &mut BoundarySchema) -> Result<(), ParseError> {
        loop {
            let name = self.expect_ident()?;
            self.expect_simple(TokenKind::Equal, "`=`")?;
            let value = self.literal_value()?;
            match name.as_str() {
                "default" => schema.default = Some(value),
                "title" => schema.title = Some(expect_json_string(value, "title", self)?),
                "description" => {
                    schema.description = Some(expect_json_string(value, "description", self)?)
                }
                "min" => schema.minimum = Some(expect_json_number(value, "min", self)?),
                "max" => schema.maximum = Some(expect_json_number(value, "max", self)?),
                "min_len" => schema.min_length = Some(expect_json_usize(value, "min_len", self)?),
                "max_len" => schema.max_length = Some(expect_json_usize(value, "max_len", self)?),
                _ => return Err(self.error_here(&format!("unknown schema option `{name}`"))),
            }
            if !self.take(&TokenKind::Comma) {
                break;
            }
            if self.at(&TokenKind::RParen) {
                break;
            }
        }
        self.validate_schema_default(schema)
    }

    fn validate_schema_default(&self, schema: &BoundarySchema) -> Result<(), ParseError> {
        if let Some(default) = &schema.default {
            schema
                .validate(default, "default")
                .map_err(|message| self.error_here(&message))?;
        }
        Ok(())
    }

    fn agent(&mut self) -> Result<AgentTemplate, ParseError> {
        let start = self.previous_span();
        let name = self.expect_ident()?;
        self.expect_simple(TokenKind::LBrace, "`{`")?;
        let mut role = String::new();
        let mut system = String::new();
        let mut model = String::new();
        let mut skills = Vec::new();
        let mut access = AccessPreset::Workspace;
        let mut actions = BTreeMap::new();
        while !self.take(&TokenKind::RBrace) {
            if self.take_keyword("action") {
                let action = self.action()?;
                if actions.insert(action.name.clone(), action).is_some() {
                    return Err(self.error_here("duplicate Action declaration"));
                }
                continue;
            }
            let field = self.expect_ident()?;
            self.expect_simple(TokenKind::Equal, "`=`")?;
            match field.as_str() {
                "role" => role = self.expect_string()?,
                "system" => system = self.expect_string()?,
                "model" => model = self.expect_string()?,
                "skills" => skills = self.string_list()?,
                "access" => access = self.access_preset()?,
                _ => return Err(self.error_here(&format!("unknown Agent field `{field}`"))),
            }
            self.expect_simple(TokenKind::Semicolon, "`;`")?;
        }
        Ok(AgentTemplate {
            name,
            role,
            system,
            model,
            skills,
            access,
            actions,
            span: merge_span(start, self.previous_span()),
        })
    }

    fn action(&mut self) -> Result<ActionDefinition, ParseError> {
        let start = self.previous_span();
        let name = self.expect_ident()?;
        let parameters = self.parameter_names()?;
        self.expect_simple(TokenKind::LBrace, "`{`")?;
        let mut prompt = None;
        let mut tools = None;
        let mut search_context = None;
        let mut reasoning_effort = None;
        let mut finalize = FinalizePolicy::None;
        let mut result = None;
        while !self.take(&TokenKind::RBrace) {
            let field = self.expect_ident()?;
            self.expect_simple(TokenKind::Equal, "`=`")?;
            match field.as_str() {
                "prompt" => prompt = Some(self.expect_string()?),
                "tools" => {
                    if self.take_keyword("default") {
                        tools = None;
                    } else {
                        tools = Some(self.string_list()?);
                    }
                }
                "search_context" => {
                    search_context = Some(match self.expect_ident()?.as_str() {
                        "low" => WebSearchContextSize::Low,
                        "medium" => WebSearchContextSize::Medium,
                        "high" => WebSearchContextSize::High,
                        _ => return Err(self.error_here("invalid search_context")),
                    });
                }
                "reasoning_effort" => {
                    reasoning_effort = Some(match self.expect_ident()?.as_str() {
                        "none" => ReasoningEffort::None,
                        "low" => ReasoningEffort::Low,
                        "medium" => ReasoningEffort::Medium,
                        "high" => ReasoningEffort::High,
                        "xhigh" => ReasoningEffort::Xhigh,
                        "max" => ReasoningEffort::Max,
                        _ => return Err(self.error_here("invalid reasoning_effort")),
                    });
                }
                "finalize" => {
                    finalize = match self.expect_ident()?.as_str() {
                        "none" => FinalizePolicy::None,
                        "if_needed" => FinalizePolicy::IfNeeded,
                        "after_search" => FinalizePolicy::AfterSearch,
                        _ => return Err(self.error_here("invalid finalize policy")),
                    };
                }
                "result" => result = Some(self.schema_expression()?),
                _ => return Err(self.error_here(&format!("unknown Action field `{field}`"))),
            }
            self.expect_simple(TokenKind::Semicolon, "`;`")?;
        }
        let prompt = prompt.ok_or_else(|| self.error_here("Action requires a prompt"))?;
        if finalize == FinalizePolicy::IfNeeded
            && result
                .as_ref()
                .is_none_or(|schema| !matches!(schema.top_level_kind(), "object" | "array"))
        {
            return Err(
                self.error_here("finalize = if_needed requires an object or list result schema")
            );
        }
        Ok(ActionDefinition {
            name,
            parameters,
            prompt,
            tools,
            search_context,
            reasoning_effort,
            finalize,
            result,
            span: merge_span(start, self.previous_span()),
        })
    }

    fn function(&mut self) -> Result<Function, ParseError> {
        let start = self.previous_span();
        let name = self.expect_ident()?;
        let parameters = self.parameter_names()?;
        let body = self.block()?;
        Ok(Function {
            name,
            parameters,
            span: merge_span(start, body.span),
            body,
        })
    }

    fn workflow(&mut self) -> Result<Workflow, ParseError> {
        let start = self.previous_span();
        let declaration_name = self.expect_ident()?;
        self.expect_simple(TokenKind::LBrace, "`{`")?;
        let mut slug = declaration_name.replace('_', "-");
        let mut name = None;
        let mut description = None;
        let mut request_mode = WorkflowRequestMode::Required;
        let mut params = Vec::new();
        let mut run = None;
        while !self.take(&TokenKind::RBrace) {
            if self.take_keyword("params") {
                if !params.is_empty() {
                    return Err(self.error_here("workflow may declare params only once"));
                }
                params = self.params()?;
                continue;
            }
            if self.take_keyword("run") {
                if run.is_some() {
                    return Err(self.error_here("workflow may declare run only once"));
                }
                let parameters = self.parameter_names()?;
                if parameters.len() != 1 {
                    return Err(self.error_here("run requires exactly one context parameter"));
                }
                run = Some((parameters[0].clone(), self.block()?));
                continue;
            }
            let field = self.expect_ident()?;
            self.expect_simple(TokenKind::Equal, "`=`")?;
            match field.as_str() {
                "slug" => slug = self.expect_string()?,
                "name" => name = Some(self.expect_string()?),
                "description" => description = Some(self.expect_string()?),
                "request" => {
                    request_mode = match self.expect_ident()?.as_str() {
                        "required" => WorkflowRequestMode::Required,
                        "none" => WorkflowRequestMode::None,
                        _ => return Err(self.error_here("request must be required or none")),
                    }
                }
                _ => return Err(self.error_here(&format!("unknown workflow field `{field}`"))),
            }
            self.expect_simple(TokenKind::Semicolon, "`;`")?;
        }
        if !valid_slug(&slug) {
            return Err(self.error_here("workflow slug must use lowercase kebab-case"));
        }
        let (run_parameter, body) = run.ok_or_else(|| self.error_here("workflow requires run"))?;
        Ok(Workflow {
            slug,
            name: name.ok_or_else(|| self.error_here("workflow requires name"))?,
            description: description
                .ok_or_else(|| self.error_here("workflow requires description"))?,
            request_mode,
            params,
            run_parameter,
            span: merge_span(start, self.previous_span()),
            body,
        })
    }

    fn params(&mut self) -> Result<Vec<Parameter>, ParseError> {
        self.expect_simple(TokenKind::LBrace, "`{`")?;
        let mut params = Vec::new();
        while !self.take(&TokenKind::RBrace) {
            let span = self.current().span;
            let name = self.expect_ident()?;
            self.expect_simple(TokenKind::Equal, "`=`")?;
            let schema = self.schema_expression()?;
            self.expect_simple(TokenKind::Semicolon, "`;`")?;
            if params.iter().any(|value: &Parameter| value.name == name) {
                return Err(self.error_here(&format!("duplicate parameter `{name}`")));
            }
            params.push(Parameter { name, schema, span });
        }
        Ok(params)
    }

    fn parameter_names(&mut self) -> Result<Vec<String>, ParseError> {
        self.expect_simple(TokenKind::LParen, "`(`")?;
        let mut values = Vec::new();
        while !self.take(&TokenKind::RParen) {
            let name = self.expect_ident()?;
            if values.contains(&name) {
                return Err(self.error_here(&format!("duplicate parameter `{name}`")));
            }
            values.push(name);
            if !self.take(&TokenKind::Comma) {
                self.expect_simple(TokenKind::RParen, "`)`")?;
                break;
            }
        }
        Ok(values)
    }

    fn block(&mut self) -> Result<Block, ParseError> {
        let start = self.current().span;
        self.expect_simple(TokenKind::LBrace, "`{`")?;
        let id = self.node_id();
        let mut statements = Vec::new();
        let mut tail = None;
        while !self.take(&TokenKind::RBrace) {
            if self.at_statement_keyword() || self.at_assignment() {
                statements.push(self.statement()?);
                continue;
            }
            let expression = self.expression(0)?;
            if self.take(&TokenKind::Semicolon) {
                let span = expression.span;
                statements.push(Statement {
                    id: self.node_id(),
                    kind: StatementKind::Expression { expression },
                    span,
                });
            } else if self.at(&TokenKind::RBrace) {
                tail = Some(Box::new(expression));
            } else {
                return Err(self.error_here("expected `;` or `}` after expression"));
            }
        }
        Ok(Block {
            id,
            statements,
            tail,
            span: merge_span(start, self.previous_span()),
        })
    }

    fn statement(&mut self) -> Result<Statement, ParseError> {
        let start = self.current().span;
        let id = self.node_id();
        let kind = if self.take_keyword("let") || self.take_keyword("var") {
            let mutable = self.previous_ident() == Some("var");
            let name = self.expect_ident()?;
            self.expect_simple(TokenKind::Equal, "`=`")?;
            let value = self.expression(0)?;
            self.expect_simple(TokenKind::Semicolon, "`;`")?;
            StatementKind::Let {
                name,
                mutable,
                value,
            }
        } else if self.take_keyword("if") {
            let condition = self.expression(0)?;
            let then_block = self.block()?;
            let else_block = if self.take_keyword("else") {
                Some(self.block()?)
            } else {
                None
            };
            StatementKind::If {
                condition,
                then_block,
                else_block,
            }
        } else if self.take_keyword("while") {
            let condition = self.expression(0)?;
            let body = self.block()?;
            StatementKind::While { condition, body }
        } else if self.take_keyword("loop") {
            StatementKind::Loop {
                body: self.block()?,
            }
        } else if self.take_keyword("for") {
            let binding = self.expect_ident()?;
            self.expect_keyword("in")?;
            let iterable = self.expression(0)?;
            let body = self.block()?;
            StatementKind::For {
                binding,
                iterable,
                body,
            }
        } else if self.take_keyword("match") {
            let value = self.expression(0)?;
            StatementKind::Match {
                value,
                arms: self.match_arms()?,
            }
        } else if self.take_keyword("break") {
            self.expect_simple(TokenKind::Semicolon, "`;`")?;
            StatementKind::Break
        } else if self.take_keyword("continue") {
            self.expect_simple(TokenKind::Semicolon, "`;`")?;
            StatementKind::Continue
        } else if self.take_keyword("return") {
            let value = self.expression(0)?;
            self.expect_simple(TokenKind::Semicolon, "`;`")?;
            StatementKind::Return { value }
        } else {
            let name = self.expect_ident()?;
            let operator = match &self.current().kind {
                TokenKind::Equal => AssignOperator::Set,
                TokenKind::PlusEqual => AssignOperator::Add,
                TokenKind::MinusEqual => AssignOperator::Subtract,
                TokenKind::StarEqual => AssignOperator::Multiply,
                TokenKind::SlashEqual => AssignOperator::Divide,
                _ => return Err(self.error_here("expected assignment operator")),
            };
            self.advance();
            let value = self.expression(0)?;
            self.expect_simple(TokenKind::Semicolon, "`;`")?;
            StatementKind::Assign {
                name,
                operator,
                value,
            }
        };
        Ok(Statement {
            id,
            kind,
            span: merge_span(start, self.previous_span()),
        })
    }

    fn match_arms(&mut self) -> Result<Vec<MatchArm>, ParseError> {
        self.expect_simple(TokenKind::LBrace, "`{`")?;
        let mut arms = Vec::new();
        while !self.take(&TokenKind::RBrace) {
            let start = self.current().span;
            let mut patterns = Vec::new();
            loop {
                if self.take_keyword("_") {
                    patterns.push(Pattern::Wildcard);
                } else {
                    patterns.push(Pattern::Literal(self.literal_value()?));
                }
                if !self.take(&TokenKind::Pipe) {
                    break;
                }
            }
            self.expect_simple(TokenKind::FatArrow, "`=>`")?;
            let body = if self.at(&TokenKind::LBrace) {
                MatchBody::Block(self.block()?)
            } else if self.take_keyword("continue") {
                MatchBody::Statement(Box::new(Statement {
                    id: self.node_id(),
                    kind: StatementKind::Continue,
                    span: self.previous_span(),
                }))
            } else if self.take_keyword("break") {
                MatchBody::Statement(Box::new(Statement {
                    id: self.node_id(),
                    kind: StatementKind::Break,
                    span: self.previous_span(),
                }))
            } else if self.take_keyword("return") {
                let value = self.expression(0)?;
                MatchBody::Statement(Box::new(Statement {
                    id: self.node_id(),
                    kind: StatementKind::Return { value },
                    span: start,
                }))
            } else {
                MatchBody::Expression(self.expression(0)?)
            };
            self.take(&TokenKind::Comma);
            arms.push(MatchArm {
                patterns,
                body,
                span: merge_span(start, self.previous_span()),
            });
        }
        Ok(arms)
    }

    fn expression(&mut self, minimum_binding_power: u8) -> Result<Expression, ParseError> {
        let start = self.current().span;
        let mut left = self.prefix()?;
        loop {
            if self.take(&TokenKind::Dot) {
                let name = self.expect_ident()?;
                left = Expression {
                    id: self.node_id(),
                    span: merge_span(start, self.previous_span()),
                    kind: ExpressionKind::Member {
                        value: Box::new(left),
                        name,
                    },
                };
                continue;
            }
            if self.take(&TokenKind::LBracket) {
                let index = self.expression(0)?;
                self.expect_simple(TokenKind::RBracket, "`]`")?;
                left = Expression {
                    id: self.node_id(),
                    span: merge_span(start, self.previous_span()),
                    kind: ExpressionKind::Index {
                        value: Box::new(left),
                        index: Box::new(index),
                    },
                };
                continue;
            }
            if self.at(&TokenKind::LParen) {
                let arguments = self.call_arguments()?;
                left = Expression {
                    id: self.node_id(),
                    span: merge_span(start, self.previous_span()),
                    kind: ExpressionKind::Call {
                        callee: Box::new(left),
                        arguments,
                    },
                };
                continue;
            }
            let Some((left_power, right_power, operator)) = self.binary_operator() else {
                break;
            };
            if left_power < minimum_binding_power {
                break;
            }
            self.advance();
            let right = self.expression(right_power)?;
            left = Expression {
                id: self.node_id(),
                span: merge_span(start, right.span),
                kind: ExpressionKind::Binary {
                    operator,
                    left: Box::new(left),
                    right: Box::new(right),
                },
            };
        }
        Ok(left)
    }

    fn prefix(&mut self) -> Result<Expression, ParseError> {
        let start = self.current().span;
        let id = self.node_id();
        let kind = if self.take_keyword("await") {
            ExpressionKind::Await {
                value: Box::new(self.expression(12)?),
            }
        } else if self.take_keyword("parallel") {
            return self.parallel_expression(start, id);
        } else if self.take(&TokenKind::Bang) {
            ExpressionKind::Unary {
                operator: UnaryOperator::Not,
                value: Box::new(self.expression(12)?),
            }
        } else if self.take(&TokenKind::Minus) {
            ExpressionKind::Unary {
                operator: UnaryOperator::Negate,
                value: Box::new(self.expression(12)?),
            }
        } else {
            match self.current().kind.clone() {
                TokenKind::String(value) => {
                    self.advance();
                    ExpressionKind::Literal {
                        value: Value::String(value),
                    }
                }
                TokenKind::Int(value) => {
                    self.advance();
                    ExpressionKind::Literal {
                        value: Value::Number(Number::from(value)),
                    }
                }
                TokenKind::Number(value) => {
                    self.advance();
                    ExpressionKind::Literal {
                        value: Value::Number(
                            Number::from_f64(value)
                                .ok_or_else(|| self.error_here("number literal must be finite"))?,
                        ),
                    }
                }
                TokenKind::Ident(value) if matches!(value.as_str(), "true" | "false" | "null") => {
                    self.advance();
                    ExpressionKind::Literal {
                        value: match value.as_str() {
                            "true" => Value::Bool(true),
                            "false" => Value::Bool(false),
                            _ => Value::Null,
                        },
                    }
                }
                TokenKind::Ident(name) => {
                    self.advance();
                    ExpressionKind::Variable { name }
                }
                TokenKind::LParen => {
                    self.advance();
                    let expression = self.expression(0)?;
                    self.expect_simple(TokenKind::RParen, "`)`")?;
                    return Ok(expression);
                }
                TokenKind::LBracket => return self.list_expression(start, id),
                TokenKind::LBrace => return self.object_expression(start, id),
                _ => return Err(self.error_here("expected expression")),
            }
        };
        Ok(Expression {
            id,
            kind,
            span: merge_span(start, self.previous_span()),
        })
    }

    fn list_expression(&mut self, start: Span, id: NodeId) -> Result<Expression, ParseError> {
        self.expect_simple(TokenKind::LBracket, "`[`")?;
        let mut values = Vec::new();
        while !self.take(&TokenKind::RBracket) {
            values.push(self.expression(0)?);
            if !self.take(&TokenKind::Comma) {
                self.expect_simple(TokenKind::RBracket, "`]`")?;
                break;
            }
        }
        Ok(Expression {
            id,
            kind: ExpressionKind::List { values },
            span: merge_span(start, self.previous_span()),
        })
    }

    fn object_expression(&mut self, start: Span, id: NodeId) -> Result<Expression, ParseError> {
        self.expect_simple(TokenKind::LBrace, "`{`")?;
        let mut fields = Vec::new();
        while !self.take(&TokenKind::RBrace) {
            let name = match self.current().kind.clone() {
                TokenKind::Ident(name) => {
                    self.advance();
                    name
                }
                TokenKind::String(name) => {
                    self.advance();
                    name
                }
                _ => return Err(self.error_here("object field must be an identifier or string")),
            };
            let value = if self.take(&TokenKind::Colon) {
                self.expression(0)?
            } else {
                Expression {
                    id: self.node_id(),
                    kind: ExpressionKind::Variable { name: name.clone() },
                    span: self.previous_span(),
                }
            };
            if fields.iter().any(|(field, _)| field == &name) {
                return Err(self.error_here(&format!("duplicate object field `{name}`")));
            }
            fields.push((name, value));
            if !self.take(&TokenKind::Comma) {
                self.expect_simple(TokenKind::RBrace, "`}`")?;
                break;
            }
        }
        Ok(Expression {
            id,
            kind: ExpressionKind::Object { fields },
            span: merge_span(start, self.previous_span()),
        })
    }

    fn parallel_expression(&mut self, start: Span, id: NodeId) -> Result<Expression, ParseError> {
        if self.take_keyword("for") {
            let binding = self.expect_ident()?;
            self.expect_keyword("in")?;
            let iterable = self.expression(0)?;
            self.expect_keyword("key")?;
            let key = self.expression(0)?;
            let body = self.block()?;
            return Ok(Expression {
                id,
                span: merge_span(start, body.span),
                kind: ExpressionKind::ParallelFor {
                    binding,
                    iterable: Box::new(iterable),
                    key: Box::new(key),
                    body,
                },
            });
        }
        self.expect_simple(TokenKind::LBrace, "`{`")?;
        let mut branches = Vec::new();
        while !self.take(&TokenKind::RBrace) {
            let branch_start = self.current().span;
            let name = self.expect_ident()?;
            self.expect_simple(TokenKind::FatArrow, "`=>`")?;
            let body = if self.at(&TokenKind::LBrace) {
                self.block()?
            } else {
                let expression = self.expression(0)?;
                Block {
                    id: self.node_id(),
                    statements: Vec::new(),
                    tail: Some(Box::new(expression.clone())),
                    span: expression.span,
                }
            };
            branches.push(ParallelBranch {
                name,
                span: merge_span(branch_start, body.span),
                body,
            });
            if !self.take(&TokenKind::Comma) {
                self.take(&TokenKind::Semicolon);
            }
        }
        Ok(Expression {
            id,
            kind: ExpressionKind::Parallel { branches },
            span: merge_span(start, self.previous_span()),
        })
    }

    fn call_arguments(&mut self) -> Result<Vec<CallArgument>, ParseError> {
        self.expect_simple(TokenKind::LParen, "`(`")?;
        let mut arguments = Vec::new();
        let mut saw_named = false;
        while !self.take(&TokenKind::RParen) {
            let start = self.current().span;
            let name = if matches!(self.current().kind, TokenKind::Ident(_))
                && self
                    .tokens
                    .get(self.cursor + 1)
                    .is_some_and(|token| token.kind == TokenKind::Equal)
            {
                let name = self.expect_ident()?;
                self.advance();
                saw_named = true;
                Some(name)
            } else {
                if saw_named {
                    return Err(
                        self.error_here("positional argument cannot follow named arguments")
                    );
                }
                None
            };
            let value = self.expression(0)?;
            arguments.push(CallArgument {
                name,
                span: merge_span(start, value.span),
                value,
            });
            if !self.take(&TokenKind::Comma) {
                self.expect_simple(TokenKind::RParen, "`)`")?;
                break;
            }
        }
        Ok(arguments)
    }

    fn binary_operator(&self) -> Option<(u8, u8, BinaryOperator)> {
        Some(match self.current().kind {
            TokenKind::Coalesce => (1, 2, BinaryOperator::Coalesce),
            TokenKind::OrOr => (2, 3, BinaryOperator::Or),
            TokenKind::AndAnd => (3, 4, BinaryOperator::And),
            TokenKind::EqualEqual => (4, 5, BinaryOperator::Equal),
            TokenKind::BangEqual => (4, 5, BinaryOperator::NotEqual),
            TokenKind::Less => (5, 6, BinaryOperator::Less),
            TokenKind::LessEqual => (5, 6, BinaryOperator::LessEqual),
            TokenKind::Greater => (5, 6, BinaryOperator::Greater),
            TokenKind::GreaterEqual => (5, 6, BinaryOperator::GreaterEqual),
            TokenKind::Plus => (6, 7, BinaryOperator::Add),
            TokenKind::Minus => (6, 7, BinaryOperator::Subtract),
            TokenKind::Star => (7, 8, BinaryOperator::Multiply),
            TokenKind::Slash => (7, 8, BinaryOperator::Divide),
            TokenKind::Percent => (7, 8, BinaryOperator::Remainder),
            _ => return None,
        })
    }

    fn literal_value(&mut self) -> Result<Value, ParseError> {
        match self.current().kind.clone() {
            TokenKind::String(value) => {
                self.advance();
                Ok(Value::String(value))
            }
            TokenKind::Int(value) => {
                self.advance();
                Ok(Value::Number(Number::from(value)))
            }
            TokenKind::Number(value) => {
                self.advance();
                Number::from_f64(value)
                    .map(Value::Number)
                    .ok_or_else(|| self.error_here("number must be finite"))
            }
            TokenKind::Ident(value) if value == "true" => {
                self.advance();
                Ok(Value::Bool(true))
            }
            TokenKind::Ident(value) if value == "false" => {
                self.advance();
                Ok(Value::Bool(false))
            }
            TokenKind::Ident(value) if value == "null" => {
                self.advance();
                Ok(Value::Null)
            }
            TokenKind::Ident(value) => {
                self.advance();
                Ok(Value::String(value))
            }
            TokenKind::LBracket => {
                self.advance();
                let mut values = Vec::new();
                while !self.take(&TokenKind::RBracket) {
                    values.push(self.literal_value()?);
                    if !self.take(&TokenKind::Comma) {
                        self.expect_simple(TokenKind::RBracket, "`]`")?;
                        break;
                    }
                }
                Ok(Value::Array(values))
            }
            TokenKind::LBrace => {
                self.advance();
                let mut values = Map::new();
                while !self.take(&TokenKind::RBrace) {
                    let key = match self.current().kind.clone() {
                        TokenKind::Ident(value) | TokenKind::String(value) => {
                            self.advance();
                            value
                        }
                        _ => return Err(self.error_here("literal object requires string keys")),
                    };
                    self.expect_simple(TokenKind::Colon, "`:`")?;
                    let value = self.literal_value()?;
                    if values.insert(key.clone(), value).is_some() {
                        return Err(self.error_here(&format!("duplicate literal key `{key}`")));
                    }
                    if !self.take(&TokenKind::Comma) {
                        self.expect_simple(TokenKind::RBrace, "`}`")?;
                        break;
                    }
                }
                Ok(Value::Object(values))
            }
            _ => Err(self.error_here("expected literal value")),
        }
    }

    fn string_list(&mut self) -> Result<Vec<String>, ParseError> {
        self.expect_simple(TokenKind::LBracket, "`[`")?;
        let mut values = Vec::new();
        while !self.take(&TokenKind::RBracket) {
            let value = self.expect_string()?;
            if values.contains(&value) {
                return Err(self.error_here("list contains a duplicate string"));
            }
            values.push(value);
            if !self.take(&TokenKind::Comma) {
                self.expect_simple(TokenKind::RBracket, "`]`")?;
                break;
            }
        }
        Ok(values)
    }

    fn access_preset(&mut self) -> Result<AccessPreset, ParseError> {
        match self.expect_ident()?.as_str() {
            "model_only" => Ok(AccessPreset::ModelOnly),
            "read_only" => Ok(AccessPreset::ReadOnly),
            "workspace" => Ok(AccessPreset::Workspace),
            "full_access" => Ok(AccessPreset::FullAccess),
            _ => Err(self.error_here("invalid access preset")),
        }
    }

    fn at_statement_keyword(&self) -> bool {
        matches!(
            self.current_ident(),
            Some(
                "let"
                    | "var"
                    | "if"
                    | "while"
                    | "loop"
                    | "for"
                    | "match"
                    | "break"
                    | "continue"
                    | "return"
            )
        )
    }

    fn at_assignment(&self) -> bool {
        matches!(self.current().kind, TokenKind::Ident(_))
            && self.tokens.get(self.cursor + 1).is_some_and(|token| {
                matches!(
                    token.kind,
                    TokenKind::Equal
                        | TokenKind::PlusEqual
                        | TokenKind::MinusEqual
                        | TokenKind::StarEqual
                        | TokenKind::SlashEqual
                )
            })
    }

    fn expect_keyword(&mut self, expected: &str) -> Result<(), ParseError> {
        if self.take_keyword(expected) {
            Ok(())
        } else {
            Err(self.error_here(&format!("expected `{expected}`")))
        }
    }

    fn take_keyword(&mut self, expected: &str) -> bool {
        if self.current_ident() == Some(expected) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn expect_ident(&mut self) -> Result<String, ParseError> {
        match self.current().kind.clone() {
            TokenKind::Ident(value) => {
                self.advance();
                Ok(value)
            }
            _ => Err(self.error_here("expected identifier")),
        }
    }

    fn expect_string(&mut self) -> Result<String, ParseError> {
        match self.current().kind.clone() {
            TokenKind::String(value) => {
                self.advance();
                Ok(value)
            }
            _ => Err(self.error_here("expected string literal")),
        }
    }

    fn expect_int(&mut self) -> Result<i64, ParseError> {
        match self.current().kind {
            TokenKind::Int(value) => {
                self.advance();
                Ok(value)
            }
            _ => Err(self.error_here("expected integer literal")),
        }
    }

    fn expect_simple(&mut self, expected: TokenKind, label: &str) -> Result<(), ParseError> {
        if self.take(&expected) {
            Ok(())
        } else {
            Err(self.error_here(&format!("expected {label}")))
        }
    }

    fn take(&mut self, expected: &TokenKind) -> bool {
        if self.at(expected) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn at(&self, expected: &TokenKind) -> bool {
        std::mem::discriminant(&self.current().kind) == std::mem::discriminant(expected)
    }

    fn current_ident(&self) -> Option<&str> {
        match &self.current().kind {
            TokenKind::Ident(value) => Some(value),
            _ => None,
        }
    }

    fn previous_ident(&self) -> Option<&str> {
        match &self.tokens[self.cursor.saturating_sub(1)].kind {
            TokenKind::Ident(value) => Some(value),
            _ => None,
        }
    }

    fn current(&self) -> &Token {
        &self.tokens[self.cursor]
    }

    fn previous_span(&self) -> Span {
        self.tokens[self.cursor.saturating_sub(1)].span
    }

    fn advance(&mut self) {
        if self.cursor + 1 < self.tokens.len() {
            self.cursor += 1;
        }
    }

    fn node_id(&mut self) -> NodeId {
        let value = self.next_node_id;
        self.next_node_id = self.next_node_id.saturating_add(1);
        value
    }

    fn error_here(&self, message: &str) -> ParseError {
        ParseError {
            message: message.to_string(),
            span: self.current().span,
        }
    }
}

fn merge_span(start: Span, end: Span) -> Span {
    Span::new(start.start, end.end, start.line, start.column)
}

fn expect_json_string(value: Value, name: &str, parser: &Parser) -> Result<String, ParseError> {
    value
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| parser.error_here(&format!("{name} must be a string")))
}

fn expect_json_number(value: Value, name: &str, parser: &Parser) -> Result<f64, ParseError> {
    value
        .as_f64()
        .ok_or_else(|| parser.error_here(&format!("{name} must be a number")))
}

fn expect_json_usize(value: Value, name: &str, parser: &Parser) -> Result<usize, ParseError> {
    value
        .as_u64()
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| parser.error_here(&format!("{name} must be a non-negative integer")))
}

fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(value) if value.is_i64() || value.is_u64() => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn valid_slug(value: &str) -> bool {
    !value.is_empty()
        && value.split('-').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        })
}
