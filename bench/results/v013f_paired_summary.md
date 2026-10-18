# v0.13.0 vs no Astrolabe — 15 tasks, 95 simultaneous pairs (summary of internal report REPORT_V013F)

Run on 2026-10-18. Binary: Astrolabe built from commit `feb77d2` (shipped in v0.13.0), 7 tools. This file covers the **11 tasks added in this round** (H1 H2 H3 H6 H7 E1 E2 N1 N3 N6 N7, 5 pairs each = 55 pairs, 110 runs, cost $120.63) and the **merged 15-task view** with the earlier N5, N8, N4, E3b run (10 pairs each, see `v013e_paired_summary.md`; 95 pairs in total). In every pair arm A (no Astrolabe) and arm C (Astrolabe) started at the same barrier; at most 3 pairs (6 runs) ran concurrently; queue order randomized with a fixed seed. Every cell has exactly one valid datum.

Conditions: per-run Go build cache, WebSearch/WebFetch disabled, model `gemini-3.8-flash-high` via Claude Code and an Anthropic-compatible proxy (address not included), bubblewrap sandbox, 1500 s timeout (a timed-out run is scored on the answer/repo state it left, not automatically as wrong), time = Claude Code `duration_ms`. One short machine freeze overlapped 3 of the 55 new pairs; sensitivity rows drop frozen pairs.

**Limits, read first:** 15 tasks that we designed ourselves; one model; 5 pairs per task for 11 tasks and 10 for 4; 6 of the 15 tasks use private material and cannot be reproduced (see `bench/README.md`). Per-task intervals are wide. The result is an average over these tasks, not evidence of a benefit on arbitrary work.

## Scoring: N5 primary is lenient, strict is reported alongside

N5's reference answer counts 2 call sites of `common.SysError(` that sit in comments (model=15/44, total=321). With `search_code skip_comments=true` Astrolabe returns 15/43 and 319, a defensible reading of "calls to", which a strict check marks wrong. From this round the **primary** N5 verdict accepts either convention (`ok_either`); the **strict** verdict (reference convention only) is still computed and reported next to it. N8 stays strict-primary (both conventions gave the same result there). All other tasks have a single convention.

## Whole set: 15 tasks, 95 pairs (mean per run)

| Arm | Correct (primary) | Correct (strict) | Timeouts | Cost mean / median | Time mean / median | Tool calls | Turns | Per-turn time mean / median | Astrolabe adoption |
|---|---|---|---|---|---|---|---|---|---|
| C (Astrolabe) | 95/95 | 86/95 | 0 | $1.51 / $1.10 | 357 s / 295 s | 27.0 | 28.1 | 12.5 s / 12.2 s | 92/95 |
| A (none) | 93/95 | 93/95 | 3 | $2.01 / $1.76 | 440 s / 358 s | 33.9 | 34.9 | 12.0 s / 11.6 s | 0/95 |

The strict gap (86 vs 93) comes entirely from N5 (C 1/10 strict, 10/10 primary). A's two strict misses are both in N4; 3 A runs timed out in total (timeouts are scored on the state they left).

## Paired differences C − A (95% bootstrap CI, resampling pairs)

| Scope | Pairs | Cost ($) | Time (s) | Turns | Per-turn time (s) | Correct, strict (pp) | Correct, primary (pp) |
|---|---|---|---|---|---|---|---|
| 15 tasks | 95 | −0.49 [−0.73, −0.28] | −83 [−129, −41] | −6.8 [−9.2, −4.3] | +0.6 [−0.3, +1.4] | −7.4 [−14.7, −1.1] | +2.1 [0.0, +5.3] |
| 15 tasks, freezes dropped | 87 | −0.45 [−0.66, −0.25] | −87 [−134, −44] | −6.5 [−9.0, −4.1] | +0.3 [−0.5, +1.2] | −8.0 [−14.9, −1.1] | +2.3 [0.0, +5.7] |
| Only the 4 earlier tasks (N5 N8 N4 E3b) | 40 | −0.65 [−0.97, −0.32] | −108 [−179, −36] | −10.3 [−14.5, −6.2] | +0.7 [−0.5, +1.9] | −17.5 [−32.5, −2.5] | +5.0 [0.0, +12.5] |
| Only the 11 new tasks | 55 | −0.38 [−0.71, −0.13] | −65 [−125, −14] | −4.1 [−7.1, −1.6] | +0.4 [−0.6, +1.7] | 0 | 0 |
| 11 new, controls (H1 H2 H3 H6 H7 E1 E2) | 35 | −0.10 [−0.21, +0.01] | −17 [−50, +18] | −1.5 [−3.1, 0.0] | +0.7 [−0.7, +2.5] | 0 | 0 |
| 11 new, discovery/new (N1 N3 N6 N7) | 20 | −0.88 [−1.68, −0.21] | −149 [−293, −26] | −8.8 [−15.8, −2.6] | 0.0 [−1.4, +1.2] | 0 | 0 |

