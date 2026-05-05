//! Dataview-style frontmatter query DSL.
//!
//! Grammar (informal):
//!   query    := expr
//!   expr     := term (("AND" | "OR") term)*
//!   term     := "NOT" term | "(" expr ")" | clause
//!   clause   := field ":" value
//!            |  field ":>" value         (Dates/strings, lex compare)
//!            |  field ":<" value
//!   field    := "id" | "type" | "title" | "tag" | "created" | "updated"
//!   value    := bare-word | "quoted string"
//!
//! Examples:
//!   type:source AND tag:customer AND updated:>2026-04-01
//!   tag:nis2 OR tag:dora
//!   NOT (type:source) AND title:NLSpec
//!
//! Parameterised SQL is generated; user input never gets string-interpolated
//! into the query (defends against SQL injection).

pub mod executor;
pub mod parser;
pub mod sql;

#[derive(Debug, Clone, PartialEq)]
pub enum Field {
    Id,
    Type,
    Title,
    Tag,
    Created,
    Updated,
}

impl Field {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.to_ascii_lowercase().as_str() {
            "id" => Some(Self::Id),
            "type" => Some(Self::Type),
            "title" => Some(Self::Title),
            "tag" => Some(Self::Tag),
            "created" => Some(Self::Created),
            "updated" => Some(Self::Updated),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    Eq,
    Gt,
    Lt,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Clause {
    pub field: Field,
    pub op: Op,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Clause(Clause),
    Not(Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
}

#[derive(Debug, thiserror::Error, Clone, PartialEq)]
pub enum QueryError {
    #[error("unexpected token: {0}")]
    UnexpectedToken(String),

    #[error("unknown field: {0} — supported: id, type, title, tag, created, updated")]
    UnknownField(String),

    #[error("expected value after operator")]
    MissingValue,

    #[error("unclosed quote in query")]
    UnclosedQuote,

    #[error("empty query")]
    EmptyQuery,

    #[error("expected closing parenthesis")]
    MissingParen,
}
