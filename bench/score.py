#!/usr/bin/env python3
"""Answer scoring + stream.jsonl parsing, usable as a library.
Rules (same as our reports): for answer_only tasks only the text after the LAST 'ANSWER:' is matched; every regex in `must`
has to match, none in `must_not` may match; edit tasks additionally need check.json ok. Timeouts are NOT counted as wrong
by themselves: a timed-out run is scored on whatever answer/repo state it left.
Tasks with must_with_comments / must_skip_comments (N5, N8) are scored under both conventions, reported separately."""
import json, pathlib, re, os, collections

HERE = pathlib.Path(__file__).resolve().parent
TASKS = {t['id']: t for t in json.loads(pathlib.Path(os.environ.get('TASKS_FILE', HERE / 'tasks' / 'tasks.json')).read_text())}

def segment(task, text):
    if not task.get('answer_only'):
        return text
    i = text.rfind('ANSWER:')
    return text[i:] if i >= 0 else ''

def match(task, text, must_key='must'):
    seg = segment(task, text)
    if task.get('answer_only') and not seg:
        return False
    must = task.get(must_key, [])
    return all(re.search(p, seg) for p in must) and not any(re.search(p, seg) for p in task.get('must_not', []))

def parse_run(d):
    d = pathlib.Path(d)
    r = dict(turns=0, cost=0.0, duration_s=None, tools=collections.Counter(), astro=0, search_code=0, aggregate=0,
             skip_comments=0, web=0, text='')
    for line in (d / 'stream.jsonl').read_text(errors='replace').splitlines():
        try: ev = json.loads(line)
        except Exception: continue
        if ev.get('type') == 'assistant':
            r['turns'] += 1
            for c in (ev.get('message') or {}).get('content') or []:
                if c.get('type') == 'tool_use':
                    n = c['name']; r['tools'][n] += 1
                    if n.startswith('mcp__astrolabe'): r['astro'] += 1
                    if n in ('WebSearch', 'WebFetch'): r['web'] += 1
                    if n == 'mcp__astrolabe__search_code':
                        r['search_code'] += 1
                        inp = c.get('input', {})
                        if inp.get('aggregate') or inp.get('count_only'): r['aggregate'] += 1
                        if inp.get('skip_comments'): r['skip_comments'] += 1
                elif c.get('type') == 'text':
                    r['text'] += c['text']
        if ev.get('type') == 'result':
            r['cost'] = ev.get('total_cost_usd') or 0.0
            if ev.get('duration_ms') is not None: r['duration_s'] = ev['duration_ms'] / 1000
            if ev.get('result'): r['text'] = (ev['result'] or '') + '\n' + r['text']
    wall = d / 'wall'
    if r['duration_s'] is None and wall.exists(): r['duration_s'] = float(wall.read_text() or 0)
    r['exit'] = (d / 'exit').read_text().strip() if (d / 'exit').exists() else None
    r['timeout'] = r['exit'] == '124'
    return r

def score_run(d, task_id):
    t = TASKS[task_id]; r = parse_run(d)
    chk = None
    cp = pathlib.Path(d) / 'check.json'
    if cp.exists() and cp.stat().st_size: chk = json.loads(cp.read_text())
    chk_ok = True if t['kind'] != 'edit' else bool(chk and chk.get('ok'))
    if 'must_with_comments' in t:
        r['ok_with_comments'] = match(t, r['text'], 'must_with_comments') and chk_ok
        r['ok_skip_comments'] = match(t, r['text'], 'must_skip_comments') and chk_ok
        r['ok'] = r['ok_with_comments']          # strict: the reference convention
        r['ok_either'] = r['ok_with_comments'] or r['ok_skip_comments']
    else:
        r['ok'] = match(t, r['text']) and chk_ok
        r['ok_either'] = r['ok']
    return r