Equal weight per task (mean difference per task, then resample the 15 tasks): cost −0.45 [−0.90, −0.15], time −76 s [−153, −21], turns −5.8 [−9.9, −2.6]. C is cheaper than A on average in 11 of 15 tasks and faster in 11 of 15.

Per-turn latency shows no significant difference; the time saving comes from fewer turns.

## Per task (C vs A; strict correct counts shown, pairs = n)

| Task | Pairs | Correct strict C/A | Cost C / A | Time C / A | Turns C / A | C adoption |
|---|---|---|---|---|---|---|
| H1 | 5 | 5 / 5 | $1.29 / $1.60 | 371 / 411 s | 23.6 / 25.0 | 5/5 |
| H2 | 5 | 5 / 5 | $0.61 / $0.58 | 157 / 181 s | 13.4 / 14.2 | 5/5 |
| H3 | 5 | 5 / 5 | $0.72 / $0.70 | 193 / 167 s | 13.0 / 13.4 | 5/5 |
| H6 | 5 | 5 / 5 | $0.26 / $0.28 | 56 / 66 s | 6.2 / 6.4 | 5/5 |
| H7 | 5 | 5 / 5 | $0.24 / $0.21 | 47 / 46 s | 4.2 / 6.0 | 5/5 |
| E1 | 5 | 5 / 5 | $0.94 / $1.32 | 298 / 359 s | 23.2 / 27.0 | 5/5 |
| E2 | 5 | 5 / 5 | $0.42 / $0.46 | 118 / 126 s | 12.4 / 14.6 | 5/5 |
| N1 | 5 | 5 / 5 | $1.36 / $4.46 | 402 / 922 s | 26.8 / 55.4 | 5/5 |
| N3 | 5 | 5 / 5 | $1.76 / $2.24 | 463 / 600 s | 32.8 / 37.6 | 5/5 |
| N6 | 5 | 5 / 5 | $0.89 / $0.75 | 227 / 202 s | 17.2 / 17.0 | 5/5 |
| N7 | 5 | 5 / 5 | $1.47 / $1.55 | 509 / 473 s | 41.4 / 43.2 | 5/5 |
| N5 | 10 | 1 / 10 (primary 10 / 10) | $1.26 / $1.90 | 223 / 338 s | 21.6 / 34.8 | 10/10 |
| N8 | 10 | 10 / 10 | $1.41 / $2.32 | 302 / 413 s | 21.7 / 35.9 | 10/10 |
| N4 | 10 | 10 / 8 | $4.08 / $4.59 | 900 / 1063 s | 68.2 / 74.5 | 7/10 |
| E3b | 10 | 10 / 10 | $2.66 / $3.19 | 543 / 587 s | 48.4 / 56.1 | 10/10 |

Per-task paired cost differences for the 11 new tasks (C − A, 5 pairs each): N1 −3.10 [−4.88, −1.32]; E1 −0.38 [−0.72, +0.04]; N3 −0.48 [−1.06, +0.26]; H1 −0.31 [−0.63, +0.15]; N7 −0.09 [−0.46, +0.27]; E2 −0.04 [−0.09, +0.01]; H6 −0.01 [−0.04, +0.01]; H2 +0.02 [−0.17, +0.16]; H3 +0.02 [−0.30, +0.25]; H7 +0.02 [−0.06, +0.10]; N6 +0.13 [−0.10, +0.34]. Only N1 excludes zero among the new tasks; the other ten intervals include zero.

## What this shows and does not show

- Averaged over these 15 tasks, with the same model and simultaneous starts, C was cheaper and faster than A and answered correctly at least as often under the primary scoring.
- The gain is concentrated in aggregation/discovery-type tasks (N1, N5, N8 and, less clearly, N3, E1, N4). Simple locating or small edit tasks (H2, H3, H6, H7, E2, N6, N7) are about even.
- It does not show a general benefit: small samples, our own tasks, a single model, and a task mix chosen before seeing results but not drawn from a real population of coding work.
- Astrolabe was used in 92/95 C runs; A never had it, so adoption is part of the measured effect.
