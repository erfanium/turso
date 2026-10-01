use std::sync::Arc;

use turso_parser::ast::{self, PolicyCommand, ResolveType, RowSecurityChange};

use crate::{
    access_control::{AccessControlCatalog, ACCESS_CONTROL_TABLE_NAME, ACCESS_CONTROL_TABLE_SQL},
    bail_parse_error,
    schema::{BTreeTable, Table},
    storage::pager::CreateBTreeFlags,
    translate::{
        emitter::{with_new_row_registers_cached, Resolver},
        expr::{
            bind_and_rewrite_expr, translate_expr_no_constant_opt, walk_expr, walk_expr_mut,
            BindingBehavior, NoConstantOptReason, WalkControl,
        },
        plan::{ColumnUsedMask, JoinedTable, Operation, TableReferences, WhereTerm},
        planner::{
            break_predicate_at_and_boundaries, collect_from_clause_table_refs,
            collect_subquery_table_refs_in_expr,
        },
        schema::{emit_schema_entry, SchemaEntryType, SQLITE_TABLEID},
    },
    util::normalize_ident,
    vdbe::{
        builder::{CursorType, ProgramBuilder},
        insn::{to_u32, CmpInsFlags, Cookie, InsertFlags, Insn, RegisterOrLiteral},
    },
    Result, MAIN_DB_ID,
};

pub fn translate_create_role(
    role_name: &ast::Name,
    resolver: &Resolver,
    program: &mut ProgramBuilder,
) -> Result<()> {
    let role = normalize_ident(role_name.as_str());
    if role == "public" {
        bail_parse_error!("role name \"public\" is reserved");
    }
    if catalog(resolver).has_role(&role) {
        bail_parse_error!("role \"{role}\" already exists");
    }
    let sql = ast::Stmt::CreateRole {
        role_name: role_name.clone(),
    }
    .to_string();
    let rows = AccessControlRows::open(resolver, program)?;
    rows.emit_insert(program, "role", &role, "", sql.clone());
    emit_catalog_update(program, resolver, sql);
    Ok(())
}

pub fn translate_drop_role(
    role_name: &ast::Name,
    if_exists: bool,
    resolver: &Resolver,
    program: &mut ProgramBuilder,
) -> Result<()> {
    let role = normalize_ident(role_name.as_str());
    let catalog = catalog(resolver);
    if !catalog.has_role(&role) {
        if if_exists {
            return Ok(());
        }
        bail_parse_error!("role \"{role}\" does not exist");
    }
    if let Some((table, policy)) = catalog.policies_using_role(&role).next() {
        bail_parse_error!(
            "role \"{role}\" cannot be dropped because policy {} on table {table} depends on it",
            policy.name
        );
    }
    let rows = AccessControlRows::open(resolver, program)?;
    rows.emit_delete(program, &[("role", 0), (&role, 1)]);
    emit_catalog_update(
        program,
        resolver,
        ast::Stmt::DropRole {
            if_exists,
            role_name: role_name.clone(),
        }
        .to_string(),
    );
    Ok(())
}

