'use strict';

const mysql = require('mysql2/promise');
const { wrap } = require('./lib/wrap');

/**
 * Drop-in for `mysql2/promise`: the same exports, with every pool and
 * connection running against an in-memory database inside this process.
 * Also reachable as `@erfanium/mylite/promise`, mirroring mysql2's layout.
 */
module.exports = wrap(mysql);
