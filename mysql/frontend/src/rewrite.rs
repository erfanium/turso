//! MySQL → engine SQL rewriting.
//!
//! The MySQL frontend parses statements with `sqlparser`'s MySQL dialect,
//! rewrites the MySQL-specific constructs on that AST, and renders the result
//! as SQL text the engine's native parser accepts. DDL is rendered by hand,
//! because a MySQL `CREATE TABLE` carries inline indexes, type modifiers and
//! options that have no direct native spelling.

use std::collections::HashMap;
use std::ops::ControlFlow;

use sqlparser::ast::{
    Assignment, AssignmentTarget, BinaryOperator, ColumnOption, CreateTable, DataType, Expr,
    FunctionArg, FunctionArgExpr, FunctionArguments, Ident, Insert, ObjectName, OnInsert, Query,
    SetExpr, SqliteOnConflict, Statement, TableConstraint, TableFactor, TableObject, Value,
    VisitMut, VisitorMut,
};
use sqlparser::dialect::{GenericDialect, MySqlDialect};
use sqlparser::parser::Parser;

use crate::catalog::{Catalog, ColumnInfo, MyType, TableInfo};

#[derive(Debug)]
pub struct RewriteError(pub String);

impl std::fmt::Display for RewriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RewriteError {}

type Result<T> = std::result::Result<T, RewriteError>;

fn err<T>(msg: impl Into<String>) -> Result<T> {
    Err(RewriteError(msg.into()))
}

/// What a rewritten statement is, as far as the session layer cares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    /// Returns a result set.
    Query,
    /// INSERT / UPDATE / DELETE / REPLACE.
    Dml,
    /// Schema change; `table` is set for `CREATE TABLE`.
    Ddl,
    Begin,
    Commit,
    Rollback,
    /// `USE db`.
    Use(String),
    /// Accepted and ignored (`SET NAMES`, ...).
    Noop,
}

#[derive(Debug, Clone)]
pub struct Rewritten {
    pub kind: Kind,
    /// Native statements to run in order. The last one produces the result.
    pub sql: Vec<String>,
    /// Table definition to register once the statements succeed.
    pub table: Option<TableInfo>,
    /// The statement inserts into a table with an AUTO_INCREMENT column, so
    /// the generated id is reported back.
    pub auto_increment: bool,
    /// MySQL types of the result columns of a query, where they can be
    /// derived from the statement.
    pub result_types: Vec<Option<MyType>>,
}

/// Rewrite one or more `;`-separated MySQL statements.
pub fn rewrite(sql: &str, catalog: &Catalog) -> Result<Vec<Rewritten>> {
    let stmts = Parser::parse_sql(&MySqlDialect {}, sql)
        .map_err(|e| RewriteError(format!("syntax error: {e}")))?;
    stmts
        .into_iter()
        .map(|stmt| rewrite_statement(stmt, catalog))
        .collect()
}

fn rewrite_statement(mut stmt: Statement, catalog: &Catalog) -> Result<Rewritten> {
    match stmt {
        Statement::StartTransaction { .. } => Ok(simple(Kind::Begin, "BEGIN")),
        Statement::Commit { .. } => Ok(simple(Kind::Commit, "COMMIT")),
        Statement::Rollback {
            savepoint: None, ..
        } => Ok(simple(Kind::Rollback, "ROLLBACK")),
        Statement::Rollback {
            savepoint: Some(ref name),
            ..
        } => Ok(simple(
            Kind::Dml,
            &format!("ROLLBACK TO {}", quote_ident(&name.value)),
        )),
        Statement::Savepoint { ref name } => Ok(simple(
            Kind::Dml,
            &format!("SAVEPOINT {}", quote_ident(&name.value)),
        )),
        Statement::ReleaseSavepoint { ref name } => Ok(simple(
            Kind::Dml,
            &format!("RELEASE {}", quote_ident(&name.value)),
        )),
        Statement::Set(_) => Ok(Rewritten {
            kind: Kind::Noop,
            sql: vec![],
            table: None,
            auto_increment: false,
            result_types: Vec::new(),
        }),
        Statement::Use(ref u) => {
            let name = u.to_string();
            let name = name
                .trim_start_matches("USE ")
                .trim_matches('`')
                .to_string();
            Ok(Rewritten {
                kind: Kind::Use(name),
                sql: vec![],
                table: None,
                auto_increment: false,
                result_types: Vec::new(),
            })
        }
        Statement::CreateTable(ref ct) => create_table(ct),
        Statement::Drop { .. } | Statement::CreateIndex(_) | Statement::Truncate(_) => {
            ddl_passthrough(stmt)
        }
        Statement::Query(ref mut query) => {
            // Types come from the MySQL statement, before it is rewritten.
            let result_types = crate::infer::result_types(query, catalog);
            add_primary_key_order(query, catalog);
            rewrite_exprs(&mut stmt)?;
            Ok(Rewritten {
                kind: Kind::Query,
                sql: vec![stmt.to_string()],
                table: None,
                auto_increment: false,
                result_types,
            })
        }
        Statement::Insert(_) => insert(stmt, catalog),
        Statement::Update(_) => update(stmt, catalog),
        Statement::Delete(_) => {
            rewrite_exprs(&mut stmt)?;
            Ok(Rewritten {
                kind: Kind::Dml,
                sql: vec![stmt.to_string()],
                table: None,
                auto_increment: false,
                result_types: Vec::new(),
            })
        }
        other => err(format!(
            "unsupported statement: {}",
            other.to_string().chars().take(80).collect::<String>()
        )),
    }
}

