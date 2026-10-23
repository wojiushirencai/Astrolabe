'use strict';

/**
 * Minimal JSONC helpers for merge-only edits that preserve surrounding comments.
 * Not a full JSONC parser: only brace-aware object-property upsert.
 */

function stripJsonc(raw) {
  let out = '';
  let i = 0;
  let inStr = false;
  let esc = false;
  while (i < raw.length) {
    const c = raw[i];
    if (inStr) {
      out += c;
      if (esc) esc = false;
      else if (c === '\\') esc = true;
      else if (c === '"') inStr = false;
      i += 1;
      continue;
    }
    if (c === '"') {
      inStr = true;
      out += c;
      i += 1;
      continue;
    }
    if (c === '/' && raw[i + 1] === '/') {
      i += 2;
      while (i < raw.length && raw[i] !== '\n') i += 1;
      continue;
    }
    if (c === '/' && raw[i + 1] === '*') {
      i += 2;
      while (i + 1 < raw.length && !(raw[i] === '*' && raw[i + 1] === '/')) i += 1;
      i += 2;
      continue;
    }
    out += c;
    i += 1;
  }
  return out;
}

function parseJsonc(raw) {
  return JSON.parse(stripJsonc(raw.replace(/^\uFEFF/, '')));
}

function findMatchingBrace(text, openIdx) {
  if (openIdx < 0 || text[openIdx] !== '{') return -1;
  let depth = 0;
  let inStr = false;
  let esc = false;
  for (let i = openIdx; i < text.length; i += 1) {
    const c = text[i];
    if (inStr) {
      if (esc) {
        esc = false;
        continue;
      }
      if (c === '\\') {
        esc = true;
        continue;
      }
      if (c === '"') inStr = false;
      continue;
    }
    if (c === '"') {
      inStr = true;
      continue;
    }
    if (c === '{') depth += 1;
    else if (c === '}') {
      depth -= 1;
      if (depth === 0) return i;
    }
  }
  return -1;
}

/**
 * Find the `{` that opens the object at a dotted path of string keys from root.
 * Returns index of `{` or -1.
 */
function findObjectOpen(text, pathKeys) {
  const stripped = stripJsonc(text);
  // Walk on stripped text to find offsets, then map... actually comments shift
  // offsets. Walk the original with a comment-aware scanner instead.
  let pos = text.search(/\{/);
  if (pos < 0) return -1;
  // Root object open:
  // skip BOM/whitespace/comments to first {
  pos = indexOfFirstBrace(text);
  if (pos < 0) return -1;

  for (const key of pathKeys) {
    const prop = findPropertyValueStart(text, pos, key);
    if (prop < 0) return -1;
    // skip whitespace to value
    let i = prop;
    while (i < text.length && /\s/.test(text[i])) i += 1;
    if (text[i] !== '{') return -1;
    pos = i;
  }
  return pos;
}

function indexOfFirstBrace(text) {
  let i = 0;
  let inStr = false;
  let esc = false;
  while (i < text.length) {
    const c = text[i];
    if (inStr) {
      if (esc) esc = false;
      else if (c === '\\') esc = true;
      else if (c === '"') inStr = false;
      i += 1;
      continue;
    }
    if (c === '"') {
      inStr = true;
      i += 1;
      continue;
    }
    if (c === '/' && text[i + 1] === '/') {
      i += 2;
      while (i < text.length && text[i] !== '\n') i += 1;
      continue;
    }
    if (c === '/' && text[i + 1] === '*') {
      i += 2;
      while (i + 1 < text.length && !(text[i] === '*' && text[i + 1] === '/')) i += 1;
      i += 2;
      continue;
    }
    if (c === '{') return i;
    i += 1;
  }
  return -1;
}

/**
 * Inside object starting at openIdx, find the start index of the value for "key".
 */
function findPropertyValueStart(text, openIdx, key) {
  const end = findMatchingBrace(text, openIdx);
  if (end < 0) return -1;
  const needle = `"${key}"`;
  let i = openIdx + 1;
  let inStr = false;
  let esc = false;
  let depth = 0; // relative depth inside arrays/objects under this object
  while (i < end) {
    const c = text[i];
    if (inStr) {
      if (esc) esc = false;
      else if (c === '\\') esc = true;
      else if (c === '"') inStr = false;
      i += 1;
      continue;
    }
    if (c === '"') {
      if (depth === 0 && text.startsWith(needle, i)) {
        let j = i + needle.length;
        while (j < end && /\s/.test(text[j])) j += 1;
        if (text[j] === ':') {
          j += 1;
          while (j < end && /\s/.test(text[j])) j += 1;
          return j;
        }
      }
      inStr = true;
      i += 1;
      continue;
    }
    if (c === '/' && text[i + 1] === '/') {
      i += 2;
      while (i < end && text[i] !== '\n') i += 1;
      continue;
    }
    if (c === '/' && text[i + 1] === '*') {
      i += 2;
      while (i + 1 < end && !(text[i] === '*' && text[i + 1] === '/')) i += 1;
      i += 2;
      continue;
    }
    if (c === '{' || c === '[') depth += 1;
    else if (c === '}' || c === ']') depth -= 1;
    i += 1;
  }
  return -1;
}

function skipJsonValue(text, start) {
  let i = start;
  while (i < text.length && /\s/.test(text[i])) i += 1;
  if (i >= text.length) return start;
  const c = text[i];
  if (c === '{') {
    const end = findMatchingBrace(text, i);
    return end < 0 ? start : end + 1;
  }
  if (c === '[') {
    let depth = 0;
    let inStr = false;
    let esc = false;
    for (let j = i; j < text.length; j += 1) {
      const ch = text[j];
      if (inStr) {
        if (esc) esc = false;
        else if (ch === '\\') esc = true;
        else if (ch === '"') inStr = false;
        continue;
      }
      if (ch === '"') {
        inStr = true;
        continue;
      }
      if (ch === '[') depth += 1;
      else if (ch === ']') {
        depth -= 1;
        if (depth === 0) return j + 1;
      }
    }
    return start;
  }
  if (c === '"') {
    let esc = false;
    for (let j = i + 1; j < text.length; j += 1) {
      const ch = text[j];
      if (esc) {
        esc = false;
        continue;
      }
      if (ch === '\\') {
        esc = true;
        continue;
      }
      if (ch === '"') return j + 1;
    }
    return start;
  }
  // number / literal
  while (i < text.length && /[^\s,\}\]]/.test(text[i])) i += 1;
  return i;
}

