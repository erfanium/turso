import type {
  Connection,
  ConnectionOptions,
  Pool,
  PoolOptions,
} from 'mysql2/promise';

export * from 'mysql2/promise';

export interface MyLiteOptions {
  /**
   * Name of the in-memory database. Instances with the same name share one
   * database; by default every instance gets a fresh one.
   */
  database?: string;
}

/** One isolated in-memory MySQL database inside this process. */
export class MyLite {
  constructor(options?: MyLiteOptions);
  readonly database: string;
  /** The pool behind `query()` and `execute()`. */
  readonly pool: Pool;
  /** A mysql2/promise pool on this database. The caller ends it. */
  createPool(config?: PoolOptions): Pool;
  /** A mysql2/promise connection on this database. The caller ends it. */
  createConnection(config?: ConnectionOptions): Promise<Connection>;
  query: Pool['query'];
  execute: Pool['execute'];
  /** End the internal pool and free the database. */
  close(): Promise<void>;
  [Symbol.asyncDispose](): Promise<void>;
}

/** Free a named in-memory database. Returns whether it existed. */
export function dropDatabase(name: string): boolean;
