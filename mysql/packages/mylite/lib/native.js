'use strict';

const fs = require('node:fs');
const path = require('node:path');

let addon;

/**
 * The native engine: an in-memory database that speaks the MySQL wire
 * protocol inside this process. Loaded on first use, so importing the package
 * on an unsupported platform only fails once a connection is opened.
 */
function native() {
  if (addon) return addon;

  const target = `${process.platform}-${process.arch}`;
  const file =
    process.env.MYLITE_NATIVE_PATH ||
    path.join(__dirname, '..', 'prebuilds', target, 'mylite.node');

  if (!fs.existsSync(file)) {
    throw new Error(
      `@erfanium/mylite has no native binary for ${target} (looked at ${file}). ` +
        'Build one with `npm run build:native`, or point MYLITE_NATIVE_PATH at one.',
    );
  }

  try {
    addon = require(file);
  } catch (cause) {
    throw new Error(
      `@erfanium/mylite could not load its native binary ${file}: ${cause.message}`,
      { cause },
    );
  }
  return addon;
}

module.exports = { native };
