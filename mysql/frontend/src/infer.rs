//! Result type inference.
//!
//! MySQL decides a result column's type from the expression, and the type is
//! visible to clients: `SUM()` over integers is a `DECIMAL` (a string in most
//! drivers), `/` adds four fractional digits, and so on. The engine only
//! knows storage classes, so the type of every projected expression is
//! derived here, from the MySQL statement and the column types in the
//! catalog.

use std::collections::HashMap;

use sqlparser::ast::{
    BinaryOperator, DataType, Expr, FunctionArg, FunctionArgExpr, FunctionArguments, Query, Select,
    SelectItem, SelectItemQualifiedWildcardKind, SetExpr, TableFactor, UnaryOperator, Value,
};

use crate::catalog::{Catalog, MyType};

type Column = (String, Option<MyType>);

#[derive(Clone)]
struct Relation {
    name: String,
    columns: Vec<Column>,
}

#[derive(Clone, Default)]
struct Env<'a> {
    ctes: HashMap<String, Vec<Column>>,
    /// Innermost scope last.
    scopes: Vec<Vec<Relation>>,
    catalog: Option<&'a Catalog>,
}

const BIGINT: MyType = MyType::Int {
    bytes: 8,
    unsigned: false,
};

fn decimal(scale: u32) -> MyType {
    MyType::Decimal {
        precision: 65,
        scale: scale.min(30),
    }
}

/// Types of the columns a top-level query returns.
pub fn result_types(query: &Query, catalog: &Catalog) -> Vec<Option<MyType>> {
    let env = Env {
        catalog: Some(catalog),
        ..Env::default()
    };
    query_columns(query, &env)
        .into_iter()
        .map(|(_, ty)| ty)
        .collect()
}

fn ident_key(name: &str) -> String {
    name.to_ascii_lowercase()
}

fn query_columns(query: &Query, outer: &Env) -> Vec<Column> {
    let mut env = outer.clone();
    if let Some(with) = &query.with {
        for cte in &with.cte_tables {
            let columns = query_columns(&cte.query, &env);
            env.ctes.insert(ident_key(&cte.alias.name.value), columns);
        }
    }
    set_expr_columns(&query.body, &env)
}

fn set_expr_columns(body: &SetExpr, env: &Env) -> Vec<Column> {
    match body {
        SetExpr::Select(select) => select_columns(select, env),
        SetExpr::Query(query) => query_columns(query, env),
        SetExpr::SetOperation { left, .. } => set_expr_columns(left, env),
        _ => Vec::new(),
    }
}

fn relation(factor: &TableFactor, env: &Env) -> Option<Relation> {
    match factor {
        TableFactor::Table { name, alias, .. } => {
            let table_name = name.0.last()?.as_ident()?.value.clone();
            let key = ident_key(&table_name);
            let columns = match env.ctes.get(&key) {
                Some(columns) => columns.clone(),
                None => env
                    .catalog?
                    .table(&table_name)?
                    .columns
                    .iter()
                    .map(|c| (c.name.clone(), Some(c.ty)))
                    .collect(),
            };
            Some(Relation {
                name: alias
                    .as_ref()
                    .map(|a| a.name.value.clone())
                    .unwrap_or(table_name),
                columns,
            })
        }
        TableFactor::Derived {
            subquery, alias, ..
        } => Some(Relation {
            name: alias
                .as_ref()
                .map(|a| a.name.value.clone())
                .unwrap_or_default(),
            columns: query_columns(subquery, env),
        }),
        _ => None,
    }
}