fn simple(kind: Kind, sql: &str) -> Rewritten {
    Rewritten {
        kind,
        sql: vec![sql.to_string()],
        table: None,
        auto_increment: false,
        result_types: Vec::new(),
    }
}

fn ddl_passthrough(stmt: Statement) -> Result<Rewritten> {
    let sql = match &stmt {
        Statement::Truncate(t) => t
            .table_names
            .iter()
            .map(|t| format!("DELETE FROM {}", quote_ident(&object_name_last(&t.name))))
            .collect(),
        _ => vec![stmt.to_string()],
    };
    let kind = if matches!(stmt, Statement::Truncate(_)) {
        Kind::Dml
    } else {
        Kind::Ddl
    };
    Ok(Rewritten {
        kind,
        sql,
        table: None,
        auto_increment: false,
        result_types: Vec::new(),
    })
}

// ---------------------------------------------------------------------------
// Expressions
// ---------------------------------------------------------------------------

struct ExprRewriter {
    error: Option<String>,
}

impl VisitorMut for ExprRewriter {
    type Break = ();

    fn pre_visit_query(&mut self, query: &mut Query) -> ControlFlow<()> {
        // Row locks have no native equivalent: writers are serialized, so a
        // locking read is a plain read.
        query.locks.clear();
        ControlFlow::Continue(())
    }

    fn pre_visit_table_factor(&mut self, table_factor: &mut TableFactor) -> ControlFlow<()> {
        if let TableFactor::Table { index_hints, .. } = table_factor {
            index_hints.clear();
        }
        ControlFlow::Continue(())
    }

    fn post_visit_expr(&mut self, expr: &mut Expr) -> ControlFlow<()> {
        match expr {
            // MySQL `/` is never integer division.
            Expr::BinaryOp {
                left,
                op: BinaryOperator::Divide,
                ..
            } => {
                let inner = std::mem::replace(left.as_mut(), Expr::value(Value::Null));
                **left = Expr::Nested(Box::new(Expr::BinaryOp {
                    left: Box::new(inner),
                    op: BinaryOperator::Multiply,
                    right: Box::new(Expr::value(Value::Number("1.0".to_string(), false))),
                }));
            }
            Expr::Value(v) => {
                if let Value::SingleQuotedString(s) | Value::DoubleQuotedString(s) = &v.value {
                    let text = match normalize_datetime_literal(s) {
                        Some(n) => n,
                        None => s.clone(),
                    };
                    // Always re-emit as a single-quoted literal: a
                    // double-quoted string is an identifier natively.
                    v.value = Value::SingleQuotedString(text);
                }
            }
            // `VALUES(col)` inside ON DUPLICATE KEY UPDATE.
            Expr::Function(f) if f.name.to_string().eq_ignore_ascii_case("values") => {
                if let FunctionArguments::List(list) = &f.args {
                    if let [FunctionArg::Unnamed(FunctionArgExpr::Expr(arg))] = list.args.as_slice()
                    {
                        let col = match arg {
                            Expr::Identifier(id) => Some(id.clone()),
                            Expr::CompoundIdentifier(ids) => ids.last().cloned(),
                            _ => None,
                        };
                        if let Some(col) = col {
                            *expr = Expr::CompoundIdentifier(vec![Ident::new("excluded"), col]);
                        }
                    }
                }
            }
            _ => {}
        }
        ControlFlow::Continue(())
    }
}

