//! MySQL sessions over in-memory databases.
//!
//! An [`Engine`] owns a set of named in-memory databases. A database is
//! created the first time a session selects it; when a template database is
//! configured, a new database starts as a copy of the template, which gives
//! every client that picks a unique database name an isolated, pre-seeded
//! schema.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};
use turso_core::{Connection, Database, LimboError, Value};

use crate::catalog::{Catalog, MyType};
use crate::dialect::open_memory_database;
use crate::rewrite::{rewrite, Kind, Rewritten};

/// How long a writer waits for another transaction before failing with a
/// lock wait timeout.
const LOCK_WAIT_TIMEOUT: Duration = Duration::from_secs(20);

/// Serializes the writers of one database.
///
/// The engine admits one write transaction at a time and reports a second
/// one as busy, which a caller can only retry by polling. Queueing writers
/// here instead hands the database over the moment the previous writer
/// finishes.
#[derive(Default)]
struct WriteGate {
    held: Mutex<bool>,
    released: Condvar,
}

impl WriteGate {
    fn acquire(&self) -> Result<()> {
        let mut held = self.held.lock();
        let deadline = Instant::now() + LOCK_WAIT_TIMEOUT;
        while *held {
            if self.released.wait_until(&mut held, deadline).timed_out() && *held {
                return Err(MyError::new(
                    1205,
                    "HY000",
                    "Lock wait timeout exceeded; try restarting transaction",
                ));
            }
        }
        *held = true;
        Ok(())
    }

    fn release(&self) {
        *self.held.lock() = false;
        self.released.notify_one();
    }
}

#[derive(Debug, Clone)]
pub struct MyError {
    pub code: u16,
    pub sqlstate: &'static str,
    pub message: String,
}

impl MyError {
    fn new(code: u16, sqlstate: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            sqlstate,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for MyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "ERROR {} ({}): {}",
            self.code, self.sqlstate, self.message
        )
    }
}

impl std::error::Error for MyError {}

pub type Result<T> = std::result::Result<T, MyError>;

fn engine_error(e: LimboError) -> MyError {
    let message = e.to_string();
    let lower = message.to_ascii_lowercase();
    match e {
        LimboError::Busy | LimboError::BusySnapshot | LimboError::TableLocked => MyError::new(
            1205,
            "HY000",
            "Lock wait timeout exceeded; try restarting transaction",
        ),
        LimboError::Constraint(_) if lower.contains("unique") || lower.contains("primary key") => {
            MyError::new(1062, "23000", format!("Duplicate entry for key: {message}"))
        }
        LimboError::Constraint(_) if lower.contains("not null") => {
            MyError::new(1048, "23000", format!("Column cannot be null: {message}"))
        }
        _ if lower.contains("no such table") => MyError::new(1146, "42S02", message),
        _ if lower.contains("no such column") => MyError::new(1054, "42S22", message),
        LimboError::ParseError(_) => MyError::new(1064, "42000", message),
        _ => MyError::new(1105, "HY000", message),
    }
}

/// One named in-memory database.
pub struct Db {
    pub name: String,
    database: Arc<Database>,
    pub catalog: Catalog,
    /// Native statements that built this database, replayed to clone it.
    journal: Mutex<Vec<String>>,
    write_gate: WriteGate,
}

pub struct Engine {
    dbs: Mutex<HashMap<String, Arc<Db>>>,
    template: Option<String>,
}

impl Engine {
    pub fn new(template: Option<String>) -> Arc<Self> {
        Arc::new(Self {
            dbs: Mutex::new(HashMap::new()),
            template,
        })
    }

    /// Forget a database. Its memory is freed once the last session using it
    /// is gone; a later use of the same name starts from scratch.
    pub fn drop_database(&self, name: &str) -> bool {
        self.dbs.lock().remove(name).is_some()
    }

    fn exists(&self, name: &str) -> bool {
        self.dbs.lock().contains_key(name)
    }