pub fn translate_create_policy(
    policy: &ast::CreatePolicy,
    resolver: &Resolver,
    program: &mut ProgramBuilder,
) -> Result<()> {
    let table = main_btree_table(&policy.tbl_name, resolver)?;
    let policy_name = normalize_ident(policy.policy_name.as_str());
    let catalog = catalog(resolver);
    if catalog
        .table(&table.name)
        .is_some_and(|state| state.has_policy(&policy_name))
    {
        bail_parse_error!(
            "policy \"{policy_name}\" for table \"{}\" already exists",
            table.name
        );
    }
    for role in &policy.roles {
        if !catalog.has_role(role.as_str()) {
            bail_parse_error!("role \"{}\" does not exist", normalize_ident(role.as_str()));
        }
    }
    match policy.command {
        PolicyCommand::Insert if policy.using_expr.is_some() => {
            bail_parse_error!("only WITH CHECK expression allowed for INSERT")
        }
        PolicyCommand::Select | PolicyCommand::Delete if policy.check_expr.is_some() => {
            bail_parse_error!("WITH CHECK cannot be applied to SELECT or DELETE")
        }
        _ => {}
    }
    for expr in [&policy.using_expr, &policy.check_expr]
        .into_iter()
        .flatten()
    {
        reject_parameters_and_with(expr)?;
        let mut expr = expr.as_ref().clone();
        bind_to_table(&mut expr, &table, resolver)?;
    }
    let checks_new_rows = match policy.command {
        PolicyCommand::All | PolicyCommand::Update => {
            policy.check_expr.as_ref().or(policy.using_expr.as_ref())
        }
        PolicyCommand::Insert => policy.check_expr.as_ref(),
        PolicyCommand::Select | PolicyCommand::Delete => None,
    };
    if checks_new_rows.is_some_and(|expr| contains_subquery(expr)) {
        bail_parse_error!("subqueries are not supported in policy expressions that check new rows");
    }

    let sql = ast::Stmt::CreatePolicy(Box::new(policy.clone())).to_string();
    let rows = AccessControlRows::open(resolver, program)?;
    rows.emit_insert(program, "policy", &policy_name, &table.name, sql.clone());
    emit_catalog_update(program, resolver, sql);
    Ok(())
}

pub fn translate_drop_policy(
    policy_name: &ast::Name,
    tbl_name: &ast::QualifiedName,
    if_exists: bool,
    resolver: &Resolver,
    program: &mut ProgramBuilder,
) -> Result<()> {
    let table = main_btree_table(tbl_name, resolver)?;
    let name = normalize_ident(policy_name.as_str());
    let exists = catalog(resolver)
        .table(&table.name)
        .is_some_and(|state| state.has_policy(&name));
    if !exists {
        if if_exists {
            return Ok(());
        }
        bail_parse_error!(
            "policy \"{name}\" for table \"{}\" does not exist",
            table.name
        );
    }
    let rows = AccessControlRows::open(resolver, program)?;
    rows.emit_delete(program, &[("policy", 0), (&name, 1), (&table.name, 2)]);
    emit_catalog_update(
        program,
        resolver,
        ast::Stmt::DropPolicy {
            if_exists,
            policy_name: policy_name.clone(),
            tbl_name: tbl_name.clone(),
        }
        .to_string(),
    );
    Ok(())
}

pub fn translate_row_security_change(
    tbl_name: &ast::QualifiedName,
    change: RowSecurityChange,
    resolver: &Resolver,
    program: &mut ProgramBuilder,
) -> Result<()> {
    let table = main_btree_table(tbl_name, resolver)?;
    let state = catalog(resolver)
        .table(&table.name)
        .cloned()
        .unwrap_or_default();
    let (flag, currently_set, set) = match change {
        RowSecurityChange::Enable => ("enable", state.enabled, true),
        RowSecurityChange::Disable => ("enable", state.enabled, false),
        RowSecurityChange::Force => ("force", state.forced, true),
        RowSecurityChange::NoForce => ("force", state.forced, false),
    };
    if currently_set == set {
        return Ok(());
    }
    let sql = ast::Stmt::AlterTable(ast::AlterTable {
        name: ast::QualifiedName::single(ast::Name::exact(table.name.clone())),
        body: ast::AlterTableBody::RowSecurity(change),
    })
    .to_string();
    let rows = AccessControlRows::open(resolver, program)?;
    if set {
        rows.emit_insert(program, "row_security", flag, &table.name, sql.clone());
    } else {
        rows.emit_delete(program, &[("row_security", 0), (flag, 1), (&table.name, 2)]);
    }
    emit_catalog_update(program, resolver, sql);
    Ok(())
}

/// DROP TABLE removes the table's policies and row-level security flags.
pub fn emit_drop_table_row_security_cleanup(
    table_name: &str,
    database_id: usize,
    resolver: &Resolver,
    program: &mut ProgramBuilder,
) -> Result<()> {
    if database_id != MAIN_DB_ID || catalog(resolver).table(table_name).is_none() {
        return Ok(());
    }
    let table_name = normalize_ident(table_name);
    let rows = AccessControlRows::open(resolver, program)?;
    rows.emit_delete(program, &[(&table_name, 2)]);
    emit_catalog_update(
        program,
        resolver,
        ast::Stmt::DropTable {
            if_exists: false,
            tbl_name: ast::QualifiedName::single(ast::Name::exact(table_name)),
        }
        .to_string(),
    );
    Ok(())
}

