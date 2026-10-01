//! Access control: roles and row-level security policies.
//!
//! The catalog is stored in the `__turso_internal_access_control` table, one row per
//! object, each holding the SQL statement that recreates it:
//!
//! | type           | name                  | tbl_name | sql                                       |
//! |----------------|-----------------------|----------|-------------------------------------------|
//! | `role`         | role name             | `''`     | `CREATE ROLE alice`                       |
//! | `policy`       | policy name           | table    | `CREATE POLICY p ON t ...`                |
//! | `row_security` | `enable` or `force`   | table    | `ALTER TABLE t ENABLE ROW LEVEL SECURITY` |
//!
//! At schema load every stored statement is applied to an empty
//! [AccessControlCatalog]. DDL statements update the table and then apply the same
//! statement to the in-memory catalog, so both paths share [AccessControlCatalog::apply].
//!
//! Row-level security is enforced by the planner: the `USING` expressions of
//! the policies that apply to the current role are added to the WHERE clause
//! of every SELECT, UPDATE and DELETE that reads the table, and the
//! `WITH CHECK` expressions are evaluated on every row written by INSERT and
//! UPDATE (see `translate/access_control.rs`). Connections without a role set with
//! `SET ROLE` act as the superuser and bypass row-level security.

use std::collections::{HashMap, HashSet};

use turso_parser::{
    ast::{self, AlterTableBody, PolicyCommand, RowSecurityChange, Stmt},
    parser::Parser,
};

use crate::{util::normalize_ident, LimboError, Result};

pub const ACCESS_CONTROL_TABLE_NAME: &str = "__turso_internal_access_control";

pub const ACCESS_CONTROL_TABLE_SQL: &str =
    "CREATE TABLE __turso_internal_access_control(type TEXT, name TEXT, tbl_name TEXT, sql TEXT)";

pub(crate) const LOAD_ACCESS_CONTROL_SQL: &str = "SELECT sql FROM __turso_internal_access_control";

#[derive(Debug, Clone, Default)]
pub struct AccessControlCatalog {
    roles: HashSet<String>,
    tables: HashMap<String, TableRowSecurity>,
}

#[derive(Debug, Clone, Default)]
pub struct TableRowSecurity {
    /// `ALTER TABLE ... ENABLE ROW LEVEL SECURITY`
    pub enabled: bool,
    /// `ALTER TABLE ... FORCE ROW LEVEL SECURITY`
    pub forced: bool,
    pub policies: Vec<Policy>,
}

#[derive(Debug, Clone)]
pub struct Policy {
    pub name: String,
    pub restrictive: bool,
    pub command: PolicyCommand,
    /// Roles the policy applies to; empty means every role (`PUBLIC`).
    pub roles: Vec<String>,
    pub using_expr: Option<ast::Expr>,
    pub check_expr: Option<ast::Expr>,
}

impl AccessControlCatalog {
    pub fn load(statements: &[String]) -> Result<Self> {
        let mut catalog = Self::default();
        for sql in statements {
            catalog.apply_sql(sql)?;
        }
        Ok(catalog)
    }

    pub fn apply_sql(&mut self, sql: &str) -> Result<()> {
        let mut parser = Parser::new(sql.as_bytes());
        match parser.next_cmd()? {
            Some(ast::Cmd::Stmt(stmt)) => self.apply(&stmt),
            _ => Err(LimboError::ParseError(format!(
                "invalid access control catalog sql: {sql}"
            ))),
        }
    }

