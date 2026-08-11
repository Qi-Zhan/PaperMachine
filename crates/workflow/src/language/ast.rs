use super::lexer::Span;
use super::schema::BoundarySchema;
use papermachine_protocol::AccessPreset;
use papermachine_protocol::ReasoningEffort;
use papermachine_protocol::WebSearchContextSize;
use papermachine_protocol::WorkflowRequestMode;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;

pub type NodeId = u32;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Program {
    pub version: u32,
    pub schemas: BTreeMap<String, BoundarySchema>,
    pub agents: BTreeMap<String, AgentTemplate>,
    pub functions: BTreeMap<String, Function>,
    pub workflow: Workflow,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentTemplate {
    pub name: String,
    pub role: String,
    pub system: String,
    pub model: String,
    pub skills: Vec<String>,
    pub access: AccessPreset,
    pub actions: BTreeMap<String, ActionDefinition>,
    pub span: Span,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ActionDefinition {
    pub name: String,
    pub parameters: Vec<String>,
    pub prompt: String,
    pub tools: Option<Vec<String>>,
    pub search_context: Option<WebSearchContextSize>,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub finalize: FinalizePolicy,
    pub result: Option<BoundarySchema>,
    pub span: Span,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinalizePolicy {
    #[default]
    None,
    IfNeeded,
    AfterSearch,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Function {
    pub name: String,
    pub parameters: Vec<String>,
    pub body: Block,
    pub span: Span,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Workflow {
    pub slug: String,
    pub name: String,
    pub description: String,
    pub request_mode: WorkflowRequestMode,
    pub params: Vec<Parameter>,
    pub run_parameter: String,
    pub body: Block,
    pub span: Span,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Parameter {
    pub name: String,
    pub schema: BoundarySchema,
    pub optional: bool,
    pub span: Span,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Block {
    pub id: NodeId,
    pub statements: Vec<Statement>,
    pub tail: Option<Box<Expression>>,
    pub span: Span,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Statement {
    pub id: NodeId,
    pub kind: StatementKind,
    pub span: Span,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StatementKind {
    Let {
        name: String,
        mutable: bool,
        value: Expression,
    },
    Assign {
        name: String,
        operator: AssignOperator,
        value: Expression,
    },
    Expression {
        expression: Expression,
    },
    If {
        condition: Expression,
        then_block: Block,
        else_block: Option<Block>,
    },
    While {
        condition: Expression,
        body: Block,
    },
    Loop {
        body: Block,
    },
    For {
        binding: String,
        iterable: Expression,
        body: Block,
    },
    Match {
        value: Expression,
        arms: Vec<MatchArm>,
    },
    Break,
    Continue,
    Return {
        value: Expression,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssignOperator {
    Set,
    Add,
    Subtract,
    Multiply,
    Divide,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MatchArm {
    pub patterns: Vec<Pattern>,
    pub body: MatchBody,
    pub span: Span,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Pattern {
    Literal(Value),
    Wildcard,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum MatchBody {
    Block(Block),
    Statement(Box<Statement>),
    Expression(Expression),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Expression {
    pub id: NodeId,
    pub kind: ExpressionKind,
    pub span: Span,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExpressionKind {
    Literal {
        value: Value,
    },
    Variable {
        name: String,
    },
    List {
        values: Vec<Expression>,
    },
    Object {
        fields: Vec<(String, Expression)>,
    },
    Unary {
        operator: UnaryOperator,
        value: Box<Expression>,
    },
    Binary {
        operator: BinaryOperator,
        left: Box<Expression>,
        right: Box<Expression>,
    },
    Member {
        value: Box<Expression>,
        name: String,
    },
    Index {
        value: Box<Expression>,
        index: Box<Expression>,
    },
    Call {
        callee: Box<Expression>,
        arguments: Vec<CallArgument>,
    },
    Await {
        value: Box<Expression>,
    },
    Parallel {
        branches: Vec<ParallelBranch>,
    },
    ParallelFor {
        binding: String,
        iterable: Box<Expression>,
        key: Box<Expression>,
        body: Block,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CallArgument {
    pub name: Option<String>,
    pub value: Expression,
    pub span: Span,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ParallelBranch {
    pub name: String,
    pub body: Block,
    pub span: Span,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnaryOperator {
    Not,
    Negate,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BinaryOperator {
    Coalesce,
    Or,
    And,
    Equal,
    NotEqual,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
    Add,
    Subtract,
    Multiply,
    Divide,
    Remainder,
}
