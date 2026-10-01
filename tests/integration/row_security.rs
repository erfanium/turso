#[cfg(test)]
mod tests {
    use crate::common::{ExecRows, TempDatabase};

    fn create_docs_with_owner_policy(conn: &std::sync::Arc<turso_core::Connection>) {
        for sql in [
            "CREATE TABLE docs(id INTEGER PRIMARY KEY, owner TEXT)",
            "INSERT INTO docs VALUES (1, 'alice'), (2, 'bob'), (3, 'alice')",
            "CREATE ROLE alice",
            "ALTER TABLE docs ENABLE ROW LEVEL SECURITY",
            "CREATE POLICY alice_rows ON docs USING (owner = 'alice')",
        ] {
            conn.execute(sql).unwrap();
        }
    }

    #[test]
    fn test_row_security_survives_reopen() {
        for mvcc in [false, true] {
            let db = TempDatabase::builder().with_mvcc(mvcc).build();
            let conn = db.connect_limbo();
            create_docs_with_owner_policy(&conn);
            conn.close().unwrap();
            let path = db.path.clone();
            drop(db);

            let db = TempDatabase::new_with_existent(&path);
            let conn = db.connect_limbo();
            conn.set_role(Some("alice")).unwrap();
            let rows: Vec<(i64,)> = conn.exec_rows("SELECT id FROM docs ORDER BY id");
            assert_eq!(rows, vec![(1,), (3,)], "mvcc={mvcc}");
            conn.close().unwrap();
        }
    }

    #[test]
    fn test_policy_created_on_one_connection_applies_to_another() {
        for mvcc in [false, true] {
            let db = TempDatabase::builder().with_mvcc(mvcc).build();
            let admin = db.connect_limbo();
            let reader = db.connect_limbo();
            let _: Vec<(i64,)> = reader.exec_rows("SELECT count(*) FROM sqlite_schema");

            create_docs_with_owner_policy(&admin);

            reader.set_role(Some("alice")).unwrap();
            let rows: Vec<(i64,)> = reader.exec_rows("SELECT id FROM docs ORDER BY id");
            assert_eq!(rows, vec![(1,), (3,)], "mvcc={mvcc}");
        }
    }

    #[test]
    fn test_statement_prepared_before_set_role_is_filtered() {
        for mvcc in [false, true] {
            let db = TempDatabase::builder().with_mvcc(mvcc).build();
            let conn = db.connect_limbo();
            create_docs_with_owner_policy(&conn);

            let mut stmt = conn.prepare("SELECT count(*) FROM docs").unwrap();
            conn.set_role(Some("alice")).unwrap();
            let rows = stmt.run_collect_rows().unwrap();
            assert_eq!(
                rows,
                vec![vec![turso_core::Value::from_i64(2)]],
                "mvcc={mvcc}"
            );

            stmt.reset().unwrap();
            conn.set_role(None).unwrap();
            let rows = stmt.run_collect_rows().unwrap();
            assert_eq!(
                rows,
                vec![vec![turso_core::Value::from_i64(3)]],
                "mvcc={mvcc}"
            );
        }
    }

    #[test]
    fn test_drop_table_removes_its_policies() {
        let db = TempDatabase::builder().build();
        let conn = db.connect_limbo();
        create_docs_with_owner_policy(&conn);
        conn.execute("DROP TABLE docs").unwrap();
        conn.execute("CREATE TABLE docs(id INTEGER PRIMARY KEY, owner TEXT)")
            .unwrap();
        conn.execute("INSERT INTO docs VALUES (1, 'bob')").unwrap();

        conn.set_role(Some("alice")).unwrap();
        let rows: Vec<(i64,)> = conn.exec_rows("SELECT id FROM docs");
        assert_eq!(rows, vec![(1,)]);
    }

    #[test]
    fn test_access_catalog_cannot_be_modified_directly() {
        let db = TempDatabase::builder().build();
        let conn = db.connect_limbo();
        create_docs_with_owner_policy(&conn);
        let err = conn
            .execute("DELETE FROM __turso_internal_access_control")
            .unwrap_err();
        assert!(err.to_string().contains("may not be modified"), "{err}");
    }

