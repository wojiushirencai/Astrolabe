#!/usr/bin/env python3
"""Paired A-vs-C report for runs made by run_pairs.sh.  usage: report.py [RUNS_DIR] [--boot 20000]
Pairs = same task and rep. Per pair differences C-A of cost, duration_ms-based time and turns, with a 95% bootstrap CI
over pairs (resampling pairs, seed fixed). Correct-answer counts use score.py (timeouts are not counted as wrong by themselves)."""
import pathlib, random, re, statistics as S, sys
sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
from score import score_run

def ci(xs, boot=20000, seed=1):
    rnd = random.Random(seed); n = len(xs)
    ms = sorted(S.mean(rnd.choices(xs, k=n)) for _ in range(boot))
    return S.mean(xs), ms[int(.025 * boot)], ms[int(.975 * boot)]

def main():
    args = [a for a in sys.argv[1:] if not a.startswith('--')]
    runs = pathlib.Path(args[0] if args else 'bench-work/runs')
    cells = {}
    for d in sorted(runs.iterdir()):
        m = re.match(r'^(\w+?)-([AC])-r(\d+)$', d.name)
        if m and (d / 'exit').exists() and (d / 'stream.jsonl').exists():
            cells[(m.group(1), m.group(2), int(m.group(3)))] = score_run(d, m.group(1))
    pairs = sorted({(t, r) for (t, a, r) in cells if (t, 'A', r) in cells and (t, 'C', r) in cells})
    print(f'{len(pairs)} complete pairs from {runs}\n')
    groups = [('ALL', pairs)] + [(t, [p for p in pairs if p[0] == t]) for t in sorted({p[0] for p in pairs})]
    print(f"{'scope':6}{'n':>3}  {'ok A/C (strict)':>16} {'ok A/C (either)':>16}  {'cost A':>7}{'cost C':>8}  " + "C-A cost [95% CI]".ljust(30) + "C-A time s [95% CI]".ljust(30) + "C-A turns")
    for name, ps in groups:
        if not ps: continue
        f = lambda arm, k: [cells[(t, arm, r)][k] for t, r in ps]
        dc = [c - a for a, c in zip(f('A', 'cost'), f('C', 'cost'))]
        dt = [c - a for a, c in zip(f('A', 'duration_s'), f('C', 'duration_s'))]
        dn = [c - a for a, c in zip(f('A', 'turns'), f('C', 'turns'))]
        okA = sum(f('A', 'ok')); okC = sum(f('C', 'ok')); eA = sum(f('A', 'ok_either')); eC = sum(f('C', 'ok_either'))
        mc, lc, hc = ci(dc); mt, lt, ht = ci(dt)
        print(f"{name:6}{len(ps):>3}  {f'{okA}/{okC}':>16} {f'{eA}/{eC}':>16}  {S.mean(f('A','cost')):>7.2f}{S.mean(f('C','cost')):>8.2f}  "
              + "".ljust(0) + f"{mc:+.2f} [{lc:+.2f},{hc:+.2f}]".ljust(30) + f"{mt:+.0f} [{lt:+.0f},{ht:+.0f}]".ljust(30) + f"{S.mean(dn):+.1f}")
    adopt = [cells[(t, 'C', r)]['astro'] > 0 for t, r in pairs]
    print(f"\nastrolabe adoption in arm C: {sum(adopt)}/{len(adopt)} runs; timeouts A/C: "
          f"{sum(cells[(t,'A',r)]['timeout'] for t,r in pairs)}/{sum(cells[(t,'C',r)]['timeout'] for t,r in pairs)}; "
          f"web tool calls (must be 0): {sum(c['web'] for c in cells.values())}")
    print('Note: pairs ran concurrently only if started via run_pairs.sh; per-turn latency drifts with time of day, so '
          'compare time only across simultaneous pairs.')

if __name__ == '__main__':
    main()
