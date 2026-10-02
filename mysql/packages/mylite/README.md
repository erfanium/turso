# @erfanium/mylite

In-memory, in-process MySQL for Node.js. A drop-in for `mysql2/promise` backed by the
[Turso](https://github.com/tursodatabase/turso) engine: no server to start, no
socket, no container. Built for test suites.

```js
const mysql = require('@erfanium/mylite'); // instead of 'mysql2/promise'

const pool = mysql.createPool({ database: 'app' });
await pool.query('create table users (id int auto_increment primary key, name varchar(255))');
const [result] = await pool.query('insert into users (name) values (?)', ['Ada']);
const [rows] = await pool.query('select * from users where id = ?', [result.insertId]);
await pool.end();
```

Or, in the spirit of PGlite, one isolated database per instance:

```js
const { MyLite } = require('@erfanium/mylite');

const db = new MyLite();
await db.query('create table t (id int primary key)');
const pool = db.createPool(); // a regular mysql2 pool on this database
await db.close();             // frees the database
```

## How it works

`@erfanium/mylite` exports everything `mysql2/promise` exports (it is also
reachable as `@erfanium/mylite/promise`). The callback API of plain `mysql2`
is not wrapped. `createPool`, `createConnection` and
`createPoolCluster` return real mysql2 objects whose transport is an in-memory
stream to an engine running inside the process, so pooling, escaping, type
casting and streaming are mysql2's own.

- `database` selects the in-memory database; it is created on first use.
  Connections that name the same database share it, different names are
  isolated. Host, port and credentials are ignored.
- A database lives until the process exits, `dropDatabase(name)` is called, or
  its `MyLite` instance is closed.
- An open connection keeps the process alive, like a socket. Set
  `MYLITE_UNREF=1` to let the process exit with connections open.

To use it under an existing codebase without touching imports, alias the
module in your test runner, for example in Vitest:

```js
resolve: {
  alias: [
    { find: /^mysql2\/promise$/, replacement: '@erfanium/mylite' },
  ],
}
```

## It is an emulation

Statements are translated to the engine's SQL, so this is not MySQL:

- Write transactions are serialized and `FOR UPDATE` is a plain read. Tests of
  lock contention prove nothing here.
- `DECIMAL` arithmetic is done in floating point and formatted to MySQL's
  scale.
- Rows a query does not order come back in primary key order, as InnoDB's
  clustered index would usually give; MySQL can differ when it picks a
  secondary index.
- A string literal shaped like an ISO datetime is normalized to MySQL's
  datetime form.
- Only the text protocol exists. `execute()` runs as `query()`.
- Error codes match for duplicate keys, unknown tables and unknown columns;
  messages do not.

Keep a run against real MySQL as the reference.

## Native binary

The engine ships as a prebuilt addon under `prebuilds/<platform>-<arch>/`.
This build includes `linux-x64` only. The source is the `mysql/` directory of
[erfanium/turso](https://github.com/erfanium/turso/tree/mysql-frontend/mysql);
`npm run build:native` builds a binary for the current machine from that
workspace, and `MYLITE_NATIVE_PATH` points the loader at a binary elsewhere.