fn rewrite_exprs<V: VisitMut>(node: &mut V) -> Result<()> {
    let mut rewriter = ExprRewriter { error: None };
    let _ = node.visit(&mut rewriter);
    match rewriter.error {
        Some(e) => err(e),
        None => Ok(()),
    }
}

/// Canonical datetime text is `YYYY-MM-DD HH:MM:SS[.fff]` with no trailing
/// zeros in the fraction, so that text comparison orders like MySQL's
/// temporal comparison. Returns `None` when `s` is not a datetime literal or
/// is already canonical.
pub fn normalize_datetime_literal(s: &str) -> Option<String> {
    let b = s.as_bytes();
    if b.len() < 19 || b.len() > 32 {
        return None;
    }
    let digit = |i: usize| b[i].is_ascii_digit();
    let shape = (0..4).all(digit)
        && b[4] == b'-'
        && digit(5)
        && digit(6)
        && b[7] == b'-'
        && digit(8)
        && digit(9)
        && (b[10] == b' ' || b[10] == b'T')
        && digit(11)
        && digit(12)
        && b[13] == b':'
        && digit(14)
        && digit(15)
        && b[16] == b':'
        && digit(17)
        && digit(18);
    if !shape {
        return None;
    }
    let mut rest = &s[19..];
    if let Some(r) = rest.strip_suffix('Z') {
        rest = r;
    }
    let frac = if rest.is_empty() {
        ""
    } else if let Some(f) = rest.strip_prefix('.') {
        if f.is_empty() || !f.bytes().all(|c| c.is_ascii_digit()) {
            return None;
        }
        f.trim_end_matches('0')
    } else {
        return None;
    };
    let mut out = String::with_capacity(26);
    out.push_str(&s[..10]);
    out.push(' ');
    out.push_str(&s[11..19]);
    if !frac.is_empty() {
        out.push('.');
        out.push_str(frac);
    }
    if out == s {
        None
    } else {
        Some(out)
    }
}

fn parse_expr(sql: &str) -> Result<Expr> {
    Parser::new(&GenericDialect {})
        .try_with_sql(sql)
        .and_then(|mut p| p.parse_expr())
        .map_err(|e| RewriteError(format!("internal expression `{sql}`: {e}")))
}

/// Build `name(expr, extra...)`.
fn wrap_fn(name: &str, expr: Expr, extra: &str) -> Result<Expr> {
    let mut call = parse_expr(&format!("{name}(NULL{extra})"))?;
    if let Expr::Function(f) = &mut call {
        if let FunctionArguments::List(list) = &mut f.args {
            list.args[0] = FunctionArg::Unnamed(FunctionArgExpr::Expr(expr));
        }
    }
    Ok(call)
}

/// Coerce a value being stored into `col`, the way MySQL does on assignment.
fn coerce_for_column(expr: Expr, col: &ColumnInfo) -> Result<Expr> {
    match col.ty {
        MyType::Datetime { fsp } | MyType::Timestamp { fsp } => {
            wrap_fn("mysql_datetime", expr, &format!(", {fsp}"))
        }
        MyType::Date => wrap_fn("mysql_date", expr, ""),
        MyType::Decimal { scale, .. } => wrap_fn("mysql_decimal", expr, &format!(", {scale}")),
        _ => Ok(expr),
    }
}

fn is_default_keyword(expr: &Expr) -> bool {
    matches!(expr, Expr::Identifier(id) if id.quote_style.is_none() && id.value.eq_ignore_ascii_case("default"))
}

// ---------------------------------------------------------------------------
// Row order
// ---------------------------------------------------------------------------

const AGGREGATES: &[&str] = &[
    "count",
    "sum",
    "avg",
    "min",
    "max",
    "group_concat",
    "json_arrayagg",
    "json_objectagg",
];