pub fn reject_rename_of_table_with_row_security(
    table_name: &str,
    resolver: &Resolver,
) -> Result<()> {
    if catalog(resolver)
        .table(table_name)
        .is_some_and(|state| !state.is_empty())
    {
        bail_parse_error!(
            "cannot rename table \"{table_name}\": renaming tables with row-level security or policies is not supported"
        );
    }
    Ok(())
}

/// Policies refer to columns by name, so renaming or dropping a column would
/// leave them pointing at a column that no longer exists.
pub fn reject_column_change_of_table_with_policies(
    table_name: &str,
    resolver: &Resolver,
) -> Result<()> {
    if catalog(resolver)
        .table(table_name)
        .is_some_and(|state| !state.policies.is_empty())
    {
        bail_parse_error!(
            "cannot change columns of table \"{table_name}\": it has row-level security policies"
        );
    }
    Ok(())
}

/// Adds the row-level security filter of every table in the FROM clause of a
/// SELECT to `where_clause`.
pub fn add_select_row_security_filters(
    table_references: &mut TableReferences,
    where_clause: &mut Vec<WhereTerm>,
    resolver: &Resolver,
) -> Result<()> {
    let internal_ids: Vec<_> = table_references
        .joined_tables()
        .iter()
        .map(|table| table.internal_id)
        .collect();
    for internal_id in internal_ids {
        add_row_security_filter(
            table_references,
            internal_id,
            &[PolicyCommand::Select],
            where_clause,
            resolver,
        )?;
    }
    Ok(())
}

/// Adds the `USING` predicates of the policies on a table for `commands` to
/// the front of `where_clause`, so that rows hidden by the policies are never
/// seen by the statement. Each command contributes its own predicate and all
/// of them must hold. For the right side of an outer join the predicate
/// becomes part of the join condition, so hidden rows produce NULLs like rows
/// that do not exist.
pub fn add_row_security_filter(
    table_references: &mut TableReferences,
    internal_id: ast::TableInternalId,
    commands: &[PolicyCommand],
    where_clause: &mut Vec<WhereTerm>,
    resolver: &Resolver,
) -> Result<()> {
    let joined_table = table_references
        .find_joined_table_by_internal_id(internal_id)
        .expect("row-level security filter for a table that is not in the FROM clause");
    let Table::BTree(btree) = &joined_table.table else {
        return Ok(());
    };
    let Some((role, catalog)) =
        row_security_in_effect(&btree.name, joined_table.database_id, resolver)?
    else {
        return Ok(());
    };
    let state = catalog
        .table(&btree.name)
        .expect("row security is in effect");
    if table_references
        .joined_tables()
        .iter()
        .any(|table| table.join_info.as_ref().is_some_and(|j| j.is_full_outer()))
    {
        bail_parse_error!(
            "FULL JOIN with table \"{}\" that has row-level security is not supported",
            btree.name
        );
    }
    reject_recursive_policies(&btree.name, &role, &catalog, resolver)?;
    let from_outer_join = joined_table
        .join_info
        .as_ref()
        .is_some_and(|join_info| join_info.is_outer())
        .then_some(internal_id);
    let identifier = joined_table.identifier.clone();
    let mut scope = TableReferences::new(vec![table_scope(joined_table)], vec![]);
    let mut terms: Vec<WhereTerm> = Vec::new();
    for command in commands {
        let mut predicate = state.using_predicate(&role, *command);
        if !identifier.eq_ignore_ascii_case(&btree.name) {
            requalify(&mut predicate, &btree.name, &identifier)?;
        }
        bind_and_rewrite_expr(
            &mut predicate,
            Some(&mut scope),
            None,
            resolver,
            BindingBehavior::ResultColumnsNotAllowed,
        )?;
        break_predicate_at_and_boundaries(&predicate, &mut terms);
    }
    let bound_table = &scope.joined_tables()[0];
    let joined_table = table_references
        .find_joined_table_by_internal_id_mut(internal_id)
        .expect("joined table exists");
    joined_table
        .col_used_mask
        .clone_from(&bound_table.col_used_mask);
    joined_table
        .column_use_counts
        .clone_from(&bound_table.column_use_counts);
    for term in terms.iter_mut() {
        term.from_outer_join = from_outer_join;
    }
    where_clause.splice(0..0, terms);
    Ok(())
}

