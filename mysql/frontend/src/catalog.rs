//! MySQL type catalog.
//!
//! The engine stores values with native affinities, so the MySQL column types
//! — which drive assignment coercion and wire-protocol result metadata — are
//! kept here, per database.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MyType {
    Int { bytes: u8, unsigned: bool },
    Decimal { precision: u32, scale: u32 },
    Float,
    Double,
    Char,
    Varchar,
    Text,
    Binary,
    Blob,
    Datetime { fsp: u32 },
    Timestamp { fsp: u32 },
    Date,
    Time,
    Json,
    Enum,
}

impl MyType {
    /// Declared type used in the native `CREATE TABLE`.
    ///
    /// The name is chosen so the engine derives the right storage affinity
    /// from it, and so [`MyType::from_decl`] can recover the MySQL type from a
    /// result column's declared type.
    pub fn native_decl(&self) -> String {
        match *self {
            MyType::Int { bytes, unsigned } => {
                let base = match bytes {
                    1 => "TINYINT",
                    2 => "SMALLINT",
                    3 => "MEDIUMINT",
                    8 => "BIGINT",
                    _ => "INT",
                };
                if unsigned {
                    format!("{base}_UNSIGNED")
                } else {
                    base.to_string()
                }
            }
            MyType::Decimal { precision, scale } => format!("DECIMAL_{precision}_{scale}"),
            MyType::Float => "FLOAT".to_string(),
            MyType::Double => "DOUBLE".to_string(),
            MyType::Char => "CHAR".to_string(),
            MyType::Varchar => "VARCHAR".to_string(),
            MyType::Text => "TEXT".to_string(),
            MyType::Binary => "BINARY_BLOB".to_string(),
            MyType::Blob => "BLOB".to_string(),
            MyType::Datetime { fsp } => format!("DATETIME_TEXT_{fsp}"),
            MyType::Timestamp { fsp } => format!("TIMESTAMP_TEXT_{fsp}"),
            MyType::Date => "DATE_TEXT".to_string(),
            MyType::Time => "TIME_TEXT".to_string(),
            MyType::Json => "JSON_TEXT".to_string(),
            MyType::Enum => "ENUM_TEXT".to_string(),
        }
    }

    /// Inverse of [`MyType::native_decl`].
    pub fn from_decl(decl: &str) -> Option<MyType> {
        // Parameters are part of the name (`DECIMAL_17_3`), because the
        // engine reports a declared type without its parenthesized arguments.
        let upper = decl.trim().to_ascii_uppercase();
        let (upper, unsigned) = match upper.strip_suffix("_UNSIGNED") {
            Some(b) => (b.to_string(), true),
            None => (upper, false),
        };
        let mut parts: Vec<&str> = upper.split('_').collect();
        let mut nums: Vec<u32> = Vec::new();
        while let Some(n) = parts.last().and_then(|p| p.parse().ok()) {
            nums.insert(0, n);
            parts.pop();
        }
        let base = parts.join("_");
        let base = base.as_str();
        let arg = |i: usize| nums.get(i).copied().unwrap_or(0);
        Some(match base {
            "TINYINT" => MyType::Int { bytes: 1, unsigned },
            "SMALLINT" => MyType::Int { bytes: 2, unsigned },
            "MEDIUMINT" => MyType::Int { bytes: 3, unsigned },
            "INT" | "INTEGER" => MyType::Int { bytes: 4, unsigned },
            "BIGINT" => MyType::Int { bytes: 8, unsigned },
            "DECIMAL" => MyType::Decimal {
                precision: arg(0),
                scale: arg(1),
            },
            "FLOAT" => MyType::Float,
            "DOUBLE" => MyType::Double,
            "CHAR" => MyType::Char,
            "VARCHAR" => MyType::Varchar,
            "TEXT" => MyType::Text,
            "BINARY_BLOB" => MyType::Binary,
            "BLOB" => MyType::Blob,
            "DATETIME_TEXT" => MyType::Datetime { fsp: arg(0) },
            "TIMESTAMP_TEXT" => MyType::Timestamp { fsp: arg(0) },
            "DATE_TEXT" => MyType::Date,
            "TIME_TEXT" => MyType::Time,
            "JSON_TEXT" => MyType::Json,
            "ENUM_TEXT" => MyType::Enum,
            _ => return None,
        })
    }

    pub fn is_text(&self) -> bool {
        matches!(
            self,
            MyType::Char | MyType::Varchar | MyType::Text | MyType::Enum
        )
    }
}

#[derive(Debug, Clone)]
pub struct ColumnInfo {
    pub name: String,
    pub ty: MyType,
    pub not_null: bool,
    pub auto_increment: bool,
    /// Default as native SQL text, used to expand the `DEFAULT` keyword.
    pub default_sql: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TableInfo {
    pub name: String,
    pub columns: Vec<ColumnInfo>,
    pub primary_key: Vec<String>,
    pub by_name: HashMap<String, usize>,
}

impl TableInfo {
    pub fn indexed(mut self) -> Self {
        self.by_name = self
            .columns
            .iter()
            .enumerate()
            .map(|(i, c)| (c.name.to_ascii_lowercase(), i))
            .collect();
        self
    }

    pub fn column(&self, name: &str) -> Option<&ColumnInfo> {
        self.by_name
            .get(&name.to_ascii_lowercase())
            .map(|&i| &self.columns[i])
    }
}

#[derive(Debug, Default)]
pub struct Catalog {
    tables: RwLock<HashMap<String, Arc<TableInfo>>>,
}

impl Catalog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn table(&self, name: &str) -> Option<Arc<TableInfo>> {
        self.tables.read().get(&name.to_ascii_lowercase()).cloned()
    }

    pub fn register(&self, table: TableInfo) {
        let key = table.name.to_ascii_lowercase();
        self.tables
            .write()
            .entry(key)
            .or_insert_with(|| Arc::new(table));
    }

    pub fn remove(&self, name: &str) {
        self.tables.write().remove(&name.to_ascii_lowercase());
    }

    pub fn tables(&self) -> Vec<Arc<TableInfo>> {
        self.tables.read().values().cloned().collect()
    }
}