    pub fn apply(&mut self, stmt: &Stmt) -> Result<()> {
        match stmt {
            Stmt::CreateRole { role_name } => {
                self.roles.insert(normalize_ident(role_name.as_str()));
            }
            Stmt::DropRole { role_name, .. } => {
                self.roles.remove(&normalize_ident(role_name.as_str()));
            }
            Stmt::CreatePolicy(policy) => {
                let table = normalize_ident(policy.tbl_name.name.as_str());
                self.tables.entry(table).or_default().policies.push(Policy {
                    name: normalize_ident(policy.policy_name.as_str()),
                    restrictive: policy.restrictive,
                    command: policy.command,
                    roles: policy
                        .roles
                        .iter()
                        .map(|role| normalize_ident(role.as_str()))
                        .collect(),
                    using_expr: policy.using_expr.as_deref().cloned(),
                    check_expr: policy.check_expr.as_deref().cloned(),
                });
            }
            Stmt::DropPolicy {
                policy_name,
                tbl_name,
                ..
            } => {
                let table = normalize_ident(tbl_name.name.as_str());
                let name = normalize_ident(policy_name.as_str());
                if let Some(state) = self.tables.get_mut(&table) {
                    state.policies.retain(|policy| policy.name != name);
                }
            }
            Stmt::AlterTable(alter) => {
                let AlterTableBody::RowSecurity(change) = alter.body else {
                    return Err(LimboError::InternalError(format!(
                        "not a row-level security change: {stmt}"
                    )));
                };
                let table = normalize_ident(alter.name.name.as_str());
                let state = self.tables.entry(table).or_default();
                match change {
                    RowSecurityChange::Enable => state.enabled = true,
                    RowSecurityChange::Disable => state.enabled = false,
                    RowSecurityChange::Force => state.forced = true,
                    RowSecurityChange::NoForce => state.forced = false,
                }
            }
            Stmt::DropTable { tbl_name, .. } => {
                self.tables.remove(&normalize_ident(tbl_name.name.as_str()));
            }
            _ => {
                return Err(LimboError::InternalError(format!(
                    "not an access control statement: {stmt}"
                )))
            }
        }
        Ok(())
    }

    pub fn has_role(&self, role: &str) -> bool {
        self.roles.contains(&normalize_ident(role))
    }

    pub fn table(&self, table: &str) -> Option<&TableRowSecurity> {
        self.tables.get(&normalize_ident(table))
    }

    /// Row-level security state of `table` when it applies to `role`.
    /// `None` for the superuser (no role set), which bypasses row-level
    /// security, and for tables that do not have it enabled.
    pub fn row_security_for(&self, table: &str, role: Option<&str>) -> Option<&TableRowSecurity> {
        role?;
        self.table(table).filter(|state| state.enabled)
    }

    pub fn policies_using_role(&self, role: &str) -> impl Iterator<Item = (&str, &Policy)> {
        let role = normalize_ident(role);
        self.tables.iter().flat_map(move |(table, state)| {
            let role = role.clone();
            state
                .policies
                .iter()
                .filter(move |policy| policy.roles.contains(&role))
                .map(move |policy| (table.as_str(), policy))
        })
    }
}

impl TableRowSecurity {
    pub fn has_policy(&self, name: &str) -> bool {
        let name = normalize_ident(name);
        self.policies.iter().any(|policy| policy.name == name)
    }

    pub fn is_empty(&self) -> bool {
        !self.enabled && !self.forced && self.policies.is_empty()
    }

    /// Predicate deciding which existing rows `role` may see for `command`:
    /// the `USING` expressions of the permissive policies combined with OR,
    /// and of the restrictive policies combined with AND. Without any
    /// permissive policy no rows are visible.
    pub fn using_predicate(&self, role: &str, command: PolicyCommand) -> ast::Expr {
        self.combine(role, command, |policy| policy.using_expr.as_ref())
    }

    /// Predicate every new row written by `role` with `command` must satisfy.
    /// Policies without `WITH CHECK` use their `USING` expression instead.
    pub fn check_predicate(&self, role: &str, command: PolicyCommand) -> ast::Expr {
        self.combine(role, command, |policy| {
            policy.check_expr.as_ref().or(policy.using_expr.as_ref())
        })
    }

    fn combine<'a>(
        &'a self,
        role: &str,
        command: PolicyCommand,
        expr_of: impl Fn(&'a Policy) -> Option<&'a ast::Expr>,
    ) -> ast::Expr {
        let applicable = self
            .policies
            .iter()
            .filter(|policy| policy.applies_to(role, command));
        let mut permissive = None;
        let mut restrictive = Vec::new();
        for policy in applicable {
            let expr = expr_of(policy).cloned().unwrap_or_else(true_expr);
            if policy.restrictive {
                restrictive.push(expr);
            } else {
                permissive = Some(match permissive {
                    None => expr,
                    Some(previous) => binary(previous, ast::Operator::Or, expr),
                });
            }
        }
        restrictive
            .into_iter()
            .fold(permissive.unwrap_or_else(false_expr), |acc, expr| {
                binary(acc, ast::Operator::And, expr)
            })
    }
}