/// Emits the `WITH CHECK` predicates of the policies on `table` for
/// `commands`, evaluated on the new row. A row that fails any of them aborts
/// the statement, whatever its conflict resolution.
#[allow(clippy::too_many_arguments)]
pub fn emit_row_security_checks<'a>(
    program: &mut ProgramBuilder,
    resolver: &mut Resolver,
    table: &BTreeTable,
    database_id: usize,
    commands: &[PolicyCommand],
    rowid_reg: usize,
    column_mappings: impl Iterator<Item = (&'a str, usize)>,
    referenced_tables: &TableReferences,
) -> Result<()> {
    let Some((role, catalog)) = row_security_in_effect(&table.name, database_id, resolver)? else {
        return Ok(());
    };
    let state = catalog
        .table(&table.name)
        .expect("row security is in effect");
    let predicates: Vec<ast::Expr> = commands
        .iter()
        .map(|command| match command {
            PolicyCommand::Select => state.using_predicate(&role, *command),
            _ => state.check_predicate(&role, *command),
        })
        .collect();
    if predicates.iter().any(contains_subquery) {
        bail_parse_error!(
            "policies on table \"{}\" with subqueries cannot check new rows",
            table.name
        );
    }
    let mut binding_tables = referenced_tables.clone();
    binding_tables.joined_tables_mut()[0]
        .identifier
        .clone_from(&table.name);
    with_new_row_registers_cached(
        resolver,
        &table.name,
        rowid_reg,
        column_mappings,
        Some(referenced_tables),
        |resolver| {
            for mut predicate in predicates {
                bind_and_rewrite_expr(
                    &mut predicate,
                    Some(&mut binding_tables),
                    None,
                    resolver,
                    BindingBehavior::ResultColumnsNotAllowed,
                )?;
                let result_reg = program.alloc_register();
                translate_expr_no_constant_opt(
                    program,
                    Some(referenced_tables),
                    &predicate,
                    result_reg,
                    resolver,
                    NoConstantOptReason::RegisterReuse,
                )?;
                let passed = program.allocate_label();
                program.emit_insn(Insn::If {
                    reg: result_reg,
                    target_pc: passed,
                    jump_if_null: false,
                });
                program.emit_insn(Insn::Halt {
                    err_code: crate::error::SQLITE_ERROR,
                    description: format!(
                        "new row violates row-level security policy for table \"{}\"",
                        table.name
                    ),
                    on_error: Some(ResolveType::Abort),
                    description_reg: None,
                });
                program.preassign_label_to_next_insn(passed);
            }
            Ok(())
        },
    )
}

pub fn row_security_applies(
    table_name: &str,
    database_id: usize,
    resolver: &Resolver,
) -> Result<bool> {
    Ok(row_security_in_effect(table_name, database_id, resolver)?.is_some())
}

/// The role and catalog when row-level security applies to `table_name` for
/// the statement being compiled. Tables of attached databases that have
/// their own access control catalog are rejected because their policies are
/// not loaded.
fn row_security_in_effect(
    table_name: &str,
    database_id: usize,
    resolver: &Resolver,
) -> Result<Option<(String, Arc<AccessControlCatalog>)>> {
    let Some(role) = resolver.row_security_role.clone() else {
        return Ok(None);
    };
    if database_id != MAIN_DB_ID {
        let has_catalog = resolver.with_schema(database_id, |schema| {
            schema.get_btree_table(ACCESS_CONTROL_TABLE_NAME).is_some()
        });
        if has_catalog {
            bail_parse_error!(
                "table \"{table_name}\" is in an attached database with row-level security, which is not supported"
            );
        }
        return Ok(None);
    }
    let catalog = catalog(resolver);
    if catalog.row_security_for(table_name, Some(&role)).is_none() {
        return Ok(None);
    }
    Ok(Some((role, catalog)))
}

