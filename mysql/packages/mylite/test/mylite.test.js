'use strict';

const assert = require('node:assert/strict');
const { test } = require('node:test');

const mysql = require('..');

test('MyLite: query, types and isolation between instances', async () => {
  const a = new mysql.MyLite();
  const b = new mysql.MyLite();

  await a.query(
    'create table t (id int auto_increment, name varchar(50) not null, amount decimal(10,2), at datetime, tags json, raw binary(4), primary key (id))',
  );
  const [insert] = await a.query(
    'insert into t (name, amount, at, tags, raw) values (?, ?, ?, ?, ?)',
    ['سلام', '1.5', new Date('2026-01-02T03:04:05Z'), JSON.stringify(['x']), Buffer.from('abcd')],
  );
  assert.equal(insert.insertId, 1);
  assert.equal(insert.affectedRows, 1);

  const [[row], fields] = await a.query('select * from t where name = ?', ['سلام']);
  assert.equal(row.amount, '1.50');
  assert.deepEqual(row.tags, ['x']);
  assert.ok(row.at instanceof Date);
  assert.ok(Buffer.isBuffer(row.raw));
  assert.deepEqual(fields.map((f) => f.name), ['id', 'name', 'amount', 'at', 'tags', 'raw']);

  // `b` is a different database: the table does not exist there.
  await assert.rejects(b.query('select * from t'), { code: 'ER_NO_SUCH_TABLE' });

  await a.close();
  await b.close();
});

test('mysql2/promise drop-in: pool, transaction, execute, errors', async () => {
  const pool = mysql.createPool({ host: 'ignored', user: 'x', database: 'dropin', connectionLimit: 4 });
  await pool.query('create table if not exists acct (id int primary key, balance bigint not null)');
  await pool.query('insert into acct values (1, 100) on duplicate key update balance = 100');

  const conn = await pool.getConnection();
  await conn.beginTransaction();
  await conn.query('update acct set balance = balance - 30 where id = ?', [1]);
  await conn.rollback();
  conn.release();

  const [[{ balance }]] = await pool.execute('select balance from acct where id = ?', [1]);
  assert.equal(balance, 100);

  await assert.rejects(pool.query('insert into acct values (1, 5)'), { code: 'ER_DUP_ENTRY' });

  const [[{ total }]] = await pool.query('select sum(balance) as total from acct');
  assert.equal(total, '100');

  await pool.end();
  assert.equal(mysql.dropDatabase('dropin'), true);
});

test('same-name databases are shared, and both entry points are one module', async () => {
  const one = await mysql.createConnection({ database: 'shared' });
  const two = await mysql.createConnection({ database: 'shared' });

  await one.query('create table s (n int)');
  await one.query('insert into s values (7)');
  const [rows] = await two.query('select n from s');
  assert.deepEqual(rows.map((r) => r.n), [7]);

  await one.end();
  await two.end();
  mysql.dropDatabase('shared');
  assert.equal(require('@erfanium/mylite/promise'), require('@erfanium/mylite'));
});

test('exports mirror mysql2', () => {
  const real = require('mysql2/promise');
  for (const key of Object.keys(real)) assert.ok(key in mysql, `missing export ${key}`);
  assert.equal(mysql.PromisePool, real.PromisePool);
  assert.equal(typeof mysql.format, 'function');
});
