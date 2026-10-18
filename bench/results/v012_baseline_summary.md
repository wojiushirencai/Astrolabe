# v0.12.0 matrix (A / B / C) — summary of internal report REPORT_V012_BASELINE

15 tasks x 3 arms x 5 rounds = 225 runs, triplets started together (median start gap 1.8 s). A = no Astrolabe, B = v0.11.0 (22 tools), C = v0.12.0 (7 tools, official release archive). Scoring: timeouts are not counted as wrong by themselves (6 timeouts re-scored on their final answer/repo state: 2 became correct, 4 stayed wrong). Time = `duration_ms`. Model `gemini-3.8-flash-high`; this older matrix did not disable WebSearch and shared a writable Go build cache between concurrent runs (later rounds fixed both).

| Arm | Correct | Timeouts | Cost median / mean | Time median / mean |
|---|---|---|---|---|
| A | 73/75 | 1 | $1.27 / $1.47 | 274 s / 366 s |
| B | 73/75 | 0 | $1.15 / $1.63 | 309 s / 408 s |
| C | 70/75 | 5 | $1.32 / $1.71 | 319 s / 418 s |

## Paired differences (triplet bootstrap, 95% CI)

| Scope | Metric | C − A | C − B |
|---|---|---|---|
| All 75 | cost | +$0.244 [+0.035, +0.467] | +$0.080 [−0.164, +0.331] |
| All 75 | time | +53 s [−8, +119] | +10 s [−55, +79] |
| All 75 | correct rate | −4.0 pp [−10.7, +2.7] | −4.0 pp [−10.7, +1.3] |
| 40 control cells | cost | +$0.294 [+0.085, +0.540] | +$0.095 [−0.057, +0.266] |
| 40 control cells | time | +59 s [−8, +141] | −2 s [−64, +68] |

A follow-up analysis found 91% of the C − A cost gap ($16.6 of $18.3) came from four tasks (E3b, N8, H1, N4), chosen after seeing the data (so a hypothesis, not a finding). Without them: cost +$0.03 per run [−0.14, +0.21], time −17 s [−63, +27].
