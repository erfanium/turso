//! The MySQL [`Dialect`].
//!
//! Statements reach the engine already rewritten to native SQL (see
//! [`crate::rewrite`]), so parsing and schema persistence are the native
//! ones. What the dialect adds is MySQL's scalar function surface.

use std::sync::Arc;

use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use turso_core::schema::{BTreeTable, Schema};
use turso_core::{Connection, Database, Dialect, Func, LimboError, Result, Value};

#[derive(Debug)]
pub struct MysqlDialect;

impl Dialect for MysqlDialect {
    fn name(&self) -> &'static str {
        "mysql"
    }

    fn parse(&self, sql: &str) -> Result<(Option<turso_parser::ast::Cmd>, usize)> {
        turso_core::dialect::sqlite::parse(sql)
    }

    fn parse_table_sql(&self, sql: &str, root_page: i64) -> Result<BTreeTable> {
        BTreeTable::from_sql(sql, root_page)
    }

    fn parse_table_sql_ast(&self, sql: &str) -> Result<turso_parser::ast::Stmt> {
        turso_core::dialect::sqlite::parse_table_sql_ast(sql)
    }

    fn table_sql_for_replay(&self, sql: &str) -> Result<String> {
        turso_core::dialect::sqlite::table_sql_for_replay(sql)
    }

    fn format_table_sql(
        &self,
        _input: &str,
        tbl_name: &turso_parser::ast::QualifiedName,
        body: &turso_parser::ast::CreateTableBody,
    ) -> Result<String> {
        Ok(format!(
            "CREATE TABLE {} {}",
            tbl_name.name.as_ident(),
            body
        ))
    }

    fn register_catalog(&self, schema: &mut Schema, enable_custom_types: bool) -> Result<()> {
        turso_core::dialect::sqlite::register_builtin_catalog(schema, enable_custom_types)
    }

    fn resolve_function(&self, name: &str, arg_count: usize) -> Result<Option<Func>> {
        let lower = name.to_ascii_lowercase();
        if is_mysql_function(&lower, arg_count) {
            return Ok(Some(Func::Dialect(lower)));
        }
        turso_core::dialect::sqlite::resolve_builtin_function(name, arg_count)
    }

    fn exec_scalar_function(
        &self,
        _conn: &Connection,
        name: &str,
        args: &[Value],
    ) -> Result<Value> {
        exec(name, args)
    }
}

/// Open a fresh in-memory database that speaks the MySQL dialect.
pub fn open_memory_database(name: &str) -> Result<Arc<Database>> {
    // The engine shares open databases by path, so a dropped database that a
    // session still holds would come back under its name. Every database
    // instance gets its own path.
    static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let io: Arc<dyn turso_core::IO> = Arc::new(turso_core::MemoryIO::new());
    let path = format!("mysql-{name}-{id}.db");
    let flags = turso_core::OpenFlags::default();
    let file = io.open_file(&path, flags, true)?;
    let db_file = Arc::new(turso_core::storage::database::DatabaseFile::new(file));
    Database::open(
        io,
        &path,
        turso_core::OpenOptions::new(Arc::new(MysqlDialect))
            .storage(db_file)
            .flags(flags),
    )
}

fn is_mysql_function(name: &str, argc: usize) -> bool {
    let arities: &[usize] = match name {
        "now" | "current_timestamp" | "localtime" | "localtimestamp" | "sysdate" => &[0, 1],
        "curdate" | "current_date" | "utc_date" => &[0],
        "datediff" => &[2],
        "unix_timestamp" => &[0, 1],
        "from_unixtime" => &[1],
        "conv" => &[3],
        "truncate" => &[2],
        "mysql_datetime" => &[2],
        "mysql_date" => &[1],
        "mysql_time" => &[2],
        "mysql_year" => &[1],
        "mysql_decimal" => &[2],
        _ => return false,
    };
    arities.contains(&argc)
}

const DATETIME_FMT: &str = "%Y-%m-%d %H:%M:%S";

fn text(s: impl Into<String>) -> Value {
    Value::build_text(s.into())
}

fn arg_text(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::Text(t) => Some(t.as_str().to_string()),
        Value::Blob(_) => None,
        other => Some(other.to_string()),
    }
}

fn arg_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Null | Value::Blob(_) => None,
        Value::Text(t) => t.as_str().trim().parse().ok(),
        other => Some(other.as_float()),
    }
}

fn arg_i64(v: &Value) -> Option<i64> {
    match v {
        Value::Null | Value::Blob(_) => None,
        Value::Text(t) => {
            let s = t.as_str().trim();
            s.parse::<i64>()
                .ok()
                .or_else(|| s.parse::<f64>().ok().map(|f| f as i64))
        }
        other => other.as_int().or_else(|| Some(other.as_float() as i64)),
    }
}