impl Policy {
    pub fn applies_to(&self, role: &str, command: PolicyCommand) -> bool {
        (self.command == command || self.command == PolicyCommand::All)
            && (self.roles.is_empty() || self.roles.contains(&normalize_ident(role)))
    }
}

fn binary(lhs: ast::Expr, op: ast::Operator, rhs: ast::Expr) -> ast::Expr {
    ast::Expr::Binary(
        Box::new(ast::Expr::Parenthesized(vec![Box::new(lhs)])),
        op,
        Box::new(ast::Expr::Parenthesized(vec![Box::new(rhs)])),
    )
}

fn true_expr() -> ast::Expr {
    ast::Expr::Literal(ast::Literal::Numeric("1".to_string()))
}

fn false_expr() -> ast::Expr {
    ast::Expr::Literal(ast::Literal::Numeric("0".to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog(statements: &[&str]) -> AccessControlCatalog {
        let mut catalog = AccessControlCatalog::default();
        for sql in statements {
            catalog.apply_sql(sql).unwrap();
        }
        catalog
    }

    #[test]
    fn superuser_bypasses_row_security() {
        let catalog = catalog(&["ALTER TABLE t ENABLE ROW LEVEL SECURITY"]);
        assert!(catalog.row_security_for("t", None).is_none());
        assert!(catalog.row_security_for("t", Some("alice")).is_some());
    }

    #[test]
    fn disabled_table_has_no_row_security_even_with_policies() {
        let catalog = catalog(&["CREATE POLICY p ON t USING (1)"]);
        assert!(catalog.row_security_for("t", Some("alice")).is_none());
    }

    #[test]
    fn no_permissive_policy_hides_all_rows() {
        let catalog = catalog(&[
            "ALTER TABLE t ENABLE ROW LEVEL SECURITY",
            "CREATE POLICY r ON t AS RESTRICTIVE USING (a > 1)",
        ]);
        let state = catalog.row_security_for("t", Some("alice")).unwrap();
        assert_eq!(
            state
                .using_predicate("alice", PolicyCommand::Select)
                .to_string(),
            "(0) AND (a > 1)"
        );
    }

    #[test]
    fn permissive_policies_are_ored_and_restrictive_anded() {
        let catalog = catalog(&[
            "ALTER TABLE t ENABLE ROW LEVEL SECURITY",
            "CREATE POLICY p1 ON t USING (a = 1)",
            "CREATE POLICY p2 ON t FOR SELECT USING (a = 2)",
            "CREATE POLICY p3 ON t FOR DELETE USING (a = 3)",
            "CREATE POLICY r ON t AS RESTRICTIVE USING (b)",
        ]);
        let state = catalog.row_security_for("t", Some("alice")).unwrap();
        assert_eq!(
            state
                .using_predicate("alice", PolicyCommand::Select)
                .to_string(),
            "((a = 1) OR (a = 2)) AND (b)"
        );
    }

    #[test]
    fn policy_for_other_role_is_ignored() {
        let catalog = catalog(&[
            "ALTER TABLE t ENABLE ROW LEVEL SECURITY",
            "CREATE POLICY p ON t TO bob USING (1)",
        ]);
        let state = catalog.row_security_for("t", Some("alice")).unwrap();
        assert_eq!(
            state
                .using_predicate("alice", PolicyCommand::Select)
                .to_string(),
            "0"
        );
        assert_eq!(
            state
                .using_predicate("bob", PolicyCommand::Select)
                .to_string(),
            "1"
        );
    }

    #[test]
    fn check_predicate_falls_back_to_using() {
        let catalog = catalog(&[
            "ALTER TABLE t ENABLE ROW LEVEL SECURITY",
            "CREATE POLICY p ON t USING (a = 1)",
            "CREATE POLICY q ON t FOR INSERT WITH CHECK (a < 10)",
        ]);
        let state = catalog.row_security_for("t", Some("alice")).unwrap();
        assert_eq!(
            state
                .check_predicate("alice", PolicyCommand::Insert)
                .to_string(),
            "(a = 1) OR (a < 10)"
        );
    }

    #[test]
    fn drop_table_forgets_row_security() {
        let catalog = catalog(&[
            "ALTER TABLE t ENABLE ROW LEVEL SECURITY",
            "CREATE POLICY p ON t USING (1)",
            "DROP TABLE t",
        ]);
        assert!(catalog.table("t").is_none());
    }
}
