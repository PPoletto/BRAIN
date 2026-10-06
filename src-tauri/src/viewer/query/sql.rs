//! Translates the parsed query AST into a parameterised SQL statement.
//!
//! The SQL is **always parameterised** with `?N` placeholders — user input
//! never gets string-interpolated. This is the security boundary against
//! SQL injection. Tests verify it.

use rusqlite::types::Value as SqlValue;

use super::{Clause, Expr, Field, Op, QueryError};

/// Returned by `compile`. The frontend passes this to `rusqlite::query_map`.
pub struct CompiledQuery {
    /// The SQL statement, with `?1`, `?2` … placeholders.
    pub sql: String,
    /// Parameter values bound in declaration order.
    pub params: Vec<SqlValue>,
}

/// Selected columns, in the order `executor` reads them. `page_access`
/// (H3 salience) is LEFT JOINed: pages never read have no row there.
const BASE_SQL: &str = "SELECT pages.id, pages.type, pages.path, pages.title, pages.frontmatter, \
                        pages.body, pages.updated_at, COALESCE(pa.reads, 0), \
                        COALESCE(pa.search_hits, 0), pa.last_read_at, pages.valid_from, \
                        pages.valid_to, pages.superseded_by \
                       FROM pages LEFT JOIN page_access pa ON pa.page_id = pages.id WHERE ";

const ORDER_BY_UPDATED: &str = " ORDER BY COALESCE(updated_at, '') DESC, pages.id ASC LIMIT 200";

const ORDER_BY_SALIENCE: &str = " ORDER BY (COALESCE(pa.reads, 0) * 2 + COALESCE(pa.search_hits, 0)) DESC, \
                                 COALESCE(updated_at, '') DESC, pages.id ASC LIMIT 200";

/// Result order of a query (`sort:` clause).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sort {
    Updated,
    Salience,
}

/// Compile a filter expression as written: no default validity filter,
/// newest first. `valid:` clauses compare against today's local date.
/// [`compile_query`] is what the executor runs; this is the seam the
/// SQL-shape tests use.
#[cfg(test)]
pub fn compile(expr: &Expr) -> CompiledQuery {
    let today = crate::wiki::lint::today_local();
    let mut params: Vec<SqlValue> = Vec::new();
    let where_clause = build_where(expr, &mut params, &today);
    let sql = format!("{BASE_SQL}{where_clause}{ORDER_BY_UPDATED}");
    CompiledQuery { sql, params }
}

/// Compile a whole query: `sort:` clauses become the ORDER BY, and a
/// query that does not mention `valid:` gets `valid:now` (relative to
/// `today`, `YYYY-MM-DD`) added. A query of only `sort:` / `valid:`
/// clauses matches every page they allow.
pub fn compile_query(expr: Expr, today: &str) -> Result<CompiledQuery, QueryError> {
    let (filter, sort) = split_sort(expr)?;
    let mut params: Vec<SqlValue> = Vec::new();
    let mut where_clause = match &filter {
        Some(f) => build_where(f, &mut params, today),
        None => "1".to_string(),
    };
    if !filter.as_ref().is_some_and(mentions_valid) {
        where_clause = format!("({where_clause}) AND ({})", valid_now_sql(&mut params, today));
    }
    let order = match sort.unwrap_or(Sort::Updated) {
        Sort::Updated => ORDER_BY_UPDATED,
        Sort::Salience => ORDER_BY_SALIENCE,
    };
    Ok(CompiledQuery {
        sql: format!("{BASE_SQL}{where_clause}{order}"),
        params,
    })
}

fn is_sort(expr: &Expr) -> bool {
    matches!(expr, Expr::Clause(c) if c.field == Field::Sort)
}

fn contains_sort(expr: &Expr) -> bool {
    match expr {
        Expr::Clause(_) => is_sort(expr),
        Expr::Not(inner) => contains_sort(inner),
        Expr::And(a, b) | Expr::Or(a, b) => contains_sort(a) || contains_sort(b),
    }
}

fn mentions_valid(expr: &Expr) -> bool {
    match expr {
        Expr::Clause(c) => c.field == Field::Valid,
        Expr::Not(inner) => mentions_valid(inner),
        Expr::And(a, b) | Expr::Or(a, b) => mentions_valid(a) || mentions_valid(b),
    }
}

