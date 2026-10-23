'use strict';

const fs = require('fs');

function isTTY() {
  return Boolean(process.stdin.isTTY && process.stdout.isTTY);
}

function readLine(question) {
  process.stdout.write(question);
  if (!process.stdin.isTTY) {
    return '';
  }
  const buf = Buffer.alloc(1024);
  let n = 0;
  try {
    n = fs.readSync(0, buf, 0, buf.length, null);
  } catch {
    return '';
  }
  return buf.toString('utf8', 0, n).replace(/\r?\n$/, '');
}

function confirm(question, { defaultYes = false } = {}) {
  const hint = defaultYes ? 'Y/n' : 'y/N';
  const answer = readLine(`${question} [${hint}] `).trim().toLowerCase();
  if (!answer) return defaultYes;
  return answer === 'y' || answer === 'yes';
}

/**
 * Multi-select from rows: [{ id, label, detected, evidence }]
 * Returns selected ids.
 */
function selectClients(rows, { preselectDetected = true } = {}) {
  if (!isTTY()) {
    return rows.filter((r) => r.detected).map((r) => r.id);
  }

  process.stdout.write('\nDetected / known agents:\n');
  rows.forEach((row, i) => {
    const mark = row.detected ? '*' : ' ';
    const status = row.detected
      ? `detected (${row.evidence.join(', ')})`
      : 'not detected';
    process.stdout.write(`  ${i + 1}. [${mark}] ${row.label} (${row.id}) — ${status}\n`);
  });
  process.stdout.write(
    '\nEnter numbers to configure (e.g. 1,2), "d" for all detected, "a" for all, or empty to cancel:\n> ',
  );

  const answer = readLine('').trim().toLowerCase();
  if (!answer) return [];
  if (answer === 'd') return rows.filter((r) => r.detected).map((r) => r.id);
  if (answer === 'a') return rows.map((r) => r.id);

  const selected = new Set();
  for (const part of answer.split(/[,\s]+/)) {
    if (!part) continue;
    const n = Number(part);
    if (!Number.isInteger(n) || n < 1 || n > rows.length) {
      throw new Error(`invalid selection: ${part}`);
    }
    selected.add(rows[n - 1].id);
  }
  return [...selected];
}

module.exports = { isTTY, readLine, confirm, selectClients };