fn select_columns(select: &Select, outer: &Env) -> Vec<Column> {
    let mut relations = Vec::new();
    for from in &select.from {
        relations.extend(relation(&from.relation, outer));
        for join in &from.joins {
            relations.extend(relation(&join.relation, outer));
        }
    }
    let mut env = outer.clone();
    env.scopes.push(relations);
    let scope = env.scopes.last().expect("scope pushed");

    let mut out = Vec::new();
    for item in &select.projection {
        match item {
            SelectItem::Wildcard(_) => {
                for rel in scope {
                    out.extend(rel.columns.iter().cloned());
                }
            }
            SelectItem::QualifiedWildcard(kind, _) => {
                if let SelectItemQualifiedWildcardKind::ObjectName(name) = kind {
                    let wanted = name
                        .0
                        .last()
                        .and_then(|p| p.as_ident())
                        .map(|i| ident_key(&i.value))
                        .unwrap_or_default();
                    for rel in scope.iter().filter(|r| ident_key(&r.name) == wanted) {
                        out.extend(rel.columns.iter().cloned());
                    }
                }
            }
            SelectItem::UnnamedExpr(expr) => {
                let name = match expr {
                    Expr::Identifier(id) => id.value.clone(),
                    Expr::CompoundIdentifier(ids) => {
                        ids.last().map(|i| i.value.clone()).unwrap_or_default()
                    }
                    other => other.to_string(),
                };
                out.push((name, infer(expr, &env)));
            }
            SelectItem::ExprWithAlias { expr, alias } => {
                out.push((alias.value.clone(), infer(expr, &env)));
            }
        }
    }
    out
}

fn lookup(env: &Env, qualifier: Option<&str>, column: &str) -> Option<MyType> {
    let column = ident_key(column);
    let qualifier = qualifier.map(ident_key);
    for scope in env.scopes.iter().rev() {
        for rel in scope {
            if qualifier
                .as_ref()
                .is_some_and(|q| *q != ident_key(&rel.name))
            {
                continue;
            }
            if let Some((_, ty)) = rel.columns.iter().find(|(n, _)| ident_key(n) == column) {
                return *ty;
            }
        }
    }
    None
}

fn is_exact(ty: MyType) -> bool {
    matches!(ty, MyType::Int { .. } | MyType::Decimal { .. })
}

fn scale_of(ty: MyType) -> u32 {
    match ty {
        MyType::Decimal { scale, .. } => scale,
        _ => 0,
    }
}

fn is_temporal(ty: MyType) -> bool {
    matches!(
        ty,
        MyType::Datetime { .. } | MyType::Timestamp { .. } | MyType::Date | MyType::Time
    )
}

/// Result type of `a <op> b` for `+`, `-`, `*`.
fn arithmetic(a: MyType, b: MyType, multiply: bool) -> MyType {
    if !is_exact(a) || !is_exact(b) {
        return MyType::Double;
    }
    match (a, b) {
        (MyType::Int { .. }, MyType::Int { .. }) => BIGINT,
        _ if multiply => decimal(scale_of(a) + scale_of(b)),
        _ => decimal(scale_of(a).max(scale_of(b))),
    }
}

/// Type of an expression that yields one of several branches
/// (`CASE`, `COALESCE`, `IF`, `IFNULL`).
fn unify(types: impl IntoIterator<Item = Option<MyType>>) -> Option<MyType> {
    let mut result: Option<MyType> = None;
    for ty in types {
        // An untyped branch (NULL, a parameter) does not constrain the result.
        let Some(ty) = ty else { continue };
        result = Some(match result {
            None => ty,
            Some(prev) if prev == ty => ty,
            Some(prev) if is_exact(prev) && is_exact(ty) => match (prev, ty) {
                (MyType::Int { .. }, MyType::Int { .. }) => BIGINT,
                _ => decimal(scale_of(prev).max(scale_of(ty))),
            },
            Some(prev) => {
                let numeric =
                    |t: MyType| is_exact(t) || matches!(t, MyType::Float | MyType::Double);
                if numeric(prev) && numeric(ty) {
                    MyType::Double
                } else if is_temporal(prev) && is_temporal(ty) {
                    MyType::Datetime {
                        fsp: fsp_of(prev).max(fsp_of(ty)),
                    }
                } else {
                    MyType::Varchar
                }
            }
        });
    }
    result
}

fn fsp_of(ty: MyType) -> u32 {
    match ty {
        MyType::Datetime { fsp } | MyType::Timestamp { fsp } => fsp,
        _ => 0,
    }
}