/// Policies whose subqueries read, directly or through other policies or
/// views, the table they protect would expand forever. Follows every table
/// and view the policies of `table_name` read and fails if one of them is
/// already being followed.
fn reject_recursive_policies(
    table_name: &str,
    role: &str,
    catalog: &AccessControlCatalog,
    resolver: &Resolver,
) -> Result<()> {
    let mut path = vec![normalize_ident(table_name)];
    let mut finished = Vec::new();
    follow_policy_reads(&mut path, &mut finished, role, catalog, resolver)
}

fn follow_policy_reads(
    path: &mut Vec<String>,
    finished: &mut Vec<String>,
    role: &str,
    catalog: &AccessControlCatalog,
    resolver: &Resolver,
) -> Result<()> {
    let name = path.last().expect("path is never empty").clone();
    let mut reads = Vec::new();
    if let Some(view) = resolver.schema().get_view(&name) {
        collect_from_clause_table_refs(&view.select_stmt, &mut reads);
    } else if let Some(state) = catalog.row_security_for(&name, Some(role)) {
        let command = if path.len() == 1 {
            None
        } else {
            Some(PolicyCommand::Select)
        };
        for policy in &state.policies {
            if !command.is_none_or(|command| policy.applies_to(role, command)) {
                continue;
            }
            if let Some(expr) = &policy.using_expr {
                collect_subquery_table_refs_in_expr(expr, &mut reads);
            }
        }
    }
    for read in reads {
        if path.contains(&read) {
            bail_parse_error!("infinite recursion detected in policy for table \"{read}\"");
        }
        if finished.contains(&read) {
            continue;
        }
        path.push(read);
        follow_policy_reads(path, finished, role, catalog, resolver)?;
        finished.push(path.pop().expect("pushed above"));
    }
    Ok(())
}

/// Rewrites references qualified with the table name to the alias the query
/// gives the table, including inside subqueries that do not reuse the name.
fn requalify(expr: &mut ast::Expr, table_name: &str, alias: &str) -> Result<()> {
    walk_expr_mut(expr, &mut |expr: &mut ast::Expr| -> Result<WalkControl> {
        match expr {
            ast::Expr::Qualified(table, _) if table.as_str().eq_ignore_ascii_case(table_name) => {
                *table = ast::Name::exact(alias.to_string());
                Ok(WalkControl::Continue)
            }
            ast::Expr::Exists(select) | ast::Expr::Subquery(select) => {
                requalify_select(select, table_name, alias)?;
                Ok(WalkControl::SkipChildren)
            }
            ast::Expr::InSelect { lhs, rhs, .. } => {
                requalify(lhs, table_name, alias)?;
                requalify_select(rhs, table_name, alias)?;
                Ok(WalkControl::SkipChildren)
            }
            _ => Ok(WalkControl::Continue),
        }
    })?;
    Ok(())
}

fn requalify_select(select: &mut ast::Select, table_name: &str, alias: &str) -> Result<()> {
    let ast::OneSelect::Select {
        columns,
        from,
        where_clause,
        group_by,
        ..
    } = &mut select.body.select
    else {
        return Ok(());
    };
    let mut introduced = Vec::new();
    if let Some(from) = from.as_ref() {
        introduced.push(from_identifier(&from.select));
        introduced.extend(from.joins.iter().map(|join| from_identifier(&join.table)));
    }
    if introduced
        .iter()
        .flatten()
        .any(|name| name.eq_ignore_ascii_case(alias))
    {
        bail_parse_error!(
            "a policy subquery on table \"{table_name}\" uses the name \"{alias}\" that the query gives the table"
        );
    }
    if introduced
        .iter()
        .flatten()
        .any(|name| name.eq_ignore_ascii_case(table_name))
    {
        return Ok(());
    }
    for column in columns.iter_mut() {
        if let ast::ResultColumn::Expr(expr, _) = column {
            requalify(expr, table_name, alias)?;
        }
    }
    if let Some(from) = from.as_mut() {
        for join in from.joins.iter_mut() {
            if let Some(ast::JoinConstraint::On(expr)) = &mut join.constraint {
                requalify(expr, table_name, alias)?;
            }
        }
    }
    if let Some(expr) = where_clause {
        requalify(expr, table_name, alias)?;
    }
    if let Some(group_by) = group_by {
        for expr in group_by.exprs.iter_mut() {
            requalify(expr, table_name, alias)?;
        }
        if let Some(having) = &mut group_by.having {
            requalify(having, table_name, alias)?;
        }
    }
    Ok(())
}