/**
 * Upsert key: valueObject into the object at parentPath inside raw JSONC.
 * valueObject is a plain JS object; serialized with JSON.stringify (2-space).
 * Returns { text, replaced, createdParent }.
 */
function upsertJsoncProperty(raw, parentPath, key, valueObject, { indent = 2 } = {}) {
  let text = raw.replace(/^\uFEFF/, '');
  if (!text.trim()) {
    text = '{\n}\n';
  }

  // Ensure parent path exists (create missing parents as empty objects via parse+rewrite fallback)
  let parentOpen = findObjectOpen(text, parentPath);
  let createdParent = false;
  if (parentOpen < 0) {
    // Fall back: parse, ensure path, re-serialize whole doc (comments lost only when parent missing)
    let doc;
    try {
      doc = parseJsonc(text);
    } catch {
      doc = {};
    }
    if (doc === null || typeof doc !== 'object' || Array.isArray(doc)) doc = {};
    let cursor = doc;
    for (const p of parentPath) {
      if (!cursor[p] || typeof cursor[p] !== 'object' || Array.isArray(cursor[p])) {
        cursor[p] = {};
        createdParent = true;
      }
      cursor = cursor[p];
    }
    cursor[key] = valueObject;
    return {
      text: `${JSON.stringify(doc, null, indent)}\n`,
      replaced: false,
      createdParent: true,
      commentsPreserved: false,
    };
  }

  const valueStart = findPropertyValueStart(text, parentOpen, key);
  const pretty = JSON.stringify(valueObject, null, indent);
  // Reindent relative to parent: detect parent indent
  const lineStart = text.lastIndexOf('\n', parentOpen) + 1;
  const parentIndent = text.slice(lineStart, parentOpen).match(/^\s*/)[0];
  const childIndent = parentIndent + ' '.repeat(indent);
  const indentedValue = pretty
    .split('\n')
    .map((line, idx) => (idx === 0 ? line : childIndent + line))
    .join('\n');

  if (valueStart >= 0) {
    const valueEnd = skipJsonValue(text, valueStart);
    const next = `${text.slice(0, valueStart)}${indentedValue}${text.slice(valueEnd)}`;
    return { text: next, replaced: true, createdParent, commentsPreserved: true };
  }

  // Insert before closing brace of parent
  const parentEnd = findMatchingBrace(text, parentOpen);
  if (parentEnd < 0) {
    throw new Error('unterminated JSON object while merging');
  }
  // Look at content between open and close for trailing comma needs
  const inner = text.slice(parentOpen + 1, parentEnd);
  const innerTrim = inner.replace(/\/\*[\s\S]*?\*\//g, '').replace(/\/\/.*$/gm, '').trim();
  const needsComma = innerTrim.length > 0 && !innerTrim.endsWith(',');
  const insertion = `${needsComma ? ',' : ''}\n${childIndent}"${key}": ${indentedValue}\n${parentIndent}`;
  // If inner ends with whitespace before }, place insertion cleanly
  let insertAt = parentEnd;
  // trim trailing whitespace inside parent before insert
  while (insertAt > parentOpen + 1 && /[ \t]/.test(text[insertAt - 1])) insertAt -= 1;
  if (text[insertAt - 1] === '\n') {
    // keep
  }
  const next = `${text.slice(0, insertAt)}${insertion}${text.slice(parentEnd)}`;
  return { text: next, replaced: false, createdParent, commentsPreserved: true };
}

module.exports = {
  stripJsonc,
  parseJsonc,
  findMatchingBrace,
  findObjectOpen,
  upsertJsoncProperty,
};
