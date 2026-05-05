//! Translates the parsed query AST into a parameterised SQL statement.
//!
//! The SQL is **always parameterised** with `?N` placeholders — user input
//! never gets string-interpolated. This is the security boundary against
//! SQL injection. Tests verify it.

use rusqlite::types::Value as SqlValue;

use super::{Clause, Expr, Field, Op};

/// Returned by `compile`. The frontend passes this to `rusqlite::query_map`.
pub struct CompiledQuery {
    /// The SQL statement, with `?1`, `?2` … placeholders.
    pub sql: String,
    /// Parameter values bound in declaration order.
    pub params: Vec<SqlValue>,
}

const BASE_SQL: &str = "SELECT id, type, path, title, frontmatter, body, updated_at \
                       FROM pages WHERE ";

const ORDER_AND_LIMIT: &str = " ORDER BY COALESCE(updated_at, '') DESC, id ASC LIMIT 200";

pub fn compile(expr: &Expr) -> CompiledQuery {
    let mut params: Vec<SqlValue> = Vec::new();
    let where_clause = build_where(expr, &mut params);
    let sql = format!("{BASE_SQL}{where_clause}{ORDER_AND_LIMIT}");
    CompiledQuery { sql, params }
}

fn build_where(expr: &Expr, params: &mut Vec<SqlValue>) -> String {
    match expr {
        Expr::Clause(c) => clause_sql(c, params),
        Expr::Not(inner) => format!("NOT ({})", build_where(inner, params)),
        Expr::And(a, b) => format!(
            "({}) AND ({})",
            build_where(a, params),
            build_where(b, params)
        ),
        Expr::Or(a, b) => format!(
            "({}) OR ({})",
            build_where(a, params),
            build_where(b, params)
        ),
    }
}

fn clause_sql(c: &Clause, params: &mut Vec<SqlValue>) -> String {
    let value = SqlValue::Text(c.value.clone());
    match (&c.field, &c.op) {
        (Field::Tag, Op::Eq) => {
            params.push(value);
            let n = params.len();
            format!("EXISTS (SELECT 1 FROM page_tags pt WHERE pt.page_id = pages.id AND pt.tag = ?{n})")
        }
        (Field::Tag, _) => "0 /* tag only supports `:` equality */".to_string(),
        (Field::Title, Op::Eq) => {
            params.push(SqlValue::Text(format!("%{}%", c.value)));
            let n = params.len();
            format!("title LIKE ?{n}")
        }
        (Field::Id, Op::Eq) => {
            params.push(value);
            let n = params.len();
            format!("id = ?{n}")
        }
        (Field::Type, Op::Eq) => {
            params.push(value);
            let n = params.len();
            format!("type = ?{n}")
        }
        (Field::Created, op) => {
            params.push(value);
            let n = params.len();
            let cmp = sql_op(op);
            format!("COALESCE(json_extract(frontmatter, '$.created'), '') {cmp} ?{n}")
        }
        (Field::Updated, op) => {
            params.push(value);
            let n = params.len();
            let cmp = sql_op(op);
            format!("COALESCE(updated_at, '') {cmp} ?{n}")
        }
        (Field::Title | Field::Id | Field::Type, op) => {
            params.push(value);
            let n = params.len();
            let cmp = sql_op(op);
            let col = match c.field {
                Field::Title => "COALESCE(title, '')",
                Field::Id => "id",
                Field::Type => "type",
                _ => unreachable!(),
            };
            format!("{col} {cmp} ?{n}")
        }
    }
}

fn sql_op(op: &Op) -> &'static str {
    match op {
        Op::Eq => "=",
        Op::Gt => ">",
        Op::Lt => "<",
    }
}

#[cfg(test)]
mod tests {
    use super::super::parser::parse;
    use super::*;

    #[test]
    fn compiled_sql_uses_parameter_placeholders_for_user_values() {
        let expr = parse("type:source AND tag:customer").unwrap();
        let q = compile(&expr);
        assert!(q.sql.contains("?1"));
        assert!(q.sql.contains("?2"));
        // Critical: the value strings must NOT appear inline anywhere.
        assert!(!q.sql.contains("source"));
        assert!(!q.sql.contains("customer"));
        assert_eq!(q.params.len(), 2);
        assert!(matches!(&q.params[0], SqlValue::Text(t) if t == "source"));
        assert!(matches!(&q.params[1], SqlValue::Text(t) if t == "customer"));
    }

    #[test]
    fn compiled_sql_resists_basic_injection_attempts() {
        // Even with a malicious value, it ends up parameterised.
        let expr = parse("title:\"x' OR '1'='1\"").unwrap();
        let q = compile(&expr);
        assert!(!q.sql.contains("OR '1'='1"));
        assert_eq!(q.params.len(), 1);
        // Title uses LIKE → wrapped in %...%
        assert!(matches!(&q.params[0], SqlValue::Text(t) if t.contains("OR '1'='1")));
    }

    #[test]
    fn updated_supports_greater_than_for_date_ranges() {
        let expr = parse("updated:>2026-04-01").unwrap();
        let q = compile(&expr);
        assert!(q.sql.contains(">"));
        assert!(q.sql.contains("updated_at"));
    }

    #[test]
    fn tag_eq_uses_exists_against_page_tags() {
        let expr = parse("tag:nis2").unwrap();
        let q = compile(&expr);
        assert!(q.sql.contains("EXISTS"));
        assert!(q.sql.contains("page_tags"));
    }

    #[test]
    fn or_renders_as_or_in_sql() {
        let expr = parse("tag:nis2 OR tag:dora").unwrap();
        let q = compile(&expr);
        assert!(q.sql.contains(") OR ("));
    }

    #[test]
    fn not_wraps_inner_expression() {
        let expr = parse("NOT type:source").unwrap();
        let q = compile(&expr);
        assert!(q.sql.contains("NOT ("));
    }
}
