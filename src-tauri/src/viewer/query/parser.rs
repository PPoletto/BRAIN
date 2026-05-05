//! Tokeniser + recursive-descent parser for the query DSL.

use super::{Clause, Expr, Field, Op, QueryError};

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Word(String),
    QuotedWord(String),
    Colon,
    ColonGt,
    ColonLt,
    LParen,
    RParen,
    And,
    Or,
    Not,
}

fn tokenize(input: &str) -> Result<Vec<Token>, QueryError> {
    let mut tokens = Vec::new();
    let mut chars = input.chars().peekable();
    while let Some(&c) = chars.peek() {
        match c {
            ' ' | '\t' | '\n' | '\r' => {
                chars.next();
            }
            '(' => {
                chars.next();
                tokens.push(Token::LParen);
            }
            ')' => {
                chars.next();
                tokens.push(Token::RParen);
            }
            ':' => {
                chars.next();
                match chars.peek() {
                    Some('>') => {
                        chars.next();
                        tokens.push(Token::ColonGt);
                    }
                    Some('<') => {
                        chars.next();
                        tokens.push(Token::ColonLt);
                    }
                    _ => tokens.push(Token::Colon),
                }
            }
            '"' => {
                chars.next();
                let mut buf = String::new();
                let mut closed = false;
                while let Some(&c) = chars.peek() {
                    if c == '"' {
                        chars.next();
                        closed = true;
                        break;
                    }
                    buf.push(c);
                    chars.next();
                }
                if !closed {
                    return Err(QueryError::UnclosedQuote);
                }
                tokens.push(Token::QuotedWord(buf));
            }
            _ => {
                let mut buf = String::new();
                while let Some(&c) = chars.peek() {
                    if c.is_whitespace() || c == '(' || c == ')' || c == ':' || c == '"' {
                        break;
                    }
                    buf.push(c);
                    chars.next();
                }
                let upper = buf.to_ascii_uppercase();
                tokens.push(match upper.as_str() {
                    "AND" => Token::And,
                    "OR" => Token::Or,
                    "NOT" => Token::Not,
                    _ => Token::Word(buf),
                });
            }
        }
    }
    Ok(tokens)
}

pub fn parse(input: &str) -> Result<Expr, QueryError> {
    let tokens = tokenize(input)?;
    if tokens.is_empty() {
        return Err(QueryError::EmptyQuery);
    }
    let mut p = Parser::new(tokens);
    let expr = p.parse_expr()?;
    if p.pos < p.tokens.len() {
        return Err(QueryError::UnexpectedToken(format!(
            "{:?}",
            p.tokens[p.pos]
        )));
    }
    Ok(expr)
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn new(tokens: Vec<Token>) -> Self {
        Self { tokens, pos: 0 }
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn bump(&mut self) -> Option<Token> {
        let t = self.tokens.get(self.pos).cloned();
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    fn parse_expr(&mut self) -> Result<Expr, QueryError> {
        let mut left = self.parse_and()?;
        while let Some(Token::Or) = self.peek() {
            self.bump();
            let right = self.parse_and()?;
            left = Expr::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<Expr, QueryError> {
        let mut left = self.parse_term()?;
        while let Some(Token::And) = self.peek() {
            self.bump();
            let right = self.parse_term()?;
            left = Expr::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_term(&mut self) -> Result<Expr, QueryError> {
        match self.peek() {
            Some(Token::Not) => {
                self.bump();
                let inner = self.parse_term()?;
                Ok(Expr::Not(Box::new(inner)))
            }
            Some(Token::LParen) => {
                self.bump();
                let inner = self.parse_expr()?;
                match self.bump() {
                    Some(Token::RParen) => Ok(inner),
                    _ => Err(QueryError::MissingParen),
                }
            }
            _ => self.parse_clause(),
        }
    }

    fn parse_clause(&mut self) -> Result<Expr, QueryError> {
        let field_word = match self.bump() {
            Some(Token::Word(w)) => w,
            Some(other) => return Err(QueryError::UnexpectedToken(format!("{other:?}"))),
            None => return Err(QueryError::EmptyQuery),
        };
        let field = Field::parse(&field_word).ok_or(QueryError::UnknownField(field_word))?;
        let op = match self.bump() {
            Some(Token::Colon) => Op::Eq,
            Some(Token::ColonGt) => Op::Gt,
            Some(Token::ColonLt) => Op::Lt,
            other => {
                return Err(QueryError::UnexpectedToken(format!(
                    "expected `:`, `:>` or `:<`, got {other:?}"
                )));
            }
        };
        let value = match self.bump() {
            Some(Token::Word(w)) => w,
            Some(Token::QuotedWord(w)) => w,
            _ => return Err(QueryError::MissingValue),
        };
        Ok(Expr::Clause(Clause { field, op, value }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_eq_clause() {
        let expr = parse("type:entity").unwrap();
        match expr {
            Expr::Clause(c) => {
                assert_eq!(c.field, Field::Type);
                assert_eq!(c.op, Op::Eq);
                assert_eq!(c.value, "entity");
            }
            _ => panic!("expected Clause"),
        }
    }

    #[test]
    fn parses_and_chain_left_associative() {
        let expr = parse("type:source AND tag:customer AND updated:>2026-04-01").unwrap();
        // (type:source AND tag:customer) AND updated:>2026-04-01
        match expr {
            Expr::And(left, right) => {
                assert!(matches!(*left, Expr::And(_, _)));
                assert!(matches!(*right, Expr::Clause(_)));
            }
            _ => panic!("expected outer And"),
        }
    }

    #[test]
    fn parses_or_with_lower_precedence_than_and() {
        // `a:1 AND b:2 OR c:3` parses as `(a:1 AND b:2) OR c:3`
        let expr = parse("tag:foo AND tag:bar OR tag:baz").unwrap();
        match expr {
            Expr::Or(left, right) => {
                assert!(matches!(*left, Expr::And(_, _)));
                assert!(matches!(*right, Expr::Clause(_)));
            }
            _ => panic!("expected outer Or"),
        }
    }

    #[test]
    fn parses_parenthesised_expression() {
        let expr = parse("(tag:foo OR tag:bar) AND type:source").unwrap();
        match expr {
            Expr::And(left, right) => {
                assert!(matches!(*left, Expr::Or(_, _)));
                assert!(matches!(*right, Expr::Clause(_)));
            }
            _ => panic!("expected outer And"),
        }
    }

    #[test]
    fn parses_not_term() {
        let expr = parse("NOT type:source").unwrap();
        assert!(matches!(expr, Expr::Not(_)));
    }

    #[test]
    fn parses_quoted_value_with_spaces() {
        let expr = parse("title:\"NLSpec & Holdouts\"").unwrap();
        match expr {
            Expr::Clause(c) => assert_eq!(c.value, "NLSpec & Holdouts"),
            _ => panic!(),
        }
    }

    #[test]
    fn rejects_unknown_field() {
        let err = parse("foobar:x").unwrap_err();
        assert!(matches!(err, QueryError::UnknownField(_)));
    }

    #[test]
    fn rejects_unclosed_quote() {
        let err = parse("title:\"open").unwrap_err();
        assert_eq!(err, QueryError::UnclosedQuote);
    }

    #[test]
    fn rejects_empty_query() {
        let err = parse("   ").unwrap_err();
        assert_eq!(err, QueryError::EmptyQuery);
    }

    #[test]
    fn rejects_missing_paren() {
        let err = parse("(type:source").unwrap_err();
        assert_eq!(err, QueryError::MissingParen);
    }
}