fn from_identifier(table: &ast::SelectTable) -> Option<String> {
    match table {
        ast::SelectTable::Table(name, alias, _) | ast::SelectTable::TableCall(name, _, alias) => {
            Some(match alias {
                Some(alias) => normalize_ident(alias.name().as_str()),
                None => normalize_ident(name.name.as_str()),
            })
        }
        ast::SelectTable::Select(_, alias) | ast::SelectTable::Sub(_, alias) => alias
            .as_ref()
            .map(|alias| normalize_ident(alias.name().as_str())),
    }
}

fn contains_subquery(expr: &ast::Expr) -> bool {
    let mut found = false;
    let _ = walk_expr(expr, &mut |expr: &ast::Expr| -> Result<WalkControl> {
        if matches!(
            expr,
            ast::Expr::Exists(_) | ast::Expr::Subquery(_) | ast::Expr::InSelect { .. }
        ) {
            found = true;
            return Ok(WalkControl::SkipChildren);
        }
        Ok(WalkControl::Continue)
    });
    found
}

fn catalog(resolver: &Resolver) -> Arc<AccessControlCatalog> {
    resolver.with_schema(MAIN_DB_ID, |schema| schema.access_control.clone())
}

fn main_btree_table(name: &ast::QualifiedName, resolver: &Resolver) -> Result<Arc<BTreeTable>> {
    let database_id = resolver.resolve_existing_table_database_id_qualified(name)?;
    if database_id != MAIN_DB_ID {
        bail_parse_error!("row-level security is only supported for tables in the main database");
    }
    match resolver.schema().get_btree_table(name.name.as_str()) {
        Some(table) => Ok(table),
        None => bail_parse_error!("no such table: {}", name.name.as_str()),
    }
}

/// Policies are stored and later added to other statements, so a parameter
/// would take its value from whatever statement the policy is added to.
fn reject_parameters_and_with(expr: &ast::Expr) -> Result<()> {
    walk_expr(expr, &mut |expr: &ast::Expr| -> Result<WalkControl> {
        match expr {
            ast::Expr::Variable(_) => {
                bail_parse_error!("parameters are not allowed in policy expressions")
            }
            ast::Expr::Exists(select) | ast::Expr::Subquery(select) => {
                reject_parameters_and_with_in_select(select)?;
                Ok(WalkControl::SkipChildren)
            }
            ast::Expr::InSelect { lhs, rhs, .. } => {
                reject_parameters_and_with(lhs)?;
                reject_parameters_and_with_in_select(rhs)?;
                Ok(WalkControl::SkipChildren)
            }
            _ => Ok(WalkControl::Continue),
        }
    })?;
    Ok(())
}

