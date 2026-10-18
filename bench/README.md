# Astrolabe AI benchmark harness (paired A/B)

This directory is the harness behind the numbers in the project README. It answers one question: *for the same task and model, does connecting Astrolabe change cost, time, turns or correctness?* Arm **A** runs Claude Code without MCP; arm **C** runs it with Astrolabe. Pairs start together so slow-moving API latency hits both arms equally.

> **Real money.** `run_pairs.sh` calls the model API. Our 40-pair run (4 tasks x 10) cost about $214 in total (summary in `results/`). Start with `--pairs 2` and set a spend limit on your provider.

## What is here

| File | Purpose |
|---|---|
| `tasks/tasks.json` | 12 tasks on **public** repos: cobra (H6, E2, N1, N6), express (H7), new-api (H1, H2, H3, N2, N3, N5, N8) |
| `corpora.json`, `fetch_corpora.sh` | Where to get the repos; reference tree shas to compare your checkout against |
| `run_one.sh` | One cell: copies the repo, runs `claude -p` in a bubblewrap sandbox (arm A or C), saves the stream, exit code, wall time, post-run check |
| `run_pairs.sh` | Starts A and C of each (task, rep) at the same barrier; `--parallel` pairs at once |
| `score.py`, `checks.py` | Answer scoring (regexes on the text after the last `ANSWER:`) and edit-task repo checks |
| `report.py` | Paired differences C − A with bootstrap CIs, adoption, timeouts |
| `monitor_load.sh` | Records host load and detects machine freezes (> 20 s gaps) during a run |
| `results/` | Number-only summaries of the two runs quoted in the README |

## Run it

```bash
# needs: claude (Claude Code CLI) logged in or with ANTHROPIC_* env set, jq, python3, git, bubblewrap (recommended), Go for Go tasks
export ASTROLABE_BIN=/path/to/astrolabe          # arm C binary (v0.13.0 release works)
export MODEL=<model name you want to test>        # optional; defaults to Claude Code's default
bash bench/fetch_corpora.sh                       # clones into ./bench-work/corpora, prints tree-sha check
bash bench/monitor_load.sh &                      # optional
bash bench/run_pairs.sh --tasks N5,N8 --pairs 3 --parallel 2
python3 bench/report.py                           # reads ./bench-work/runs
```

Credentials: `claude` inherits your environment (for example `ANTHROPIC_API_KEY`, `ANTHROPIC_BASE_URL`). The sandbox gives each run an empty `$HOME`, so OAuth sessions stored in `~/.claude` are not available inside it — use API-key environment variables. Nothing in this directory contains keys, endpoints or personal paths.

## Method (what makes it a fair comparison)

- **Paired, simultaneous start** (barrier). Per-turn latency drifted by about 3x over a day in our runs (5 s to 15 s median), so non-simultaneous comparisons are unreliable for time. Cost and correctness are less sensitive.
- **Isolation**: bubblewrap with tmpfs `$HOME` and `/tmp`; the bench dir, corpora and other runs are hidden from the model; per-run Go build cache; WebSearch and WebFetch disallowed in both arms (otherwise the model can look up the prompt text).
- **Metrics**: cost = Claude Code `total_cost_usd`; time = `duration_ms`; turns = assistant messages; adoption = any `mcp__astrolabe__*` call.
- **Statistics**: mean paired difference with a 95% percentile bootstrap over pairs (20,000 resamples, fixed seed). Tasks are not independent samples of "all coding work"; treat intervals as describing these tasks only.
- **Timeouts** (default 1500 s) are not counted as wrong by themselves: the run is scored on whatever answer or repo state it left.

## Scoring conventions

- Answer tasks put the answer on a final `ANSWER:` line; every regex in `must` has to match it and none in `must_not` may.
- **N5 and N8 have two accepted conventions** because some counted call sites sit inside comments: `must_with_comments` (the reference count includes them) and `must_skip_comments` (comment hits excluded, which is what `search_code skip_comments=true` returns). `score.py` reports `ok` (reference convention) and `ok_either`. Our README reports both for N5.
- Edit tasks also need `checks.py` to pass (a verify command plus a needle string in the tree).

## What is deliberately not here

Six of the 15 tasks in our runs cannot be published: five use a **private repository** (N4, E3b, H4, H5, E1; a Flutter + Go monorepo and a Rust codebase) and N7 uses an edited copy of cobra with injected compile errors. Their prompts and checks reveal private file names, so they are not published. Consequences:
- The headline 40-pair result includes N4 and E3b, which **others cannot reproduce**. Only N5 and N8 (20 of the 40 pairs) can be re-run on public code.
- The new-api / cobra / express snapshots we used were local single-commit copies with unknown upstream commits (new-api had one uncommitted change). `fetch_corpora.sh` compares your checkout's tree sha with ours; if it differs, expected counts (N5, N8, N2, N3, H1–H3) may not match and results are not comparable with our reports.

## Known limitations

One model family and one proxy; small samples (10 pairs per task); tasks chosen by us; no simultaneous v0.13.0-vs-A run on the other 11 tasks; N5/N8 are counting questions where `aggregate=top` fits well, so they favor Astrolabe by design. The older v0.12.0 matrix (`results/v012_baseline_summary.md`) shows the other side: no benefit across 15 tasks.
