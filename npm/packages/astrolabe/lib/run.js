'use strict';

const { spawn } = require('child_process');

const FORWARDED = ['SIGINT', 'SIGTERM', 'SIGHUP'];

/**
 * Run the native binary with full stdio / exit-code / signal passthrough.
 *
 * MCP JSON-RPC is stdin/stdout; logs are stderr. Inheriting the three fds
 * (no pipes, no buffering, no rewriting) is required or the protocol breaks.
 *
 * `spawnSync` would inherit stdio and exit codes, but it blocks the event
 * loop, so an MCP client SIGTERM aimed at the shim PID would never reach the
 * Rust process. `spawn` plus forward/re-raise is the stdio-equivalent that
 * also transmits signals in both directions.
 */
function runBinary(binPath, argv) {
  const child = spawn(binPath, argv, {
    stdio: 'inherit',
    windowsHide: false,
  });

  const forward = (signal) => {
    if (!child.killed) {
      try {
        child.kill(signal);
      } catch {
        // Child may have already exited.
      }
    }
  };

  for (const signal of FORWARDED) {
    try {
      process.on(signal, forward);
    } catch {
      // Windows does not implement every POSIX signal.
    }
  }

  const detach = () => {
    for (const signal of FORWARDED) {
      process.removeListener(signal, forward);
    }
  };

  child.on('error', (error) => {
    detach();
    console.error(`[astrolabe] failed to execute ${binPath}: ${error.message}`);
    process.exit(1);
  });

  child.on('exit', (code, signal) => {
    detach();
    if (signal) {
      try {
        process.kill(process.pid, signal);
      } catch {
        process.exit(1);
      }
      process.exit(1);
      return;
    }
    process.exit(code == null ? 1 : code);
  });
}

module.exports = { runBinary };