fn reject_parameters_and_with_in_select(select: &ast::Select) -> Result<()> {
    if select.with.is_some() {
        bail_parse_error!("WITH is not supported in policy expressions");
    }
    for one in std::iter::once(&select.body.select).chain(
        select
            .body
            .compounds
            .iter()
            .map(|compound| &compound.select),
    ) {
        match one {
            ast::OneSelect::Select {
                columns,
                from,
                where_clause,
                group_by,
                window_clause,
                ..
            } => {
                if !window_clause.is_empty() {
                    bail_parse_error!("WINDOW is not supported in policy expressions");
                }
                for column in columns {
                    if let ast::ResultColumn::Expr(expr, _) = column {
                        reject_parameters_and_with(expr)?;
                    }
                }
                if let Some(from) = from {
                    reject_parameters_and_with_in_from(&from.select)?;
                    for join in &from.joins {
                        reject_parameters_and_with_in_from(&join.table)?;
                        if let Some(ast::JoinConstraint::On(expr)) = &join.constraint {
                            reject_parameters_and_with(expr)?;
                        }
                    }
                }
                if let Some(expr) = where_clause {
                    reject_parameters_and_with(expr)?;
                }
                if let Some(group_by) = group_by {
                    for expr in &group_by.exprs {
                        reject_parameters_and_with(expr)?;
                    }
                    if let Some(having) = &group_by.having {
                        reject_parameters_and_with(having)?;
                    }
                }
            }
            ast::OneSelect::Values(rows) => {
                for expr in rows.iter().flatten() {
                    reject_parameters_and_with(expr)?;
                }
            }
        }
    }
    for sorted in &select.order_by {
        reject_parameters_and_with(&sorted.expr)?;
    }
    if let Some(limit) = &select.limit {
        reject_parameters_and_with(&limit.expr)?;
        if let Some(offset) = &limit.offset {
            reject_parameters_and_with(offset)?;
        }
    }
    Ok(())
}

fn reject_parameters_and_with_in_from(table: &ast::SelectTable) -> Result<()> {
    match table {
        ast::SelectTable::Table(..) => Ok(()),
        ast::SelectTable::TableCall(_, args, _) => {
            for arg in args {
                reject_parameters_and_with(arg)?;
            }
            Ok(())
        }
        ast::SelectTable::Select(select, _) => reject_parameters_and_with_in_select(select),
        ast::SelectTable::Sub(from, _) => {
            reject_parameters_and_with_in_from(&from.select)?;
            for join in &from.joins {
                reject_parameters_and_with_in_from(&join.table)?;
                if let Some(ast::JoinConstraint::On(expr)) = &join.constraint {
                    reject_parameters_and_with(expr)?;
                }
            }
            Ok(())
        }
    }
}

fn table_scope(joined_table: &JoinedTable) -> JoinedTable {
    let mut scope = joined_table.clone();
    scope.join_info = None;
    scope
}

fn bind_to_table(expr: &mut ast::Expr, table: &Arc<BTreeTable>, resolver: &Resolver) -> Result<()> {
    let table_ref = Table::BTree(table.clone());
    let mut scope = TableReferences::new(
        vec![JoinedTable {
            op: Operation::default_scan_for(&table_ref),
            table: table_ref,
            identifier: table.name.clone(),
            internal_id: ast::TableInternalId::from(0),
            join_info: None,
            col_used_mask: ColumnUsedMask::default(),
            column_use_counts: Vec::new(),
            expression_index_usages: Vec::new(),
            database_id: MAIN_DB_ID,
            indexed: None,
            plan_estimate: None,
        }],
        vec![],
    );
    bind_and_rewrite_expr(
        expr,
        Some(&mut scope),
        None,
        resolver,
        BindingBehavior::ResultColumnsNotAllowed,
    )
}

fn emit_catalog_update(program: &mut ProgramBuilder, resolver: &Resolver, sql: String) {
    program.emit_insn(Insn::UpdateAccessControl {
        db: MAIN_DB_ID,
        sql,
    });
    program.emit_insn(Insn::SetCookie {
        db: MAIN_DB_ID,
        cookie: Cookie::SchemaVersion,
        value: (resolver.schema().schema_version + 1) as i32,
        p5: 0,
    });
}

/// Write cursor on `__turso_internal_access_control`, which is created on first use.
struct AccessControlRows {
    cursor_id: usize,
}

impl AccessControlRows {
    fn open(resolver: &Resolver, program: &mut ProgramBuilder) -> Result<Self> {
        let (table, root_page) = match resolver.schema().get_btree_table(ACCESS_CONTROL_TABLE_NAME)
        {
            Some(table) => {
                let root_page = RegisterOrLiteral::Literal(table.root_page);
                (table, root_page)
            }
            None => Self::emit_create_table(resolver, program)?,
        };
        let cursor_id = program.alloc_cursor_id(CursorType::BTreeTable(table));
        program.emit_insn(Insn::OpenWrite {
            cursor_id,
            root_page,
            db: MAIN_DB_ID,
        });
        Ok(Self { cursor_id })
    }