    /// Whether `db` is still the live database under its name.
    fn is_current(&self, db: &Arc<Db>) -> bool {
        self.dbs
            .lock()
            .get(&db.name)
            .is_some_and(|live| Arc::ptr_eq(live, db))
    }

    fn database(&self, name: &str) -> Result<Arc<Db>> {
        let mut dbs = self.dbs.lock();
        if let Some(db) = dbs.get(name) {
            return Ok(db.clone());
        }
        let database = open_memory_database(name).map_err(engine_error)?;
        let db = Arc::new(Db {
            name: name.to_string(),
            database,
            catalog: Catalog::new(),
            journal: Mutex::new(Vec::new()),
            write_gate: WriteGate::default(),
        });
        let template = self
            .template
            .as_deref()
            .filter(|t| *t != name)
            .and_then(|t| dbs.get(t).cloned());
        if let Some(template) = template {
            let conn = db.database.connect().map_err(engine_error)?;
            let statements = template.journal.lock().clone();
            conn.execute("BEGIN").map_err(engine_error)?;
            for sql in &statements {
                conn.execute(sql).map_err(engine_error)?;
            }
            conn.execute("COMMIT").map_err(engine_error)?;
            for table in template.catalog.tables() {
                db.catalog.register((*table).clone());
            }
            db.journal.lock().clone_from(&statements);
        }
        dbs.insert(name.to_string(), db.clone());
        Ok(db)
    }
}

#[derive(Debug, Clone)]
pub struct ColumnMeta {
    pub name: String,
    pub table: String,
    /// MySQL type when the column traces back to a table column; otherwise
    /// the type is taken from the values.
    pub ty: Option<MyType>,
}

#[derive(Debug)]
pub struct ResultSet {
    pub columns: Vec<ColumnMeta>,
    pub rows: Vec<Vec<Value>>,
}

#[derive(Debug)]
pub enum Outcome {
    Rows(ResultSet),
    Ok {
        affected_rows: u64,
        last_insert_id: u64,
        /// The OK packet's human-readable info, e.g. UPDATE's
        /// `Rows matched: 1  Changed: 1  Warnings: 0`.
        info: String,
    },
}

impl Outcome {
    fn ok() -> Self {
        Outcome::Ok {
            affected_rows: 0,
            last_insert_id: 0,
            info: String::new(),
        }
    }
}

pub struct Session {
    engine: Arc<Engine>,
    db: Option<Arc<Db>>,
    conn: Option<Arc<Connection>>,
    in_transaction: bool,
    /// Write statements of the open transaction, journaled on commit.
    pending_journal: Vec<String>,
}

impl Session {
    pub fn new(engine: Arc<Engine>) -> Self {
        Self {
            engine,
            db: None,
            conn: None,
            in_transaction: false,
            pending_journal: Vec::new(),
        }
    }

    pub fn in_transaction(&self) -> bool {
        self.in_transaction
    }

    pub fn use_database(&mut self, name: &str) -> Result<()> {
        if self
            .db
            .as_ref()
            .is_some_and(|db| db.name == name && self.engine.is_current(db))
        {
            return Ok(());
        }
        self.rollback_open_transaction();
        let db = self.engine.database(name)?;
        let conn = db.database.connect().map_err(engine_error)?;
        // Writers queue on the database's gate; this only covers engine-internal
        // contention.
        conn.set_busy_timeout(Duration::from_secs(1));
        self.db = Some(db);
        self.conn = Some(conn);
        Ok(())
    }

    fn rollback_open_transaction(&mut self) {
        if self.in_transaction {
            if let Some(conn) = &self.conn {
                let _ = conn.execute("ROLLBACK");
            }
            self.in_transaction = false;
            self.pending_journal.clear();
            if let Some(db) = &self.db {
                db.write_gate.release();
            }
        }
    }

    /// Reset session state, as `COM_RESET_CONNECTION` does.
    pub fn reset(&mut self) {
        self.rollback_open_transaction();
    }