    #[test]
    fn test_row_security_cannot_be_enabled_on_system_tables() {
        let db = TempDatabase::builder().build();
        let conn = db.connect_limbo();
        create_docs_with_owner_policy(&conn);
        for table in ["__turso_internal_access_control", "sqlite_schema"] {
            let err = conn
                .execute(format!("ALTER TABLE {table} ENABLE ROW LEVEL SECURITY"))
                .unwrap_err();
            assert!(
                err.to_string().contains("may not be modified"),
                "{table}: {err}"
            );
        }
    }

    #[test]
    fn test_role_used_by_policy_cannot_be_dropped() {
        let db = TempDatabase::builder().build();
        let conn = db.connect_limbo();
        create_docs_with_owner_policy(&conn);
        conn.execute("CREATE POLICY p ON docs TO alice USING (1)")
            .unwrap();
        let err = conn.execute("DROP ROLE alice").unwrap_err();
        assert!(err.to_string().contains("depends on it"), "{err}");
        conn.execute("DROP POLICY p ON docs").unwrap();
        conn.execute("DROP ROLE alice").unwrap();
    }

    fn connect_as_alice(
        opts: turso_core::DatabaseOpts,
    ) -> (TempDatabase, std::sync::Arc<turso_core::Connection>) {
        let db = TempDatabase::builder().with_opts(opts).build();
        let conn = db.connect_limbo();
        create_docs_with_owner_policy(&conn);
        (db, conn)
    }