/// Parse the temporal spellings MySQL accepts and drivers produce.
pub fn parse_datetime(s: &str) -> Option<NaiveDateTime> {
    let s = s.trim().trim_end_matches('Z');
    for fmt in [
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M",
    ] {
        if let Ok(dt) = NaiveDateTime::parse_from_str(s, fmt) {
            return Some(dt);
        }
    }
    NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.and_hms_opt(0, 0, 0))
}

/// Render `dt` rounded to `fsp` fractional digits, without trailing zeros,
/// which is the canonical stored form (see `rewrite::normalize_datetime_literal`).
/// Parse a MySQL TIME value (`[-]H:MM:SS[.frac]`, a datetime, or an
/// `HHMMSS` number) into signed microseconds.
pub fn parse_time_micros(s: &str) -> Option<i64> {
    let s = s.trim();
    if let Some(dt) = parse_datetime(s).filter(|_| s.contains('-') && s.len() > 8) {
        let t = dt.time();
        use chrono::Timelike;
        return Some(
            (t.num_seconds_from_midnight() as i64) * 1_000_000 + (t.nanosecond() / 1000) as i64,
        );
    }
    let (neg, body) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s),
    };
    let (whole, frac) = body.split_once('.').unwrap_or((body, ""));
    let parts: Vec<&str> = whole.split(':').collect();
    let num = |p: &str| p.parse::<i64>().ok();
    let (h, m, sec) = match parts.as_slice() {
        [h, m, sec] => (num(h)?, num(m)?, num(sec)?),
        [h, m] => (num(h)?, num(m)?, 0),
        [n] => {
            let n = num(n)?;
            (n / 10000, n / 100 % 100, n % 100)
        }
        _ => return None,
    };
    if m > 59 || sec > 59 || !frac.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let mut digits: String = frac.chars().take(7).collect();
    while digits.len() < 7 {
        digits.push('0');
    }
    // Seven digits, so the sub-microsecond digit rounds.
    let frac_micros = (digits.parse::<i64>().unwrap_or(0) + 5) / 10;
    let micros = ((h * 60 + m) * 60 + sec) * 1_000_000 + frac_micros;
    Some(if neg { -micros } else { micros })
}

/// Format signed microseconds as a TIME value with `fsp` fractional digits.
pub fn format_time(micros: i64, fsp: u32) -> String {
    let unit = 10i64.pow(6 - fsp);
    let abs = micros.abs();
    let rounded = (abs + unit / 2) / unit * unit;
    let secs = rounded / 1_000_000;
    let mut out = format!(
        "{}{:02}:{:02}:{:02}",
        if micros < 0 { "-" } else { "" },
        secs / 3600,
        secs / 60 % 60,
        secs % 60
    );
    if fsp > 0 {
        let frac = (rounded % 1_000_000) / unit;
        out.push_str(&format!(".{frac:0width$}", width = fsp as usize));
    }
    out
}

pub fn format_datetime(dt: NaiveDateTime, fsp: u32) -> String {
    let fsp = fsp.min(6);
    let unit = 10i64.pow(6 - fsp);
    let micros = dt.and_utc().timestamp_micros();
    // MySQL rounds half up when a value has more fractional digits than the
    // column stores.
    let rounded = (micros + unit / 2).div_euclid(unit) * unit;
    let dt = DateTime::<Utc>::from_timestamp_micros(rounded)
        .map(|d| d.naive_utc())
        .unwrap_or(dt);
    let mut out = dt.format(DATETIME_FMT).to_string();
    let frac = dt.and_utc().timestamp_subsec_micros();
    if frac != 0 {
        let digits = format!("{frac:06}");
        out.push('.');
        out.push_str(digits.trim_end_matches('0'));
    }
    out
}