fn has_aggregate(select: &sqlparser::ast::Select) -> bool {
    let mut found = false;
    for item in &select.projection {
        let _ = sqlparser::ast::visit_expressions(item, |e| {
            if let Expr::Function(f) = e {
                if f.over.is_none()
                    && AGGREGATES.contains(&f.name.to_string().to_ascii_lowercase().as_str())
                {
                    found = true;
                }
            }
            ControlFlow::<()>::Continue(())
        });
    }
    found
}

/// The base table behind the first FROM item: the table itself, or the table
/// a `WITH x AS (SELECT * FROM t ...)` scope selects from.
fn driving_table(query: &Query, catalog: &Catalog) -> Option<(String, std::sync::Arc<TableInfo>)> {
    let SetExpr::Select(select) = query.body.as_ref() else {
        return None;
    };
    let TableFactor::Table { name, alias, .. } = &select.from.first()?.relation else {
        return None;
    };
    let table_name = object_name_last(name);
    let qualifier = alias
        .as_ref()
        .map(|a| a.name.value.clone())
        .unwrap_or_else(|| table_name.clone());
    let cte = query.with.as_ref().and_then(|w| {
        w.cte_tables
            .iter()
            .find(|c| c.alias.name.value.eq_ignore_ascii_case(&table_name))
    });
    let base = match cte {
        None => table_name,
        Some(cte) => {
            let SetExpr::Select(inner) = cte.query.body.as_ref() else {
                return None;
            };
            if !matches!(
                inner.projection.as_slice(),
                [sqlparser::ast::SelectItem::Wildcard(_)]
            ) || inner.from.len() != 1
                || !inner.from[0].joins.is_empty()
            {
                return None;
            }
            let TableFactor::Table { name, .. } = &inner.from[0].relation else {
                return None;
            };
            object_name_last(name)
        }
    };
    Some((qualifier, catalog.table(&base)?))
}