    /// Execute MySQL text. With several statements, the last outcome wins.
    pub fn execute(&mut self, sql: &str) -> Result<Outcome> {
        let Some(db) = self.db.clone() else {
            // Statements that need no database (`SET`, `SELECT 1` on a bare
            // connection) run against a scratch one.
            self.use_database("mysql")?;
            return self.execute(sql);
        };
        let statements =
            rewrite(sql, &db.catalog).map_err(|e| MyError::new(1064, "42000", e.to_string()))?;
        let mut outcome = Outcome::ok();
        for stmt in statements {
            // An earlier statement may have switched or dropped the database.
            let db = match self.db.clone() {
                Some(db) => db,
                None => {
                    self.use_database("mysql")?;
                    self.db.clone().expect("database selected")
                }
            };
            outcome = self.execute_one(&db, stmt)?;
        }
        Ok(outcome)
    }

    fn conn(&self) -> &Arc<Connection> {
        self.conn.as_ref().expect("database selected")
    }

    fn run(&self, sql: &str) -> Result<u64> {
        let mut stmt = self.conn().prepare(sql).map_err(engine_error)?;
        stmt.run_ignore_rows().map_err(engine_error)?;
        Ok(stmt.n_change().max(0) as u64)
    }

    fn count(&self, sql: &str) -> Result<u64> {
        let rs = self.query(sql, &[])?;
        Ok(rs
            .rows
            .first()
            .and_then(|r| r.first())
            .and_then(|v| v.as_int())
            .unwrap_or(0)
            .max(0) as u64)
    }

    fn journal(&mut self, db: &Db, statements: &[String]) {
        if self.in_transaction {
            self.pending_journal.extend_from_slice(statements);
        } else {
            db.journal.lock().extend_from_slice(statements);
        }
    }

    fn execute_one(&mut self, db: &Arc<Db>, stmt: Rewritten) -> Result<Outcome> {
        match stmt.kind {
            Kind::Noop => Ok(Outcome::ok()),
            Kind::Use(name) => {
                self.use_database(&name)?;
                Ok(Outcome::ok())
            }
            Kind::CreateDatabase {
                name,
                if_not_exists,
            } => {
                if self.engine.exists(&name) {
                    if !if_not_exists {
                        return Err(MyError::new(
                            1007,
                            "HY000",
                            format!("Can't create database '{name}'; database exists"),
                        ));
                    }
                } else {
                    self.engine.database(&name)?;
                }
                Ok(Outcome::ok())
            }
            Kind::DropDatabase { name, if_exists } => {
                if !self.engine.drop_database(&name) && !if_exists {
                    return Err(MyError::new(
                        1008,
                        "HY000",
                        format!("Can't drop database '{name}'; database doesn't exist"),
                    ));
                }
                if self.db.as_ref().is_some_and(|db| db.name == name) {
                    // MySQL leaves the session with no database selected.
                    self.rollback_open_transaction();
                    self.db = None;
                    self.conn = None;
                }
                Ok(Outcome::ok())
            }
            Kind::Begin => {
                if self.in_transaction {
                    // START TRANSACTION implicitly commits an open one.
                    self.commit(db)?;
                }
                // Writers are serialized: taking the write lock up front
                // means a transaction never fails halfway on a lock upgrade.
                db.write_gate.acquire()?;
                if let Err(e) = self.run("BEGIN IMMEDIATE") {
                    db.write_gate.release();
                    return Err(e);
                }
                self.in_transaction = true;
                Ok(Outcome::ok())
            }
            Kind::Commit => {
                if self.in_transaction {
                    self.commit(db)?;
                }
                Ok(Outcome::ok())
            }
            Kind::Rollback => {
                if self.in_transaction {
                    self.in_transaction = false;
                    self.pending_journal.clear();
                    let result = self.run("ROLLBACK");
                    db.write_gate.release();
                    result?;
                }
                Ok(Outcome::ok())
            }
            Kind::Query => {
                let sql = stmt.sql.last().expect("query has a statement");
                self.query(sql, &stmt.result_types).map(Outcome::Rows)
            }
            Kind::Dml => {
                let (affected_rows, last_insert_id, changed) = self.write(db, |session| {
                    let changed = match &stmt.changed_sql {
                        Some(sql) => Some(session.count(sql)?),
                        None => None,
                    };
                    let mut affected_rows = 0;
                    for sql in &stmt.sql {
                        affected_rows = session.run(sql)?;
                    }
                    let mut last_insert_id = if stmt.auto_increment && affected_rows > 0 {
                        session.conn().last_insert_rowid().max(0) as u64
                    } else {
                        0
                    };
                    if stmt.first_insert_id && affected_rows > 0 {
                        last_insert_id = last_insert_id.saturating_sub(affected_rows - 1);
                    }
                    Ok((affected_rows, last_insert_id, changed))
                })?;
                self.journal(db, &stmt.sql);
                Ok(Outcome::Ok {
                    affected_rows,
                    last_insert_id,
                    info: changed
                        .map(|changed| {
                            format!("Rows matched: {affected_rows}  Changed: {changed}  Warnings: 0")
                        })
                        .unwrap_or_default(),
                })
            }
            Kind::Ddl => {
                self.write(db, |session| {
                    for sql in &stmt.sql {
                        session.run(sql)?;
                    }
                    Ok(())
                })?;
                for name in &stmt.dropped_tables {
                    db.catalog.remove(name);
                }
                if let Some(table) = stmt.table {
                    // A plain CREATE TABLE that succeeded made a new table; with
                    // IF NOT EXISTS an existing definition stays.
                    if stmt.sql[0].starts_with("CREATE TABLE IF NOT EXISTS") {
                        db.catalog.register(table);
                    } else {
                        db.catalog.replace(table);
                    }
                }
                self.journal(db, &stmt.sql);
                Ok(Outcome::ok())
            }
        }
    }

