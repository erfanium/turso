# MySQL frontend

A MySQL dialect for the engine, in the same spirit as the Postgres frontend in
`postgres/`, aimed at running an application's test suite against an in-memory
database instead of a MySQL server.

| Directory | What it is |
|---|---|
| `frontend/` | `turso_mysql`: rewrites MySQL statements to the engine's SQL, derives MySQL's result types, keeps the MySQL type catalog, implements MySQL scalar functions, and manages sessions over named in-memory databases. |
| `server/` | `turso_mysql_server`: the MySQL wire protocol (text protocol) over any byte stream, and `tursomysql`, a standalone TCP server. |
| `node/` | `turso_mysql_node`: a Node.js addon that runs the protocol in-process over an in-memory channel. |
| `packages/mylite/` | `@erfanium/mylite`: the npm package — `mysql2/promise` with the socket replaced by the addon. |

It depends on two changes in `core/`: declared column types are reported
through subqueries and CTEs (`core/statement.rs`), and locale collations accept
a comparison strength (`core/translate/collate.rs`), which is how MySQL's
case- and accent-insensitive default collation is expressed.

This is an emulation built to make one application's suite pass, not a
complete MySQL: see the limits listed in `packages/mylite/README.md`.

```
cargo test -p turso_mysql
cd mysql/packages/mylite && npm run build:native && npm test
```
