#!/usr/bin/env node
'use strict';

const { resolveBinary } = require('../lib/resolve');
const { runBinary } = require('../lib/run');

resolveBinary()
  .then((binPath) => {
    runBinary(binPath, process.argv.slice(2));
  })
  .catch((error) => {
    console.error(`[astrolabe] ${error.message || error}`);
    process.exit(1);
  });
