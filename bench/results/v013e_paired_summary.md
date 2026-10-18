# v0.13.0 vs no Astrolabe — simultaneous pairs (summary of internal report REPORT_V013E)

Run on 2026-10-18. Binary: Astrolabe built from commit `feb77d2` (shipped in v0.13.0), 7 tools. Tasks N5, N8, N4, E3b, 10 pairs each (40 pairs). In each pair arm A (no Astrolabe) and arm C (Astrolabe) started at the same barrier; at most 3 pairs (6 runs) ran concurrently; queue order randomized with a fixed seed. Every cell has exactly one valid datum.

Conditions: per-run Go build cache, WebSearch/WebFetch disabled, model `gemini-3.8-flash-high` via Claude Code and an Anthropic-compatible proxy (address not included), bubblewrap sandbox, fresh `CARGO_TARGET_DIR` for the post-run check, 1500 s timeout (a timed-out run is scored on the answer/repo state it left, not automatically as wrong), time = Claude Code `duration_ms`. N8 scored under both comment conventions; N5 scored strict and lenient. Three short machine freezes (76, 64, 54 s) overlapped 5 pairs; the sensitivity table drops them.

## All four tasks (mean per run unless noted)

| Arm | Correct (strict) | Correct (lenient N5) | Timeouts | Cost mean / median | Time mean / median | Turns | Astrolabe adoption |
|---|---|---|---|---|---|---|---|
| C (Astrolabe) | 31/40 | 40/40 | 0 | $2.35 / $2.15 | 492 s / 378 s | 40.0 | 37/40 |
| A (none) | 38/40 | 38/40 | 3 | $3.00 / $2.59 | 600 s / 462 s | 50.3 | 0/40 |

## Paired differences C − A (95% bootstrap CI, resampling pairs)

| Scope | Pairs | Cost | Time (s) | Turns | Correct, strict (pp) | Correct, lenient (pp) |
|---|---|---|---|---|---|---|
| All four | 40 | −0.65 [−0.98, −0.31] | −108 [−178, −37] | −10.3 [−14.4, −6.2] | −17.5 [−32.5, −2.5] | +5.0 [0.0, +12.5] |
| N5 | 10 | −0.64 [−1.22, −0.02] | −115 [−220, +2] | −13.2 [−21.2, −4.7] | −90.0 [−100, −70] | 0 |
| N8 | 10 | −0.91 [−1.48, −0.31] | −110 [−196, −16] | −14.2 [−21.0, −7.1] | 0 | 0 |
| N4 | 10 | −0.51 [−1.20, +0.21] | −164 [−307, −10] | −6.3 [−13.9, +2.1] | +20.0 [0, +50] | +20.0 [0, +50] |
| E3b | 10 | −0.52 [−1.22, +0.18] | −44 [−240, +146] | −7.7 [−16.5, +1.6] | 0 | 0 |
| All four, freezes dropped | 35 | −0.70 [−1.04, −0.36] | −137 [−208, −65] | −11.3 [−15.4, −7.0] | −20.0 [−37.1, −2.9] | +5.7 [0, +14.3] |

Notes
- Strict N5 correctness is 1/10 for arm C because the reference answer counts 2 call sites inside comments (model=15/44, total=321); with `skip_comments=true` the tool returns 15/43 and 319. Lenient scoring accepts both.
- Arm C used `aggregate` in 20/40 runs (45 calls; `top` 34, `skip_comments` 27). N4 and E3b used none, so their differences are not attributable to the new options.
- Per-turn latency difference C − A: +0.7 s [−0.5, +2.0] (no significant difference); the time saving comes from fewer turns.
- N4 and E3b ran on private repositories and cannot be reproduced; N5 and N8 ran on QuantumNous/new-api (see `corpora.json` for the snapshot caveat).