/// Take the `sort:` clauses out of the top-level AND chain (the last one
/// wins). A `sort:` anywhere else (under OR or NOT) is an error.
fn split_sort(expr: Expr) -> Result<(Option<Expr>, Option<Sort>), QueryError> {
    match expr {
        Expr::Clause(c) if c.field == Field::Sort => {
            let sort = if c.value == "salience" { Sort::Salience } else { Sort::Updated };
            Ok((None, Some(sort)))
        }
        Expr::And(a, b) => {
            let (fa, sa) = split_sort(*a)?;
            let (fb, sb) = split_sort(*b)?;
            let filter = match (fa, fb) {
                (Some(x), Some(y)) => Some(Expr::And(Box::new(x), Box::new(y))),
                (x, y) => x.or(y),
            };
            Ok((filter, sb.or(sa)))
        }
        other if contains_sort(&other) => Err(QueryError::MisplacedSort),
        other => Ok((Some(other), None)),
    }
}

/// SQLite GLOB of a `YYYY-MM-DD`-shaped date. A validity date of another
/// shape is treated as absent (the lint reports it as `invalid-date`).
const ISO_DATE_GLOB: &str = "'[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]'";

/// `valid:now`: no valid `valid_to` before today, no valid `valid_from`
/// after today, no `superseded_by`.
fn valid_now_sql(params: &mut Vec<SqlValue>, today: &str) -> String {
    params.push(SqlValue::Text(today.to_string()));
    let n = params.len();
    format!(
        "(NOT (COALESCE(pages.valid_to, '') GLOB {ISO_DATE_GLOB}) OR pages.valid_to >= ?{n}) \
         AND (NOT (COALESCE(pages.valid_from, '') GLOB {ISO_DATE_GLOB}) OR pages.valid_from <= ?{n}) \
         AND COALESCE(pages.superseded_by, '') = ''"
    )
}

fn build_where(expr: &Expr, params: &mut Vec<SqlValue>, today: &str) -> String {
    match expr {
        Expr::Clause(c) => clause_sql(c, params, today),
        Expr::Not(inner) => format!("NOT ({})", build_where(inner, params, today)),
        Expr::And(a, b) => format!(
            "({}) AND ({})",
            build_where(a, params, today),
            build_where(b, params, today)
        ),
        Expr::Or(a, b) => format!(
            "({}) OR ({})",
            build_where(a, params, today),
            build_where(b, params, today)
        ),
    }
}

fn clause_sql(c: &Clause, params: &mut Vec<SqlValue>, today: &str) -> String {
    let value = SqlValue::Text(c.value.clone());
    match (&c.field, &c.op) {
        (Field::Valid, _) => match c.value.as_str() {
            "all" => "1".to_string(),
            "expired" => format!("NOT ({})", valid_now_sql(params, today)),
            _ => valid_now_sql(params, today),
        },
        // Ordering, not a filter; `compile_query` removes it before this.
        (Field::Sort, _) => "1".to_string(),
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
    fn a_query_without_valid_gets_the_valid_now_filter() {
        let q = compile_query(parse("type:entity").unwrap(), "2026-10-06").unwrap();
        assert!(q.sql.contains("superseded_by, '') = ''"), "sql: {}", q.sql);
    }

    #[test]
    fn valid_all_switches_the_default_validity_filter_off() {
        let q = compile_query(parse("type:entity AND valid:all").unwrap(), "2026-10-06").unwrap();
        assert!(!q.sql.contains("COALESCE(pages.superseded_by"), "sql: {}", q.sql);
    }

    #[test]
    fn sort_salience_orders_by_reads_and_search_hits() {
        let q = compile_query(parse("sort:salience").unwrap(), "2026-10-06").unwrap();
        assert!(q.sql.contains("ORDER BY (COALESCE(pa.reads, 0) * 2"), "sql: {}", q.sql);
    }

    #[test]
    fn sort_inside_an_or_is_rejected() {
        let err = compile_query(parse("type:entity OR sort:salience").unwrap(), "2026-10-06")
            .err()
            .unwrap();
        assert_eq!(err, QueryError::MisplacedSort);
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
