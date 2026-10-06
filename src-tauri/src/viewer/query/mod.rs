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
//!            |  "valid"                   (now | all | expired; `:` only)
//!            |  "sort"                    (updated | salience; `:` only,
//!                                          top-level AND chain only)
//!   value    := bare-word | "quoted string"
//!
//! Validity (Slice C): a query that does not mention `valid:` behaves as
//! if it said `AND valid:now` — pages whose `valid_to` lies before today,
//! whose `valid_from` lies after today, or that carry `superseded_by` are
//! left out. A validity date that is not `YYYY-MM-DD` counts as absent.
//! `valid:all` disables the filter, `valid:expired` returns only the
//! pages `valid:now` leaves out (including not-yet-valid ones).
//!
//! Order: newest `updated` first; `sort:salience` orders by
//! `reads * 2 + search_hits` (H3, local `page_access` counters) instead.
//!
//! Examples:
//!   type:source AND tag:customer AND updated:>2026-04-01
//!   tag:nis2 OR tag:dora
//!   NOT (type:source) AND title:NLSpec
//!   type:entity AND valid:all AND sort:salience
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
    /// Validity filter: `now` (default), `all`, `expired`.
    Valid,
    /// Result order: `updated` (default), `salience`.
    Sort,
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
            "valid" => Some(Self::Valid),
            "sort" => Some(Self::Sort),
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

    #[error("unknown field: {0} — supported: id, type, title, tag, created, updated, valid, sort")]
    UnknownField(String),

    #[error("invalid value: {0}")]
    InvalidValue(String),

    #[error("sort: may only be combined with AND at the top level of the query (not inside OR, NOT or parentheses with OR)")]
    MisplacedSort,

    #[error("expected value after operator")]
    MissingValue,

    #[error("unclosed quote in query")]
    UnclosedQuote,

    #[error("empty query")]
    EmptyQuery,

    #[error("expected closing parenthesis")]
    MissingParen,
}
