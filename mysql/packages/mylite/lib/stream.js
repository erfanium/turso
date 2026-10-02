'use strict';

const { Duplex } = require('node:stream');
const { native } = require('./native');

/**
 * The transport handed to mysql2 in place of a socket. mysql2 only needs a
 * duplex stream, so the real driver — pooling, escaping, type casting — runs
 * unchanged against the in-process engine.
 */
class MyLiteStream extends Duplex {
  constructor() {
    super();
    this._channel = new (native().Channel)(
      (chunk) => this.push(chunk),
      () => {
        // The engine ended the connection (the client sent COM_QUIT).
        this.push(null);
        this._release();
      },
    );
    // The engine's callbacks do not hold the event loop open, so the stream
    // does, the way an open socket would: a process with a live connection
    // stays up until the connection is ended.
    this._keepAlive = setInterval(() => {}, 2 ** 30);
  }

  _read() {
    // Data is pushed as the engine produces it.
  }

  _write(chunk, _encoding, callback) {
    this._channel.write(chunk);
    callback();
  }

  _final(callback) {
    this._release();
    callback();
  }

  _destroy(error, callback) {
    this._release();
    callback(error);
  }

  _release() {
    if (this._released) return;
    this._released = true;
    clearInterval(this._keepAlive);
    this._channel.close();
  }

  /** Let the process exit while this connection is still open. */
  unref() {
    this._keepAlive.unref();
    return this;
  }

  ref() {
    this._keepAlive.ref();
    return this;
  }

  // Socket-only knobs mysql2 may call.
  setKeepAlive() {
    return this;
  }

  setNoDelay() {
    return this;
  }

  setTimeout() {
    return this;
  }
}

function createStream() {
  const stream = new MyLiteStream();
  if (process.env.MYLITE_UNREF) stream.unref();
  return stream;
}

module.exports = { createStream };