fn exec(name: &str, args: &[Value]) -> Result<Value> {
    let arg = |i: usize| args.get(i).unwrap_or(&Value::Null);
    Ok(match name {
        "now" | "current_timestamp" | "localtime" | "localtimestamp" | "sysdate" => {
            let fsp = arg_i64(arg(0)).unwrap_or(0) as u32;
            let now = Utc::now().naive_utc();
            if fsp == 0 {
                // NOW() truncates rather than rounds.
                text(now.format(DATETIME_FMT).to_string())
            } else {
                text(format_datetime(now, fsp))
            }
        }
        "curdate" | "current_date" | "utc_date" => {
            text(Utc::now().naive_utc().format("%Y-%m-%d").to_string())
        }
        "datediff" => {
            let a = arg_text(arg(0)).and_then(|s| parse_datetime(&s));
            let b = arg_text(arg(1)).and_then(|s| parse_datetime(&s));
            match (a, b) {
                (Some(a), Some(b)) => Value::from_i64((a.date() - b.date()).num_days()),
                _ => Value::Null,
            }
        }
        "unix_timestamp" => {
            if args.is_empty() {
                Value::from_i64(Utc::now().timestamp())
            } else {
                match arg_text(arg(0)).and_then(|s| parse_datetime(&s)) {
                    Some(dt) => Value::from_i64(dt.and_utc().timestamp()),
                    None => Value::Null,
                }
            }
        }
        "from_unixtime" => match arg_f64(arg(0)) {
            Some(secs) => {
                let micros = (secs * 1_000_000.0).round() as i64;
                match DateTime::<Utc>::from_timestamp_micros(micros) {
                    Some(dt) => text(format_datetime(dt.naive_utc(), 6)),
                    None => Value::Null,
                }
            }
            None => Value::Null,
        },
        "conv" => {
            let (Some(s), Some(from), Some(to)) =
                (arg_text(arg(0)), arg_i64(arg(1)), arg_i64(arg(2)))
            else {
                return Ok(Value::Null);
            };
            if !(2..=36).contains(&from) || !(2..=36).contains(&to) {
                return Ok(Value::Null);
            }
            let n = u64::from_str_radix(s.trim(), from as u32).unwrap_or(0);
            text(to_radix(n, to as u32))
        }
        "truncate" => {
            let (Some(x), Some(d)) = (arg_f64(arg(0)), arg_i64(arg(1))) else {
                return Ok(Value::Null);
            };
            let factor = 10f64.powi(d as i32);
            let truncated = (x * factor).trunc() / factor;
            if d <= 0 {
                Value::from_i64(truncated as i64)
            } else {
                text(format!("{truncated:.*}", d as usize))
            }
        }
        "mysql_datetime" => {
            let fsp = arg_i64(arg(1)).unwrap_or(0) as u32;
            match arg(0) {
                Value::Text(t) => match parse_datetime(t.as_str()) {
                    Some(dt) => text(format_datetime(dt, fsp)),
                    None => arg(0).clone(),
                },
                other => other.clone(),
            }
        }
        "mysql_date" => match arg(0) {
            Value::Text(t) => match parse_datetime(t.as_str()) {
                Some(dt) => text(dt.format("%Y-%m-%d").to_string()),
                None => arg(0).clone(),
            },
            other => other.clone(),
        },
        "mysql_time" => {
            let fsp = arg_i64(arg(1)).unwrap_or(0).clamp(0, 6) as u32;
            match arg(0) {
                Value::Null | Value::Blob(_) => arg(0).clone(),
                v => match arg_text(v).and_then(|t| parse_time_micros(&t)) {
                    Some(micros) => text(format_time(micros, fsp)),
                    None => v.clone(),
                },
            }
        }
        "mysql_year" => match arg(0) {
            Value::Null | Value::Blob(_) => arg(0).clone(),
            v => {
                let raw = arg_text(v).unwrap_or_default();
                let raw = raw.trim();
                match raw.parse::<f64>() {
                    // A numeric 0 is the zero year; the string '0' or '00' is 2000.
                    Ok(n) if n == 0.0 && !matches!(v, Value::Text(_)) => Value::from_i64(0),
                    Ok(n) => {
                        let n = n.round() as i64;
                        let two_digit = raw.trim_start_matches('-').split('.').next().unwrap_or("").len() <= 2;
                        Value::from_i64(match n {
                            0..=69 if two_digit => 2000 + n,
                            70..=99 if two_digit => 1900 + n,
                            n => n,
                        })
                    }
                    Err(_) => v.clone(),
                }
            }
        },
        "mysql_decimal" => {
            let scale = arg_i64(arg(1)).unwrap_or(0).max(0) as usize;
            match arg(0) {
                Value::Null => Value::Null,
                Value::Blob(_) => arg(0).clone(),
                v => match arg_f64(v) {
                    Some(x) => {
                        let factor = 10f64.powi(scale as i32);
                        let rounded = (x * factor).round() / factor;
                        text(format!("{rounded:.scale$}"))
                    }
                    None => v.clone(),
                },
            }
        }
        _ => return Err(LimboError::ParseError(format!("no such function: {name}"))),
    })
}

fn to_radix(mut n: u64, radix: u32) -> String {
    if n == 0 {
        return "0".to_string();
    }
    let mut digits = Vec::new();
    while n > 0 {
        let d = (n % radix as u64) as u32;
        digits.push(
            std::char::from_digit(d, radix)
                .unwrap()
                .to_ascii_uppercase(),
        );
        n /= radix as u64;
    }
    digits.iter().rev().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn datetime_rounding() {
        let dt = parse_datetime("2026-10-02 16:02:43.985").unwrap();
        assert_eq!(format_datetime(dt, 0), "2026-10-02 16:02:44");
        assert_eq!(format_datetime(dt, 3), "2026-10-02 16:02:43.985");
        let iso = parse_datetime("2026-10-01T20:30:00.000Z").unwrap();
        assert_eq!(format_datetime(iso, 3), "2026-10-01 20:30:00");
    }

    #[test]
    fn conv_hex() {
        assert_eq!(to_radix(0x6abfd5a1, 10), "1790956961");
    }
}
