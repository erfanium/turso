'use strict';

const { randomUUID } = require('node:crypto');
const { createStream } = require('./stream');
const { native } = require('./native');

/** Database used when a connection config names none. */
const DEFAULT_DATABASE = 'mylite';

/**
 * mysql2 config with the socket swapped for an in-process stream. Host, port
 * and credentials are accepted and ignored; `database` picks the in-memory
 * database, which is created on first use.
 */
function inProcess(config) {
  const options = typeof config === 'string' ? { uri: config } : { ...config };
  if (!options.uri && !options.database) options.database = DEFAULT_DATABASE;
  options.stream = createStream;
  return options;
}

/**
 * The engine speaks the text protocol only, so `execute()` — a server-side
 * prepared statement in mysql2 — is run as a `query()` with the same
 * arguments. Results have the same shape.
 */
function executeAsQuery(coreConnection) {
  coreConnection.execute = coreConnection.query;
  return coreConnection;
}

function patchCorePool(corePool) {
  corePool.on('connection', executeAsQuery);
  return corePool;
}

/**
 * Wrap `mysql2/promise` so everything it creates talks to the in-process
 * engine. The `execute` shim lives on the callback objects underneath the
 * promise ones (`pool.pool`, `connection.connection`).
 */
function wrap(real) {
  const createPool = (config) => {
    const pool = real.createPool(inProcess(config));
    patchCorePool(pool.pool);
    return pool;
  };

  const createConnection = async (config) => {
    const connection = await real.createConnection(inProcess(config));
    executeAsQuery(connection.connection);
    return connection;
  };

  const createPoolCluster = (config) => {
    const cluster = real.createPoolCluster(config);
    const add = cluster.add.bind(cluster);
    cluster.add = (id, nodeConfig) =>
      typeof id === 'object' || nodeConfig === undefined
        ? add(inProcess(id))
        : add(id, inProcess(nodeConfig));
    return cluster;
  };

  /**
   * One isolated in-memory database, in the spirit of PGlite:
   *
   *     const db = new MyLite();
   *     await db.query('create table t (id int primary key)');
   *     const pool = db.createPool();
   */
  class MyLite {
    constructor(options = {}) {
      this.database =
        options.database ?? `mylite_${randomUUID().replaceAll('-', '')}`;
      this._pool = undefined;
      this._closed = false;
    }

    _config(config) {
      const options = typeof config === 'string' ? {} : { ...config };
      delete options.uri;
      return { ...options, database: this.database };
    }

    /** A mysql2 pool on this database. The caller ends it. */
    createPool(config) {
      return createPool(this._config(config));
    }

    /** A mysql2 connection on this database. The caller ends it. */
    createConnection(config) {
      return createConnection(this._config(config));
    }

    /** The pool behind `query()` and `execute()`. */
    get pool() {
      if (this._closed) throw new Error('MyLite database is closed');
      this._pool ??= this.createPool();
      return this._pool;
    }

    query(...args) {
      return this.pool.query(...args);
    }

    execute(...args) {
      return this.pool.execute(...args);
    }

    /** End the internal pool and free the database. */
    async close() {
      if (this._closed) return;
      this._closed = true;
      const pool = this._pool;
      this._pool = undefined;
      if (pool) await pool.end();
      native().dropDatabase(this.database);
    }

    async [Symbol.asyncDispose]() {
      await this.close();
    }
  }

  return {
    ...real,
    createPool,
    createConnection,
    createPoolCluster,
    MyLite,
    /** Free a named in-memory database. */
    dropDatabase: (name) => native().dropDatabase(name),
  };
}

module.exports = { wrap };
