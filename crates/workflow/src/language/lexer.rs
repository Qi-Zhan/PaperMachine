use serde::Deserialize;
use serde::Serialize;
use thiserror::Error;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    pub line: u32,
    pub column: u32,
}

impl Span {
    pub const fn new(start: usize, end: usize, line: u32, column: u32) -> Self {
        Self {
            start,
            end,
            line,
            column,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum TokenKind {
    Ident(String),
    String(String),
    Int(i64),
    Number(f64),
    LBrace,
    RBrace,
    LParen,
    RParen,
    LBracket,
    RBracket,
    Comma,
    Colon,
    Semicolon,
    Dot,
    Question,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Bang,
    Equal,
    Less,
    Greater,
    Pipe,
    PlusEqual,
    MinusEqual,
    StarEqual,
    SlashEqual,
    EqualEqual,
    BangEqual,
    LessEqual,
    GreaterEqual,
    AndAnd,
    OrOr,
    Coalesce,
    FatArrow,
    Eof,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

#[derive(Debug, Error)]
#[error("{message} at {line}:{column}")]
pub struct LexError {
    pub message: String,
    pub line: u32,
    pub column: u32,
}

pub fn lex(source: &str) -> Result<Vec<Token>, LexError> {
    Lexer::new(source).lex_all()
}

struct Lexer<'a> {
    source: &'a str,
    offset: usize,
    line: u32,
    column: u32,
}

impl<'a> Lexer<'a> {
    const fn new(source: &'a str) -> Self {
        Self {
            source,
            offset: 0,
            line: 1,
            column: 1,
        }
    }

    fn lex_all(mut self) -> Result<Vec<Token>, LexError> {
        let mut tokens = Vec::new();
        loop {
            self.skip_space_and_comments();
            let start = self.offset;
            let line = self.line;
            let column = self.column;
            let Some(character) = self.peek() else {
                tokens.push(Token {
                    kind: TokenKind::Eof,
                    span: Span::new(start, start, line, column),
                });
                return Ok(tokens);
            };
            let kind = if is_ident_start(character) {
                self.identifier()
            } else if character.is_ascii_digit() {
                self.number()?
            } else {
                match character {
                    '"' => TokenKind::String(self.string()?),
                    '{' => self.single(TokenKind::LBrace),
                    '}' => self.single(TokenKind::RBrace),
                    '(' => self.single(TokenKind::LParen),
                    ')' => self.single(TokenKind::RParen),
                    '[' => self.single(TokenKind::LBracket),
                    ']' => self.single(TokenKind::RBracket),
                    ',' => self.single(TokenKind::Comma),
                    ':' => self.single(TokenKind::Colon),
                    ';' => self.single(TokenKind::Semicolon),
                    '.' => self.single(TokenKind::Dot),
                    '%' => self.single(TokenKind::Percent),
                    '|' => {
                        self.bump();
                        if self.take('|') {
                            TokenKind::OrOr
                        } else {
                            TokenKind::Pipe
                        }
                    }
                    '+' => self.with_equal(TokenKind::Plus, TokenKind::PlusEqual),
                    '-' => self.with_equal(TokenKind::Minus, TokenKind::MinusEqual),
                    '*' => self.with_equal(TokenKind::Star, TokenKind::StarEqual),
                    '/' => self.with_equal(TokenKind::Slash, TokenKind::SlashEqual),
                    '!' => self.with_equal(TokenKind::Bang, TokenKind::BangEqual),
                    '=' => {
                        self.bump();
                        if self.take('=') {
                            TokenKind::EqualEqual
                        } else if self.take('>') {
                            TokenKind::FatArrow
                        } else {
                            TokenKind::Equal
                        }
                    }
                    '<' => self.with_equal(TokenKind::Less, TokenKind::LessEqual),
                    '>' => self.with_equal(TokenKind::Greater, TokenKind::GreaterEqual),
                    '&' => {
                        self.bump();
                        if self.take('&') {
                            TokenKind::AndAnd
                        } else {
                            return Err(self.error("single `&` is not supported; use `&&`"));
                        }
                    }
                    '?' => {
                        self.bump();
                        if self.take('?') {
                            TokenKind::Coalesce
                        } else {
                            TokenKind::Question
                        }
                    }
                    other => {
                        return Err(self.error(&format!("unexpected character `{other}`")));
                    }
                }
            };
            tokens.push(Token {
                kind,
                span: Span::new(start, self.offset, line, column),
            });
        }
    }

    fn identifier(&mut self) -> TokenKind {
        let start = self.offset;
        self.bump();
        while self.peek().is_some_and(is_ident_continue) {
            self.bump();
        }
        TokenKind::Ident(self.source[start..self.offset].to_string())
    }

    fn number(&mut self) -> Result<TokenKind, LexError> {
        let start = self.offset;
        while self.peek().is_some_and(|value| value.is_ascii_digit()) {
            self.bump();
        }
        let mut floating = false;
        if self.peek() == Some('.')
            && self
                .source
                .get(self.offset + 1..)
                .and_then(|rest| rest.chars().next())
                .is_some_and(|value| value.is_ascii_digit())
        {
            floating = true;
            self.bump();
            while self.peek().is_some_and(|value| value.is_ascii_digit()) {
                self.bump();
            }
        }
        if matches!(self.peek(), Some('e' | 'E')) {
            floating = true;
            self.bump();
            if matches!(self.peek(), Some('+' | '-')) {
                self.bump();
            }
            let exponent_start = self.offset;
            while self.peek().is_some_and(|value| value.is_ascii_digit()) {
                self.bump();
            }
            if exponent_start == self.offset {
                return Err(self.error("number exponent requires digits"));
            }
        }
        let text = &self.source[start..self.offset];
        if floating {
            text.parse::<f64>()
                .map(TokenKind::Number)
                .map_err(|_| self.error("invalid number literal"))
        } else {
            text.parse::<i64>()
                .map(TokenKind::Int)
                .map_err(|_| self.error("integer literal is outside i64"))
        }
    }

    fn string(&mut self) -> Result<String, LexError> {
        let triple = self.source[self.offset..].starts_with("\"\"\"");
        if triple {
            self.bump();
            self.bump();
            self.bump();
        } else {
            self.bump();
        }
        let mut output = String::new();
        loop {
            if triple && self.source[self.offset..].starts_with("\"\"\"") {
                self.bump();
                self.bump();
                self.bump();
                return Ok(output);
            }
            let Some(character) = self.bump() else {
                return Err(self.error("unterminated string literal"));
            };
            if !triple && character == '"' {
                return Ok(output);
            }
            if !triple && character == '\n' {
                return Err(self.error("ordinary string literals cannot contain newlines"));
            }
            if character != '\\' {
                output.push(character);
                continue;
            }
            let Some(escaped) = self.bump() else {
                return Err(self.error("unterminated string escape"));
            };
            match escaped {
                'n' => output.push('\n'),
                'r' => output.push('\r'),
                't' => output.push('\t'),
                '\\' => output.push('\\'),
                '"' => output.push('"'),
                other => {
                    return Err(self.error(&format!("unsupported string escape `\\{other}`")));
                }
            }
        }
    }

    fn skip_space_and_comments(&mut self) {
        loop {
            while self.peek().is_some_and(char::is_whitespace) {
                self.bump();
            }
            if !self.source[self.offset..].starts_with("//") {
                break;
            }
            while self.peek().is_some_and(|value| value != '\n') {
                self.bump();
            }
        }
    }

    fn single(&mut self, token: TokenKind) -> TokenKind {
        self.bump();
        token
    }

    fn with_equal(&mut self, plain: TokenKind, equal: TokenKind) -> TokenKind {
        self.bump();
        if self.take('=') { equal } else { plain }
    }

    fn take(&mut self, expected: char) -> bool {
        if self.peek() == Some(expected) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn peek(&self) -> Option<char> {
        self.source[self.offset..].chars().next()
    }

    fn bump(&mut self) -> Option<char> {
        let character = self.peek()?;
        self.offset += character.len_utf8();
        if character == '\n' {
            self.line += 1;
            self.column = 1;
        } else {
            self.column += 1;
        }
        Some(character)
    }

    fn error(&self, message: &str) -> LexError {
        LexError {
            message: message.to_string(),
            line: self.line,
            column: self.column,
        }
    }
}

fn is_ident_start(value: char) -> bool {
    value == '_' || value.is_ascii_alphabetic()
}

fn is_ident_continue(value: char) -> bool {
    value == '_' || value.is_ascii_alphanumeric()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lexes_comments_strings_numbers_and_operators() {
        let tokens = lex(r#"version 1; // comment
            let text = "hi\nthere";
            let prompt = """multi
line""";
            value += 2.5e1;
            value == 25 && text != "";
            "#)
        .expect("source should lex");
        assert!(
            tokens
                .iter()
                .any(|token| token.kind == TokenKind::PlusEqual)
        );
        assert!(tokens.iter().any(|token| token.kind == TokenKind::AndAnd));
        assert!(tokens.iter().any(|token| {
            matches!(&token.kind, TokenKind::String(value) if value == "multi\nline")
        }));
        assert!(
            tokens
                .iter()
                .any(|token| { matches!(token.kind, TokenKind::Number(value) if value == 25.0) })
        );
    }
}
