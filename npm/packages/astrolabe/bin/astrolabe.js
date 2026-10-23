#!/usr/bin/env node
'use strict';

const argv = process.argv.slice(2);

// `astrolabe setup` is implemented in Node (detect agents + merge MCP configs).
// Intercept before resolving / spawning the native binary.
if (argv[0] === 'setup') {
  const { run } = require('../lib/setup');
  try {
    process.exitCode = run(argv.slice(1));
  } catch (error) {
    console.error(`[astrolabe setup] ${error.message || error}`);
    process.exitCode = 1;
  }
} else {
  const { resolveBinary } = require('../lib/resolve');
  const { runBinary } = require('../lib/run');

  resolveBinary()
    .then((binPath) => {
      runBinary(binPath, argv);
    })
    .catch((error) => {
      console.error(`[astrolabe] ${error.message || error}`);
      process.exit(1);
    });
}
