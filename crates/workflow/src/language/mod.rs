//! Parser and compiler for PaperMachine Workflow Language v1.

pub mod ast;
pub mod compiler;
pub mod lexer;
pub mod parser;
pub mod schema;

pub use ast::*;
pub use compiler::{CompiledWorkflow, compile_source, validate_source};
pub use lexer::Span;
pub use schema::{
    BoundarySchema, SchemaField, SchemaKind, apply_json_schema_defaults,
    validate_json_schema_definition, validate_json_schema_value,
};

/// Maximum accepted Workflow source size in bytes.
pub const MAX_SOURCE_BYTES: usize = 128 * 1024;

#[derive(Clone, Debug)]
pub struct LanguageError {
    pub message: String,
    pub span: Option<Span>,
}

pub fn parse_source(source: &str) -> Result<Program, LanguageError> {
    if source.len() > MAX_SOURCE_BYTES {
        return Err(LanguageError {
            message: format!(
                "workflow source exceeds the public {} byte limit",
                MAX_SOURCE_BYTES
            ),
            span: None,
        });
    }
    let tokens = lexer::lex(source).map_err(|error| LanguageError {
        message: error.message,
        span: Some(Span::new(0, 0, error.line, error.column)),
    })?;
    parser::parse(tokens).map_err(|error| LanguageError {
        message: error.message,
        span: Some(error.span),
    })
}
