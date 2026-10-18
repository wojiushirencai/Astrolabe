#!/usr/bin/env python3
"""Post-run repo check for edit tasks (run outside the sandbox on the run's repo copy).
usage: checks.py TASK_ID REPO_DIR  -> prints JSON {"ok": bool|null, "detail": ...}
Edit tasks carry "verify": [command, needle] in tasks.json: the command must exit 0 and the needle must occur in the tree."""
import json, os, pathlib, subprocess, sys

HERE = pathlib.Path(__file__).resolve().parent
TASKS = {t['id']: t for t in json.loads(pathlib.Path(os.environ.get('TASKS_FILE', HERE / 'tasks' / 'tasks.json')).read_text())}

def main():
    t = TASKS[sys.argv[1]]; repo = sys.argv[2]
    if t['kind'] != 'edit' or 'verify' not in t:
        print(json.dumps({'ok': None})); return
    cmd, needle = t['verify']
    p = subprocess.run(cmd, shell=True, cwd=repo, capture_output=True, text=True, timeout=900)
    r = subprocess.run(['grep', '-rqF', '--exclude-dir=.git', needle, '.'], cwd=repo)
    print(json.dumps({'ok': p.returncode == 0 and r.returncode == 0,
                      'detail': {'rc': p.returncode, 'needle_found': r.returncode == 0, 'tail': (p.stdout + p.stderr)[-300:]}}))

if __name__ == '__main__':
    main()