/// InnoDB tables are clustered on the primary key, so rows that a query does
/// not order — or that tie on its ORDER BY — come back in primary key order.
/// The engine's tables are not clustered; the primary key is appended to the
/// ordering to get the same result.
fn add_primary_key_order(query: &mut Query, catalog: &Catalog) {
    let Some((qualifier, table)) = driving_table(query, catalog) else {
        return;
    };
    let SetExpr::Select(select) = query.body.as_ref() else {
        return;
    };
    let grouped = match &select.group_by {
        sqlparser::ast::GroupByExpr::Expressions(exprs, _) => !exprs.is_empty(),
        _ => true,
    };
    if grouped || select.distinct.is_some() || has_aggregate(select) || table.primary_key.is_empty()
    {
        return;
    }
    let keys = table
        .primary_key
        .iter()
        .map(|c| {
            format!(
                "`{}`.`{}`",
                qualifier.replace('`', "``"),
                c.replace('`', "``")
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let Ok(mut parsed) = Parser::parse_sql(&MySqlDialect {}, &format!("SELECT 1 ORDER BY {keys}"))
    else {
        return;
    };
    let Some(Statement::Query(donor)) = parsed.pop() else {
        return;
    };
    let Some(sqlparser::ast::OrderBy {
        kind: sqlparser::ast::OrderByKind::Expressions(extra),
        ..
    }) = donor.order_by
    else {
        return;
    };
    match &mut query.order_by {
        Some(sqlparser::ast::OrderBy {
            kind: sqlparser::ast::OrderByKind::Expressions(existing),
            ..
        }) => existing.extend(extra),
        Some(_) => {}
        None => {
            query.order_by = Some(sqlparser::ast::OrderBy {
                kind: sqlparser::ast::OrderByKind::Expressions(extra),
                interpolate: None,
            })
        }
    }
}

// ---------------------------------------------------------------------------
// DML
// ---------------------------------------------------------------------------

fn object_name_last(name: &ObjectName) -> String {
    name.0
        .last()
        .and_then(|p| p.as_ident())
        .map(|i| i.value.clone())
        .unwrap_or_else(|| name.to_string())
}

fn coerce_assignments(assignments: &mut [Assignment], table: Option<&TableInfo>) -> Result<()> {
    let Some(table) = table else { return Ok(()) };
    for a in assignments {
        let AssignmentTarget::ColumnName(name) = &a.target else {
            continue;
        };
        let Some(col) = table.column(&object_name_last(name)) else {
            continue;
        };
        let value = std::mem::replace(&mut a.value, Expr::value(Value::Null));
        a.value = if is_default_keyword(&value) {
            default_expr(col)?
        } else {
            coerce_for_column(value, col)?
        };
    }
    Ok(())
}

fn default_expr(col: &ColumnInfo) -> Result<Expr> {
    match &col.default_sql {
        Some(sql) => parse_expr(sql),
        None => Ok(Expr::value(Value::Null)),
    }
}

fn insert(mut stmt: Statement, catalog: &Catalog) -> Result<Rewritten> {
    rewrite_exprs(&mut stmt)?;
    let Statement::Insert(ins) = &mut stmt else {
        unreachable!()
    };
    let table_name = match &ins.table {
        TableObject::TableName(name) => object_name_last(name),
        _ => return err("unsupported INSERT target"),
    };
    let table = catalog.table(&table_name);

    if ins.ignore {
        ins.ignore = false;
        ins.or = Some(SqliteOnConflict::Ignore);
    }

    coerce_insert_rows(ins, table.as_deref())?;

    let on_duplicate = match ins.on.take() {
        Some(OnInsert::DuplicateKeyUpdate(mut assignments)) => {
            coerce_assignments(&mut assignments, table.as_deref())?;
            Some(assignments)
        }
        other => {
            ins.on = other;
            None
        }
    };

    let mut sql = stmt.to_string();
    if let Some(assignments) = on_duplicate {
        let set = assignments
            .iter()
            .map(|a| a.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        sql.push_str(" ON CONFLICT DO UPDATE SET ");
        sql.push_str(&set);
    }
    Ok(Rewritten {
        kind: Kind::Dml,
        sql: vec![sql],
        table: None,
        auto_increment: table
            .as_deref()
            .is_some_and(|t| t.columns.iter().any(|c| c.auto_increment)),
        result_types: Vec::new(),
    })
}

fn coerce_insert_rows(ins: &mut Insert, table: Option<&TableInfo>) -> Result<()> {
    let Some(table) = table else { return Ok(()) };
    let Some(source) = ins.source.as_mut() else {
        return Ok(());
    };
    let SetExpr::Values(values) = source.body.as_mut() else {
        return Ok(());
    };
    let columns: Vec<Option<&ColumnInfo>> = if ins.columns.is_empty() {
        table.columns.iter().map(Some).collect()
    } else {
        ins.columns.iter().map(|c| table.column(&c.value)).collect()
    };
    for row in &mut values.rows {
        for (i, cell) in row.iter_mut().enumerate() {
            let Some(Some(col)) = columns.get(i) else {
                if is_default_keyword(cell) {
                    *cell = Expr::value(Value::Null);
                }
                continue;
            };
            let value = std::mem::replace(cell, Expr::value(Value::Null));
            *cell = if is_default_keyword(&value) {
                default_expr(col)?
            } else {
                coerce_for_column(value, col)?
            };
        }
    }
    Ok(())
}

fn update(mut stmt: Statement, catalog: &Catalog) -> Result<Rewritten> {
    rewrite_exprs(&mut stmt)?;
    let Statement::Update(upd) = &mut stmt else {
        unreachable!()
    };
    let table = match &upd.table.relation {
        TableFactor::Table { name, .. } => catalog.table(&object_name_last(name)),
        _ => None,
    };
    coerce_assignments(&mut upd.assignments, table.as_deref())?;
    Ok(Rewritten {
        kind: Kind::Dml,
        sql: vec![stmt.to_string()],
        table: None,
        auto_increment: false,
        result_types: Vec::new(),
    })
}

// ---------------------------------------------------------------------------
// DDL
// ---------------------------------------------------------------------------

/// MySQL's default collation (`utf8mb4_0900_ai_ci`) is the Unicode collation
/// algorithm at primary strength: case- and accent-insensitive.
const TEXT_COLLATION: &str = " COLLATE \"und-u-ks-level1\"";

pub fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn my_type(data_type: &DataType) -> Result<MyType> {
    let text = data_type.to_string().to_ascii_lowercase();
    let unsigned = text.contains("unsigned");
    let base = text.split(['(', ' ']).next().unwrap_or("");
    let args: Vec<u32> = text
        .split_once('(')
        .and_then(|(_, rest)| rest.split_once(')'))
        .map(|(inner, _)| {
            inner
                .split(',')
                .filter_map(|p| p.trim().parse().ok())
                .collect()
        })
        .unwrap_or_default();
    Ok(match base {
        "tinyint" => MyType::Int { bytes: 1, unsigned },
        "bool" | "boolean" => MyType::Int {
            bytes: 1,
            unsigned: false,
        },
        "smallint" => MyType::Int { bytes: 2, unsigned },
        "mediumint" => MyType::Int { bytes: 3, unsigned },
        "int" | "integer" => MyType::Int { bytes: 4, unsigned },
        "bigint" => MyType::Int { bytes: 8, unsigned },
        "decimal" | "numeric" | "dec" => MyType::Decimal {
            precision: args.first().copied().unwrap_or(10),
            scale: args.get(1).copied().unwrap_or(0),
        },
        "float" => MyType::Float,
        "double" | "real" => MyType::Double,
        "char" | "character" => MyType::Char,
        "varchar" | "nvarchar" => MyType::Varchar,
        "text" | "tinytext" | "mediumtext" | "longtext" => MyType::Text,
        "binary" | "varbinary" => MyType::Binary,
        "blob" | "tinyblob" | "mediumblob" | "longblob" => MyType::Blob,
        "datetime" => MyType::Datetime {
            fsp: args.first().copied().unwrap_or(0),
        },
        "timestamp" => MyType::Timestamp {
            fsp: args.first().copied().unwrap_or(0),
        },
        "date" => MyType::Date,
        "time" => MyType::Time,
        "json" => MyType::Json,
        "enum" | "set" => MyType::Enum,
        other => return err(format!("unsupported column type `{other}`")),
    })
}

fn default_sql(expr: &Expr) -> String {
    let text = expr.to_string();
    let lower = text.to_ascii_lowercase();
    let unwrapped = lower.trim_matches(['(', ')']);
    if unwrapped == "now" || unwrapped == "current_timestamp" || lower == "now()" {
        return "CURRENT_TIMESTAMP".to_string();
    }
    if let Some(hex) = lower.strip_prefix("0x") {
        return format!("X'{hex}'");
    }
    match expr {
        Expr::Value(_) | Expr::UnaryOp { .. } => text,
        _ => format!("({text})"),
    }
}

fn create_table(ct: &CreateTable) -> Result<Rewritten> {
    let table_name = object_name_last(&ct.name);
    let mut columns = Vec::new();
    let mut defs = Vec::new();
    let mut indexes = Vec::new();

    // A single-column integer AUTO_INCREMENT key must be declared inline to
    // become the rowid alias.
    let mut primary_key: Vec<String> = Vec::new();
    let mut pk_columns: Vec<String> = Vec::new();
    for c in &ct.constraints {
        if let TableConstraint::PrimaryKey(pk) = c {
            pk_columns = pk
                .columns
                .iter()
                .map(|c| index_column_name(&c.column.expr))
                .collect();
        }
    }

    for col in &ct.columns {
        let ty = my_type(&col.data_type)?;
        let mut not_null = false;
        let mut default = None;
        let mut auto_increment = false;
        let mut inline_pk = false;
        let mut binary_collation = false;
        for opt in &col.options {
            match &opt.option {
                ColumnOption::NotNull => not_null = true,
                ColumnOption::Default(e) => default = Some(default_sql(e)),
                ColumnOption::PrimaryKey(_) => inline_pk = true,
                ColumnOption::Collation(name) => {
                    binary_collation = name.to_string().to_ascii_lowercase().ends_with("_bin")
                }
                ColumnOption::DialectSpecific(tokens) => {
                    if tokens
                        .iter()
                        .any(|t| t.to_string().eq_ignore_ascii_case("AUTO_INCREMENT"))
                    {
                        auto_increment = true;
                    }
                }
                _ => {}
            }
        }
        if col
            .data_type
            .to_string()
            .to_ascii_lowercase()
            .contains("_bin")
        {
            binary_collation = true;
        }
        if inline_pk {
            pk_columns = vec![col.name.value.clone()];
        }
        if primary_key.is_empty() && !pk_columns.is_empty() {
            primary_key.clone_from(&pk_columns);
        }
        let is_rowid_alias = auto_increment
            || (pk_columns.len() == 1
                && pk_columns[0].eq_ignore_ascii_case(&col.name.value)
                && matches!(ty, MyType::Int { .. }));

        let mut def = format!("{} {}", quote_ident(&col.name.value), ty.native_decl());
        if is_rowid_alias {
            def = format!("{} INTEGER PRIMARY KEY", quote_ident(&col.name.value));
            if auto_increment {
                def.push_str(" AUTOINCREMENT");
            }
        } else {
            if ty.is_text() && !binary_collation {
                def.push_str(TEXT_COLLATION);
            }
            if not_null {
                def.push_str(" NOT NULL");
            }
            if let Some(d) = &default {
                def.push_str(" DEFAULT ");
                def.push_str(d);
            }
        }
        defs.push(def);
        columns.push(ColumnInfo {
            name: col.name.value.clone(),
            ty,
            not_null,
            auto_increment,
            default_sql: if is_rowid_alias { None } else { default },
        });
        if is_rowid_alias {
            pk_columns.clear();
        }
    }

    for c in &ct.constraints {
        match c {
            TableConstraint::PrimaryKey(_) => {
                if !pk_columns.is_empty() {
                    let cols = pk_columns
                        .iter()
                        .map(|c| quote_ident(c))
                        .collect::<Vec<_>>()
                        .join(", ");
                    defs.push(format!("PRIMARY KEY ({cols})"));
                }
            }
            TableConstraint::Unique(u) => {
                let name = u.index_name.as_ref().or(u.name.as_ref());
                indexes.push(index_sql(
                    &table_name,
                    name.map(|n| n.value.as_str()),
                    &u.columns,
                    true,
                    indexes.len(),
                ));
            }
            TableConstraint::Index(i) => {
                indexes.push(index_sql(
                    &table_name,
                    i.name.as_ref().map(|n| n.value.as_str()),
                    &i.columns,
                    false,
                    indexes.len(),
                ));
            }
            _ => {}
        }
    }
    if pk_columns.len() == 1
        && !ct
            .constraints
            .iter()
            .any(|c| matches!(c, TableConstraint::PrimaryKey(_)))
    {
        defs.push(format!("PRIMARY KEY ({})", quote_ident(&pk_columns[0])));
    }

    let mut sql = vec![format!(
        "CREATE TABLE {}{} ({})",
        if ct.if_not_exists {
            "IF NOT EXISTS "
        } else {
            ""
        },
        quote_ident(&table_name),
        defs.join(", ")
    )];
    sql.extend(indexes);

    Ok(Rewritten {
        kind: Kind::Ddl,
        sql,
        table: Some(
            TableInfo {
                name: table_name,
                columns,
                primary_key,
                by_name: HashMap::new(),
            }
            .indexed(),
        ),
        auto_increment: false,
        result_types: Vec::new(),
    })
}

/// MySQL prefix indexes (`col(3)`) parse as a function call; the indexed
/// column is the function name.
fn index_column_name(expr: &Expr) -> String {
    match expr {
        Expr::Identifier(id) => id.value.clone(),
        Expr::Function(f) => object_name_last(&f.name),
        other => other.to_string(),
    }
}

fn index_sql(
    table: &str,
    name: Option<&str>,
    columns: &[sqlparser::ast::IndexColumn],
    unique: bool,
    ordinal: usize,
) -> String {
    let cols = columns
        .iter()
        .map(|c| quote_ident(&index_column_name(&c.column.expr)))
        .collect::<Vec<_>>()
        .join(", ");
    // Index names are per-table in MySQL but global natively.
    let index_name = match name {
        Some(n) => format!("{table}__{n}"),
        None => format!("{table}__idx{ordinal}"),
    };
    format!(
        "CREATE {}INDEX IF NOT EXISTS {} ON {} ({})",
        if unique { "UNIQUE " } else { "" },
        quote_ident(&index_name),
        quote_ident(table),
        cols
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn datetime_literals() {
        assert_eq!(
            normalize_datetime_literal("2026-10-01T20:30:00.000Z").as_deref(),
            Some("2026-10-01 20:30:00")
        );
        assert_eq!(
            normalize_datetime_literal("2026-10-02 16:02:43.985").as_deref(),
            None
        );
        assert_eq!(
            normalize_datetime_literal("2026-10-02 16:02:43.980").as_deref(),
            Some("2026-10-02 16:02:43.98")
        );
        assert_eq!(normalize_datetime_literal("hello"), None);
    }
}