    fn emit_create_table(
        resolver: &Resolver,
        program: &mut ProgramBuilder,
    ) -> Result<(Arc<BTreeTable>, RegisterOrLiteral<i64>)> {
        let root_reg = program.alloc_register();
        program.emit_insn(Insn::CreateBtree {
            db: MAIN_DB_ID,
            root: root_reg,
            flags: CreateBTreeFlags::new_table(),
        });
        let schema_table = resolver
            .schema()
            .get_btree_table(SQLITE_TABLEID)
            .expect("sqlite_schema exists");
        let schema_cursor_id = program.alloc_cursor_id(CursorType::BTreeTable(schema_table));
        program.emit_insn(Insn::OpenWrite {
            cursor_id: schema_cursor_id,
            root_page: 1i64.into(),
            db: MAIN_DB_ID,
        });
        emit_schema_entry(
            program,
            resolver,
            schema_cursor_id,
            None,
            SchemaEntryType::Table,
            ACCESS_CONTROL_TABLE_NAME,
            ACCESS_CONTROL_TABLE_NAME,
            root_reg,
            Some(ACCESS_CONTROL_TABLE_SQL.to_string()),
        )?;
        program.emit_insn(Insn::ParseSchema {
            db: schema_cursor_id,
            where_clause: Some(format!(
                "tbl_name = '{ACCESS_CONTROL_TABLE_NAME}' AND type != 'trigger'"
            )),
            trigger_target_database_id: None,
        });
        let table = Arc::new(BTreeTable::from_sql(ACCESS_CONTROL_TABLE_SQL, 0)?);
        Ok((table, RegisterOrLiteral::Register(root_reg)))
    }

    fn emit_insert(
        &self,
        program: &mut ProgramBuilder,
        kind: &str,
        name: &str,
        tbl_name: &str,
        sql: String,
    ) {
        let rowid_reg = program.alloc_register();
        program.emit_insn(Insn::NewRowid {
            cursor: self.cursor_id,
            rowid_reg,
            prev_largest_reg: 0,
        });
        let first_reg = program.emit_string8_new_reg(kind.to_string());
        program.emit_string8_new_reg(name.to_string());
        program.emit_string8_new_reg(tbl_name.to_string());
        program.emit_string8_new_reg(sql);
        let record_reg = program.alloc_register();
        program.emit_insn(Insn::MakeRecord {
            start_reg: to_u32(first_reg),
            count: to_u32(4),
            dest_reg: to_u32(record_reg),
            index_name: None,
            affinity_str: None,
        });
        program.emit_insn(Insn::Insert {
            cursor: self.cursor_id,
            key_reg: rowid_reg,
            record_reg,
            flag: InsertFlags::new(),
            table_name: ACCESS_CONTROL_TABLE_NAME.to_string(),
        });
    }

    /// Deletes every row whose columns equal the given `(value, column)` pairs.
    fn emit_delete(&self, program: &mut ProgramBuilder, matches: &[(&str, usize)]) {
        let done = program.allocate_label();
        let loop_start = program.allocate_label();
        program.emit_insn(Insn::Rewind {
            cursor_id: self.cursor_id,
            pc_if_empty: done,
        });
        program.preassign_label_to_next_insn(loop_start);
        let next = program.allocate_label();
        for (value, column) in matches {
            let column_reg = program.alloc_register();
            program.emit_column_or_rowid(self.cursor_id, *column, column_reg);
            let value_reg = program.emit_string8_new_reg(value.to_string());
            program.emit_insn(Insn::Ne {
                lhs: column_reg,
                rhs: value_reg,
                target_pc: next,
                flags: CmpInsFlags::default(),
                collation: None,
            });
        }
        program.emit_insn(Insn::Delete {
            cursor_id: self.cursor_id,
            table_name: ACCESS_CONTROL_TABLE_NAME.to_string(),
            is_part_of_update: false,
        });
        program.preassign_label_to_next_insn(next);
        program.emit_insn(Insn::Next {
            cursor_id: self.cursor_id,
            pc_if_next: loop_start,
            fullscan: false,
            is_index: false,
        });
        program.preassign_label_to_next_insn(done);
    }
}