fn function_args(args: &FunctionArguments) -> Vec<&Expr> {
    match args {
        FunctionArguments::List(list) => list
            .args
            .iter()
            .filter_map(|arg| match arg {
                FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Some(e),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn literal_int(expr: &Expr) -> Option<i64> {
    match expr {
        Expr::Value(v) => match &v.value {
            Value::Number(n, _) => n.parse().ok(),
            _ => None,
        },
        _ => None,
    }
}

fn cast_type(data_type: &DataType) -> Option<MyType> {
    let text = data_type.to_string().to_ascii_lowercase();
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
    Some(match base {
        "decimal" | "numeric" | "dec" => decimal(args.get(1).copied().unwrap_or(0)),
        "double" | "float" | "real" => MyType::Double,
        "signed" | "unsigned" | "int" | "integer" | "bigint" => BIGINT,
        "char" | "varchar" | "nchar" => MyType::Varchar,
        "binary" => MyType::Binary,
        "datetime" => MyType::Datetime {
            fsp: args.first().copied().unwrap_or(0),
        },
        "date" => MyType::Date,
        "json" => MyType::Json,
        _ => return None,
    })
}

fn infer(expr: &Expr, env: &Env) -> Option<MyType> {
    match expr {
        Expr::Identifier(id) => lookup(env, None, &id.value),
        Expr::CompoundIdentifier(ids) => {
            let column = ids.last()?;
            let qualifier = ids.len().checked_sub(2).map(|i| ids[i].value.as_str());
            lookup(env, qualifier, &column.value)
        }
        Expr::Nested(inner) => infer(inner, env),
        Expr::Value(v) => match &v.value {
            Value::Number(n, _) => Some(match n.split_once('.') {
                Some((_, frac)) if !n.contains(['e', 'E']) => decimal(frac.len() as u32),
                Some(_) => MyType::Double,
                None => BIGINT,
            }),
            Value::Boolean(_) => Some(BIGINT),
            Value::SingleQuotedString(_) | Value::DoubleQuotedString(_) => Some(MyType::Varchar),
            Value::HexStringLiteral(_) => Some(MyType::Binary),
            _ => None,
        },
        Expr::UnaryOp { op, expr } => match op {
            UnaryOperator::Minus | UnaryOperator::Plus => infer(expr, env).map(|t| {
                if is_exact(t) {
                    match t {
                        MyType::Int { .. } => BIGINT,
                        other => other,
                    }
                } else {
                    MyType::Double
                }
            }),
            UnaryOperator::Not => Some(BIGINT),
            _ => None,
        },
        Expr::BinaryOp { left, op, right } => {
            let (a, b) = (infer(left, env), infer(right, env));
            match op {
                BinaryOperator::Plus | BinaryOperator::Minus | BinaryOperator::Multiply => {
                    Some(arithmetic(a?, b?, *op == BinaryOperator::Multiply))
                }
                BinaryOperator::Divide => {
                    let (a, b) = (a?, b?);
                    Some(if is_exact(a) && is_exact(b) {
                        // div_precision_increment
                        decimal(scale_of(a) + 4)
                    } else {
                        MyType::Double
                    })
                }
                BinaryOperator::Modulo => Some(arithmetic(a?, b?, false)),
                BinaryOperator::Eq
                | BinaryOperator::NotEq
                | BinaryOperator::Lt
                | BinaryOperator::LtEq
                | BinaryOperator::Gt
                | BinaryOperator::GtEq
                | BinaryOperator::And
                | BinaryOperator::Or
                | BinaryOperator::Xor
                | BinaryOperator::MyIntegerDivide => Some(BIGINT),
                _ => None,
            }
        }
        Expr::IsNull(_)
        | Expr::IsNotNull(_)
        | Expr::IsTrue(_)
        | Expr::IsFalse(_)
        | Expr::IsNotTrue(_)
        | Expr::IsNotFalse(_)
        | Expr::InList { .. }
        | Expr::InSubquery { .. }
        | Expr::Between { .. }
        | Expr::Like { .. }
        | Expr::Exists { .. } => Some(BIGINT),
        Expr::Case {
            conditions,
            else_result,
            ..
        } => unify(
            conditions
                .iter()
                .map(|c| infer(&c.result, env))
                .chain(else_result.iter().map(|e| infer(e, env))),
        ),
        Expr::Cast { data_type, .. } => cast_type(data_type),
        Expr::Subquery(query) => query_columns(query, env).into_iter().next()?.1,
        Expr::Function(f) => {
            let name = f.name.to_string().to_ascii_lowercase();
            let args = function_args(&f.args);
            let arg = |i: usize| args.get(i).and_then(|e| infer(e, env));
            match name.as_str() {
                "count" | "row_number" | "rank" | "dense_rank" | "datediff" | "unix_timestamp"
                | "length" | "char_length" | "instr" | "sign" | "exists" => Some(BIGINT),
                "sum" => Some(match arg(0)? {
                    MyType::Int { .. } => decimal(0),
                    MyType::Decimal { scale, .. } => decimal(scale),
                    _ => MyType::Double,
                }),
                "avg" => Some(match arg(0)? {
                    t if is_exact(t) => decimal(scale_of(t) + 4),
                    _ => MyType::Double,
                }),
                "max" | "min" | "abs" | "any_value" => arg(0),
                "coalesce" | "ifnull" | "greatest" | "least" => unify((0..args.len()).map(arg)),
                "if" => unify([arg(1), arg(2)]),
                "nullif" => arg(0),
                "truncate" | "round" => {
                    let digits = args.get(1).and_then(|e| literal_int(e)).unwrap_or(0).max(0);
                    Some(match arg(0)? {
                        MyType::Int { .. } => BIGINT,
                        MyType::Decimal { scale, .. } => decimal(scale.min(digits as u32)),
                        _ => MyType::Double,
                    })
                }
                "floor" | "ceil" | "ceiling" => Some(match arg(0)? {
                    t if is_exact(t) => BIGINT,
                    _ => MyType::Double,
                }),
                "now" | "current_timestamp" | "localtime" | "localtimestamp" | "sysdate" => {
                    Some(MyType::Datetime {
                        fsp: args.first().and_then(|e| literal_int(e)).unwrap_or(0) as u32,
                    })
                }
                "from_unixtime" => Some(MyType::Datetime { fsp: 0 }),
                "curdate" | "current_date" | "date" => Some(MyType::Date),
                "hex" | "lower" | "upper" | "concat" | "conv" | "substr" | "substring" | "trim"
                | "replace" | "date_format" | "left" | "right" => match arg(0) {
                    // SUBSTR of a binary string stays binary.
                    Some(MyType::Binary | MyType::Blob)
                        if matches!(name.as_str(), "substr" | "substring" | "left" | "right") =>
                    {
                        Some(MyType::Binary)
                    }
                    _ => Some(MyType::Varchar),
                },
                "json_array" | "json_object" | "json_extract" => Some(MyType::Json),
                _ => None,
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rewrite::rewrite;
    use sqlparser::ast::Statement;
    use sqlparser::dialect::MySqlDialect;
    use sqlparser::parser::Parser;

    fn types(catalog: &Catalog, sql: &str) -> Vec<Option<MyType>> {
        let stmt = Parser::parse_sql(&MySqlDialect {}, sql).unwrap().remove(0);
        let Statement::Query(q) = stmt else { panic!() };
        result_types(&q, catalog)
    }

    #[test]
    fn mysql_expression_types() {
        let catalog = Catalog::new();
        for r in rewrite(
            "CREATE TABLE r (id binary(12) NOT NULL, amount decimal(17,3) NOT NULL, price bigint NOT NULL, \
             discount bigint, at datetime, PRIMARY KEY (id))",
            &catalog,
        )
        .unwrap()
        {
            catalog.register(r.table.unwrap());
        }
        let got = types(
            &catalog,
            "with `$r` as (select * from r where id = X'00') select sum(`price`) as a, sum(amount), \
             (`$r`.`amount` * `$r`.`price` - COALESCE(`$r`.`discount`, 0)) / `amount`, count(*), \
             coalesce(`at`, from_unixtime(1)), price / 2, cast(sum(price) as decimal(17,3)), -(`price`) from `$r`",
        );
        assert_eq!(
            got,
            vec![
                Some(decimal(0)),
                Some(decimal(3)),
                Some(decimal(7)),
                Some(BIGINT),
                Some(MyType::Datetime { fsp: 0 }),
                Some(decimal(4)),
                Some(decimal(3)),
                Some(BIGINT),
            ]
        );
    }
}