    fn commit(&mut self, db: &Db) -> Result<()> {
        self.in_transaction = false;
        let pending = std::mem::take(&mut self.pending_journal);
        let result = self.run("COMMIT");
        db.write_gate.release();
        result?;
        db.journal.lock().extend(pending);
        Ok(())
    }

    /// Run a write. Inside a transaction the gate is already held; an
    /// autocommit write takes it for the statement.
    fn write<T>(&self, db: &Db, f: impl FnOnce(&Self) -> Result<T>) -> Result<T> {
        if self.in_transaction {
            return f(self);
        }
        db.write_gate.acquire()?;
        let result = f(self);
        db.write_gate.release();
        result
    }

    fn query(&self, sql: &str, inferred: &[Option<MyType>]) -> Result<ResultSet> {
        let mut stmt = self.conn().prepare(sql).map_err(engine_error)?;
        let columns = (0..stmt.num_columns())
            .map(|i| ColumnMeta {
                name: stmt.get_column_name(i).to_string(),
                table: stmt
                    .get_column_table_name(i)
                    .map(|t| t.to_string())
                    .unwrap_or_default(),
                // The statement-level type knows MySQL's expression typing;
                // the declared type covers what inference could not resolve.
                ty: inferred.get(i).copied().flatten().or_else(|| {
                    stmt.get_column_decltype(i)
                        .and_then(|decl| MyType::from_decl(&decl))
                }),
            })
            .collect();
        let rows = stmt.run_collect_rows().map_err(engine_error)?;
        Ok(ResultSet { columns, rows })
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.rollback_open_transaction();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(outcome: Outcome) -> Vec<Vec<String>> {
        match outcome {
            Outcome::Rows(rs) => rs
                .rows
                .iter()
                .map(|r| r.iter().map(|v| v.to_string()).collect())
                .collect(),
            other => panic!("expected rows, got {other:?}"),
        }
    }

    fn session(engine: &Arc<Engine>, db: &str) -> Session {
        let mut s = Session::new(engine.clone());
        s.use_database(db).unwrap();
        s
    }

    fn ok(outcome: Outcome) -> (u64, u64, String) {
        match outcome {
            Outcome::Ok {
                affected_rows,
                last_insert_id,
                info,
            } => (affected_rows, last_insert_id, info),
            other => panic!("expected ok, got {other:?}"),
        }
    }

    #[test]
    fn create_and_drop_database() {
        let engine = Engine::new(None);
        let mut s = session(&engine, "drizzle");
        s.execute("create table t (id int primary key)").unwrap();
        s.execute("drop database if exists drizzle; create database drizzle; use drizzle;")
            .unwrap();
        // The old database is gone: the table does not exist in the new one.
        assert!(s.execute("select * from t").is_err());
        assert_eq!(s.execute("create database drizzle").unwrap_err().code, 1007);
        assert_eq!(s.execute("drop database nope").unwrap_err().code, 1008);
        s.execute("create schema if not exists drizzle").unwrap();
        s.execute("create schema other").unwrap();
        s.execute("drop schema if exists other").unwrap();
    }

    #[test]
    fn serial_multi_drop_cte_delete_and_foreign_keys() {
        let engine = Engine::new(None);
        let mut s = session(&engine, "t");
        s.execute("create table a (id serial primary key, name text not null)")
            .unwrap();
        s.execute("create table b (id serial primary key, a_id bigint unsigned, y year)")
            .unwrap();
        s.execute("alter table `b` add constraint `fk` foreign key (`a_id`) references `a`(`id`) on delete cascade")
            .unwrap();
        assert_eq!(ok(s.execute("insert into a (name) values ('x')").unwrap()).1, 1);
        // `WITH ... DELETE` affects rows; it is not a result set.
        let (affected, _, _) = ok(s
            .execute("with big as (select max(id) as m from a) delete from a where id = (select m from big)")
            .unwrap());
        assert_eq!(affected, 1);
        s.execute("drop table if exists a, b, missing").unwrap();
        assert!(s.execute("select * from a").is_err());
        assert!(s.execute("select * from b").is_err());
    }

    #[test]
    fn insert_id_is_first_generated_id() {
        let engine = Engine::new(None);
        let mut s = session(&engine, "t");
        s.execute("create table u (id serial primary key, name text not null)")
            .unwrap();
        assert_eq!(
            ok(s.execute("insert into u (name) values ('a'), ('b'), ('c')").unwrap()),
            (3, 1, String::new())
        );
        assert_eq!(
            ok(s.execute("insert into u (id, name) values (default, 'd'), (null, 'e')").unwrap()).1,
            4
        );
        // Explicit ids: MySQL reports the last one.
        assert_eq!(
            ok(s.execute("insert into u (id, name) values (10, 'f'), (11, 'g')").unwrap()).1,
            11
        );
    }

    #[test]
    fn update_reports_matched_and_changed_rows() {
        let engine = Engine::new(None);
        let mut s = session(&engine, "t");
        s.execute("create table u (id serial primary key, name varchar(20) not null)")
            .unwrap();
        s.execute("insert into u (name) values ('John'), ('John'), ('Ann')")
            .unwrap();
        let info = |matched: u64, changed: u64| {
            format!("Rows matched: {matched}  Changed: {changed}  Warnings: 0")
        };
        assert_eq!(
            ok(s.execute("update u set name = 'Jane' where id = 1").unwrap()),
            (1, 0, info(1, 1))
        );
        // Same value: matched, not changed.
        assert_eq!(
            ok(s.execute("update u set name = 'Jane' where id = 1").unwrap()).2,
            info(1, 0)
        );
        // Only the case differs: the collation calls them equal, MySQL still
        // counts the row as changed.
        assert_eq!(
            ok(s.execute("update u set name = 'JANE' where name = 'jane'").unwrap()).2,
            info(1, 1)
        );
        assert_eq!(
            ok(s.execute("update u set name = 'Ann'").unwrap()).2,
            info(3, 2)
        );
    }

    #[test]
    fn update_and_delete_with_order_by_and_limit() {
        let engine = Engine::new(None);
        let mut s = session(&engine, "t");
        s.execute("create table u (id serial primary key, name text not null, v boolean not null default false)")
            .unwrap();
        s.execute("insert into u (name) values ('c'), ('a'), ('b'), ('d')")
            .unwrap();
        let (affected, _, _) = ok(s
            .execute("update `u` set `v` = true where `u`.`v` = false order by `u`.`name` asc limit 2")
            .unwrap());
        assert_eq!(affected, 2);
        assert_eq!(
            rows(s.execute("select name from u where v = true order by name").unwrap()),
            vec![vec!["a"], vec!["b"]]
        );
        s.execute("update u set name = 'order by x' limit 1").unwrap();
        assert_eq!(
            rows(s.execute("select name from u where id = 1").unwrap()),
            vec![vec!["order by x"]]
        );
        let (affected, _, _) = ok(s
            .execute("delete from u where v = true order by name desc limit 1")
            .unwrap());
        assert_eq!(affected, 1);
        assert_eq!(
            rows(s.execute("select name from u where v = true").unwrap()),
            vec![vec!["a"]]
        );
        s.execute("delete from u limit 10").unwrap();
        assert_eq!(rows(s.execute("select count(*) from u").unwrap()), vec![vec!["0"]]);
    }

    #[test]
    fn time_and_year_values() {
        let engine = Engine::new(None);
        let mut s = session(&engine, "t");
        s.execute("create table d (t1 time(1), t0 time, y year)").unwrap();
        s.execute(
            "insert into d values ('12:12:12', '12:12:12.6', 22), ('-838:59:59', 121212, '69'),              ('23:59:59.96', '01:02:03', 1999), (null, null, 70)",
        )
        .unwrap();
        assert_eq!(
            rows(s.execute("select * from d").unwrap()),
            vec![
                vec!["12:12:12.0", "12:12:13", "2022"],
                vec!["-838:59:59.0", "12:12:12", "2069"],
                vec!["24:00:00.0", "01:02:03", "1999"],
                vec!["", "", "1970"],
            ]
        );
        s.execute("update d set t1 = '1:2:3' where y = 2022").unwrap();
        assert_eq!(
            rows(s.execute("select t1 from d where y = 2022").unwrap()),
            vec![vec!["01:02:03.0"]]
        );
    }

    #[test]
    fn recreated_table_uses_its_new_types() {
        let engine = Engine::new(None);
        let mut s = session(&engine, "t");
        s.execute("create table d (v date)").unwrap();
        s.execute("drop table d").unwrap();
        s.execute("create table d (v text)").unwrap();
        s.execute("insert into d values ('2022-11-11 10:00:00')").unwrap();
        assert_eq!(
            rows(s.execute("select v from d").unwrap()),
            vec![vec!["2022-11-11 10:00:00"]]
        );
    }

    #[test]
    fn upsert_default_and_datetime() {
        let engine = Engine::new(None);
        let mut s = session(&engine, "t");
        s.execute(
            "CREATE TABLE `seq` (`userId` binary(12) NOT NULL, `prefix` varchar(15) NOT NULL DEFAULT 'default', \
             `value` int unsigned NOT NULL DEFAULT 0, `at` datetime, PRIMARY KEY (`userId`, `prefix`))",
        )
        .unwrap();
        for _ in 0..3 {
            s.execute(
                "insert into `seq` (`userId`, `prefix`, `value`, `at`) values (X'0102', default, 1, '2026-10-02 16:02:43.985') \
                 on duplicate key update `value` = `seq`.`value` + 1",
            )
            .unwrap();
        }
        let got = rows(
            s.execute("select `prefix`, `value`, `at` from `seq`")
                .unwrap(),
        );
        assert_eq!(got, vec![vec!["default", "3", "2026-10-02 16:02:44"]]);
        let got = rows(
            s.execute("select count(*) from seq where at > '2026-10-02T16:02:43.000Z'")
                .unwrap(),
        );
        assert_eq!(got, vec![vec!["1"]]);
    }

    #[test]
    fn auto_increment_and_template_clone() {
        let engine = Engine::new(Some("base".to_string()));
        let mut base = session(&engine, "base");
        base.execute("CREATE TABLE `bank` (`id` int AUTO_INCREMENT, `label` varchar(100) NOT NULL, PRIMARY KEY (`id`), UNIQUE `u` (`label`))")
            .unwrap();
        let out = base
            .execute("insert into `bank` (`id`, `label`) values (default, 'a')")
            .unwrap();
        assert!(matches!(
            out,
            Outcome::Ok {
                affected_rows: 1,
                last_insert_id: 1,
                ..
            }
        ));

        let mut a = session(&engine, "file_a");
        let mut b = session(&engine, "file_b");
        a.execute("insert into bank (label) values ('only-a')")
            .unwrap();
        assert_eq!(
            rows(a.execute("select count(*) from bank").unwrap()),
            vec![vec!["2"]]
        );
        assert_eq!(
            rows(b.execute("select count(*) from bank").unwrap()),
            vec![vec!["1"]]
        );

        let dup = b
            .execute("insert into bank (label) values ('a')")
            .unwrap_err();
        assert_eq!(dup.code, 1062);
        b.execute("insert ignore into bank (label) values ('a')")
            .unwrap();
    }

    #[test]
    fn text_collation_is_unicode_and_case_insensitive() {
        let engine = Engine::new(None);
        let mut s = session(&engine, "t");
        s.execute("CREATE TABLE p (id int NOT NULL, name varchar(255) NOT NULL, PRIMARY KEY (id), UNIQUE `u` (`name`))")
            .unwrap();
        for (i, name) in ["محمد", "پریچهر", "آیدین", "علی", "سارا", "Zed", "apple"]
            .iter()
            .enumerate()
        {
            s.execute(&format!("insert into p values ({i}, '{name}')"))
                .unwrap();
        }
        let got = rows(s.execute("select name from p order by name").unwrap());
        let got: Vec<&str> = got.iter().map(|r| r[0].as_str()).collect();
        assert_eq!(
            got,
            ["apple", "Zed", "آیدین", "پریچهر", "سارا", "علی", "محمد"]
        );
        assert_eq!(
            rows(
                s.execute("select count(*) from p where name = 'APPLE'")
                    .unwrap()
            ),
            vec![vec!["1"]]
        );
        assert_eq!(
            s.execute("insert into p values (99, 'ZED')")
                .unwrap_err()
                .code,
            1062
        );
    }

    #[test]
    fn concurrent_writers_queue() {
        let engine = Engine::new(None);
        session(&engine, "t")
            .execute("CREATE TABLE c (id int NOT NULL, n int NOT NULL, PRIMARY KEY (id))")
            .unwrap();
        session(&engine, "t")
            .execute("insert into c values (1, 0)")
            .unwrap();
        let started = Instant::now();
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let engine = engine.clone();
                std::thread::spawn(move || {
                    let mut s = session(&engine, "t");
                    for i in 0..200 {
                        if i % 2 == 0 {
                            s.execute("begin").unwrap();
                            s.execute("update c set n = n + 1 where id = 1").unwrap();
                            s.execute("commit").unwrap();
                        } else {
                            s.execute("update c set n = n + 1 where id = 1").unwrap();
                        }
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        let mut s = session(&engine, "t");
        assert_eq!(
            rows(s.execute("select n from c").unwrap()),
            vec![vec!["1600"]]
        );
        eprintln!("1600 contended writes in {:?}", started.elapsed());
    }

    #[test]
    fn transactions_and_division() {
        let engine = Engine::new(None);
        let mut s = session(&engine, "t");
        s.execute("CREATE TABLE t (id int NOT NULL, n int NOT NULL, PRIMARY KEY (id))")
            .unwrap();
        s.execute("begin").unwrap();
        s.execute("insert into t values (1, 7)").unwrap();
        s.execute("rollback").unwrap();
        assert_eq!(
            rows(s.execute("select count(*) from t").unwrap()),
            vec![vec!["0"]]
        );
        s.execute("begin").unwrap();
        s.execute("insert into t values (1, 7)").unwrap();
        s.execute("commit").unwrap();
        assert_eq!(
            rows(s.execute("select n / 2 from t").unwrap()),
            vec![vec!["3.5"]]
        );
    }
}