    fn assert_error_contains(result: turso_core::Result<()>, expected: &str) {
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains(expected),
            "expected \"{expected}\", got: {err}"
        );
    }

    #[test]
    fn test_vacuum_under_role_keeps_hidden_rows() {
        let (_db, conn) = connect_as_alice(turso_core::DatabaseOpts::new().with_vacuum(true));
        conn.set_role(Some("alice")).unwrap();
        conn.execute("VACUUM").unwrap();
        conn.set_role(None).unwrap();
        let rows: Vec<(i64,)> = conn.exec_rows("SELECT id FROM docs ORDER BY id");
        assert_eq!(rows, vec![(1,), (2,), (3,)]);
    }

    #[test]
    fn test_foreign_key_cascade_deletes_hidden_child_rows() {
        let (_db, conn) = connect_as_alice(turso_core::DatabaseOpts::new());
        for sql in [
            "PRAGMA foreign_keys = ON",
            "CREATE TABLE parent(id INTEGER PRIMARY KEY)",
            "CREATE TABLE child(id INTEGER PRIMARY KEY, parent_id INTEGER REFERENCES parent(id) ON DELETE CASCADE, owner TEXT)",
            "INSERT INTO parent VALUES (1)",
            "INSERT INTO child VALUES (1, 1, 'alice'), (2, 1, 'bob')",
            "ALTER TABLE child ENABLE ROW LEVEL SECURITY",
            "CREATE POLICY alice_children ON child USING (owner = 'alice')",
        ] {
            conn.execute(sql).unwrap();
        }
        conn.set_role(Some("alice")).unwrap();
        conn.execute("DELETE FROM parent WHERE id = 1").unwrap();
        conn.set_role(None).unwrap();
        let rows: Vec<(i64,)> = conn.exec_rows("SELECT count(*) FROM child");
        assert_eq!(rows, vec![(0,)]);
    }

    #[test]
    fn test_protected_table_in_attached_database_is_rejected() {
        let other = TempDatabase::builder().build();
        let other_conn = other.connect_limbo();
        create_docs_with_owner_policy(&other_conn);
        other_conn.close().unwrap();
        let other_path = other.path.clone();
        drop(other);

        let db = TempDatabase::builder()
            .with_opts(turso_core::DatabaseOpts::new().with_attach(true))
            .build();
        let conn = db.connect_limbo();
        conn.execute("CREATE ROLE alice").unwrap();
        conn.execute(format!("ATTACH '{}' AS other", other_path.display()))
            .unwrap();
        conn.set_role(Some("alice")).unwrap();
        assert_error_contains(
            conn.execute("SELECT * FROM other.docs"),
            "row-level security",
        );
        assert_error_contains(
            conn.execute("UPDATE other.docs SET owner = 'alice'"),
            "row-level security",
        );
        assert_error_contains(
            conn.execute("INSERT INTO other.docs VALUES (4, 'bob')"),
            "row-level security",
        );
    }

    #[test]
    fn test_replace_on_protected_table_is_rejected() {
        let (_db, conn) = connect_as_alice(turso_core::DatabaseOpts::new());
        for sql in [
            "CREATE TABLE replacing(id INTEGER PRIMARY KEY ON CONFLICT REPLACE, owner TEXT)",
            "INSERT INTO replacing VALUES (1, 'alice'), (2, 'bob')",
            "ALTER TABLE replacing ENABLE ROW LEVEL SECURITY",
            "CREATE POLICY alice_rows ON replacing USING (owner = 'alice')",
        ] {
            conn.execute(sql).unwrap();
        }
        conn.set_role(Some("alice")).unwrap();
        assert_error_contains(
            conn.execute("INSERT INTO replacing VALUES (2, 'alice')"),
            "REPLACE",
        );
        assert_error_contains(
            conn.execute("UPDATE OR REPLACE docs SET id = 2 WHERE id = 1"),
            "REPLACE",
        );
        assert_error_contains(
            conn.execute("UPDATE replacing SET id = 2 WHERE id = 1"),
            "REPLACE",
        );
        conn.set_role(None).unwrap();
        let rows: Vec<(i64, String)> =
            conn.exec_rows("SELECT id, owner FROM replacing ORDER BY id");
        assert_eq!(rows, vec![(1, "alice".to_string()), (2, "bob".to_string())]);
    }

    #[test]
    fn test_recursive_policy_is_an_error() {
        let (_db, conn) = connect_as_alice(turso_core::DatabaseOpts::new());
        for sql in [
            "CREATE TABLE a(x INTEGER)",
            "CREATE TABLE b(x INTEGER)",
            "ALTER TABLE a ENABLE ROW LEVEL SECURITY",
            "ALTER TABLE b ENABLE ROW LEVEL SECURITY",
            "CREATE POLICY a_rows ON a FOR SELECT USING (EXISTS (SELECT 1 FROM b))",
            "CREATE POLICY b_rows ON b FOR SELECT USING (EXISTS (SELECT 1 FROM a))",
            "CREATE POLICY self_rows ON docs FOR SELECT USING (EXISTS (SELECT 1 FROM docs))",
        ] {
            conn.execute(sql).unwrap();
        }
        conn.set_role(Some("alice")).unwrap();
        assert_error_contains(conn.execute("SELECT * FROM docs"), "infinite recursion");
        assert_error_contains(conn.execute("SELECT * FROM a"), "infinite recursion");
    }

    #[test]
    fn test_failed_check_aborts_or_fail_statement() {
        let (_db, conn) = connect_as_alice(turso_core::DatabaseOpts::new());
        conn.execute(
            "CREATE POLICY small_ids ON docs AS RESTRICTIVE FOR INSERT WITH CHECK (id < 5)",
        )
        .unwrap();
        conn.set_role(Some("alice")).unwrap();
        assert_error_contains(
            conn.execute("INSERT OR FAIL INTO docs VALUES (4, 'alice'), (7, 'alice')"),
            "row-level security",
        );
        conn.set_role(None).unwrap();
        let rows: Vec<(i64,)> = conn.exec_rows("SELECT count(*) FROM docs");
        assert_eq!(rows, vec![(3,)]);
    }

    #[test]
    fn test_failed_check_keeps_earlier_statements_of_transaction() {
        let (_db, conn) = connect_as_alice(turso_core::DatabaseOpts::new());
        conn.execute("CREATE TABLE plain(a INTEGER)").unwrap();
        conn.execute("ALTER TABLE plain ENABLE ROW LEVEL SECURITY")
            .unwrap();
        conn.execute("CREATE POLICY small ON plain USING (a < 5)")
            .unwrap();
        conn.set_role(Some("alice")).unwrap();
        conn.execute("BEGIN").unwrap();
        conn.execute("INSERT INTO plain VALUES (0)").unwrap();
        assert_error_contains(
            conn.execute("INSERT INTO plain VALUES (1), (7)"),
            "row-level security",
        );
        conn.execute("COMMIT").unwrap();
        let rows: Vec<(i64,)> = conn.exec_rows("SELECT a FROM plain");
        assert_eq!(rows, vec![(0,)]);
    }

    #[test]
    fn test_full_join_with_protected_left_side_is_rejected() {
        let (_db, conn) = connect_as_alice(turso_core::DatabaseOpts::new());
        conn.execute("CREATE TABLE u(id INTEGER)").unwrap();
        conn.set_role(Some("alice")).unwrap();
        assert_error_contains(
            conn.execute("SELECT * FROM docs FULL JOIN u ON docs.id = u.id"),
            "FULL JOIN",
        );
    }

    #[test]
    fn test_policy_used_as_check_cannot_contain_subquery() {
        let (_db, conn) = connect_as_alice(turso_core::DatabaseOpts::new());
        assert_error_contains(
            conn.execute("CREATE POLICY p ON docs USING (EXISTS (SELECT 1))"),
            "subquer",
        );
        assert_error_contains(
            conn.execute("CREATE POLICY p ON docs FOR INSERT WITH CHECK (EXISTS (SELECT 1))"),
            "subquer",
        );
        conn.execute("CREATE POLICY p ON docs FOR SELECT USING (EXISTS (SELECT 1))")
            .unwrap();
    }

    #[test]
    fn test_policy_subquery_works_with_table_alias() {
        let (_db, conn) = connect_as_alice(turso_core::DatabaseOpts::new());
        for sql in [
            "CREATE TABLE acl(doc_id INTEGER)",
            "INSERT INTO acl VALUES (2)",
            "CREATE POLICY acl_rows ON docs FOR SELECT USING (EXISTS (SELECT 1 FROM acl WHERE acl.doc_id = docs.id))",
        ] {
            conn.execute(sql).unwrap();
        }
        conn.set_role(Some("alice")).unwrap();
        let rows: Vec<(i64,)> =
            conn.exec_rows("SELECT other.id FROM docs AS other ORDER BY other.id");
        assert_eq!(rows, vec![(1,), (2,), (3,)]);
        let rows: Vec<(i64,)> = conn.exec_rows("SELECT docs.id FROM docs ORDER BY docs.id");
        assert_eq!(rows, vec![(1,), (2,), (3,)]);
    }

    #[test]
    fn test_column_changes_on_table_with_policies_are_rejected() {
        let (_db, conn) = connect_as_alice(turso_core::DatabaseOpts::new());
        assert_error_contains(
            conn.execute("ALTER TABLE docs RENAME COLUMN owner TO o"),
            "row-level security",
        );
        assert_error_contains(
            conn.execute("ALTER TABLE docs DROP COLUMN owner"),
            "row-level security",
        );
    }

    #[test]
    fn test_policy_to_public_applies_to_every_role() {
        let (_db, conn) = connect_as_alice(turso_core::DatabaseOpts::new());
        conn.execute("CREATE POLICY everyone ON docs TO PUBLIC USING (owner = 'bob')")
            .unwrap();
        assert_error_contains(conn.execute("CREATE ROLE public"), "reserved");
        conn.set_role(Some("alice")).unwrap();
        let rows: Vec<(i64,)> = conn.exec_rows("SELECT count(*) FROM docs");
        assert_eq!(rows, vec![(3,)]);
    }

    #[test]
    fn test_policy_cannot_contain_bind_parameters() {
        let (_db, conn) = connect_as_alice(turso_core::DatabaseOpts::new());
        assert_error_contains(
            conn.execute("CREATE POLICY p ON docs USING (owner = ?1)"),
            "parameter",
        );
        assert_error_contains(
            conn.execute("CREATE POLICY p ON docs FOR SELECT USING (EXISTS (SELECT 1 WHERE ?1))"),
            "parameter",
        );
    }
}
