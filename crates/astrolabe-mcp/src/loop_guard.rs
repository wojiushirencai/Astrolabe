//! Server-side loop guard for the search tools (`search_code` / `find_symbol`).
//!
//! Hooks deliberately do not count Astrolabe's own tools, so a model stuck in a
//! loop (observed: 9,055 `search_code` calls in 48 minutes cycling through 109
//! distinct queries) is never stopped client-side. This guard lives in the MCP
//! server process (one per client connection) and:
//!
//! * short-circuits a query whose signature (tool + query + path filter +
//!   flags) already ran more than `repeat_max` times inside `repeat_window`,
//!   returning a short "you already ran this" note instead of re-searching;
//! * appends a warning once the overall search call count inside
//!   `rate_window` exceeds `rate_max`.
//!
//! Thresholds come from env vars (see [`LoopGuardConfig::from_env`]); `0`
//! disables the corresponding check.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

pub(crate) const DEFAULT_REPEAT_MAX: usize = 3;
pub(crate) const DEFAULT_REPEAT_WINDOW_SECS: u64 = 600;
pub(crate) const DEFAULT_RATE_MAX: usize = 300;
pub(crate) const DEFAULT_RATE_WINDOW_SECS: u64 = 600;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LoopGuardConfig {
    /// Allowed runs of one signature per window; `0` disables.
    pub repeat_max: usize,
    pub repeat_window: Duration,
    /// Search calls per window before warning; `0` disables.
    pub rate_max: usize,
    pub rate_window: Duration,
}

impl Default for LoopGuardConfig {
    fn default() -> Self {
        Self {
            repeat_max: DEFAULT_REPEAT_MAX,
            repeat_window: Duration::from_secs(DEFAULT_REPEAT_WINDOW_SECS),
            rate_max: DEFAULT_RATE_MAX,
            rate_window: Duration::from_secs(DEFAULT_RATE_WINDOW_SECS),
        }
    }
}

fn parse_num<T: std::str::FromStr + Copy>(raw: Option<&str>, default: T, name: &str) -> T {
    match raw.map(str::trim).filter(|s| !s.is_empty()) {
        None => default,
        Some(s) => s.parse().unwrap_or_else(|_| {
            tracing::warn!(value = s, knob = name, "invalid value; using default");
            default
        }),
    }
}

impl LoopGuardConfig {
    pub(crate) fn from_env() -> Self {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    pub(crate) fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Self {
        let n = |k: &str, d: usize| parse_num(get(k).as_deref(), d, k);
        let s = |k: &str, d: u64| {
            let v = parse_num(get(k).as_deref(), d, k);
            Duration::from_secs(if v == 0 { d } else { v })
        };
        Self {
            repeat_max: n("ASTROLABE_SEARCH_REPEAT_MAX", DEFAULT_REPEAT_MAX),
            repeat_window: s(
                "ASTROLABE_SEARCH_REPEAT_WINDOW_SECS",
                DEFAULT_REPEAT_WINDOW_SECS,
            ),
            rate_max: n("ASTROLABE_SEARCH_RATE_MAX", DEFAULT_RATE_MAX),
            rate_window: s(
                "ASTROLABE_SEARCH_RATE_WINDOW_SECS",
                DEFAULT_RATE_WINDOW_SECS,
            ),
        }
    }
}

/// Outcome of [`LoopGuard::check`].
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// Run the search normally.
    Run,
    /// Run, but append this warning to the result.
    RunWithWarning(String),
    /// Do not run; return this text instead.
    Block(String),
}

#[derive(Debug, Default)]
pub(crate) struct LoopGuard {
    config: LoopGuardConfig,
    per_sig: HashMap<String, VecDeque<Instant>>,
    all: VecDeque<Instant>,
}

fn prune(q: &mut VecDeque<Instant>, now: Instant, window: Duration) {
    while q
        .front()
        .is_some_and(|t| now.saturating_duration_since(*t) >= window)
    {
        q.pop_front();
    }
}

impl LoopGuard {
    pub(crate) fn new(config: LoopGuardConfig) -> Self {
        Self {
            config,
            ..Self::default()
        }
    }

    pub(crate) fn check(&mut self, tool: &str, signature: &str) -> Verdict {
        self.check_at(tool, signature, Instant::now())
    }

    pub(crate) fn check_at(&mut self, tool: &str, signature: &str, now: Instant) -> Verdict {
        let cfg = self.config;
        let mut rate_note = None;
        if cfg.rate_max > 0 {
            prune(&mut self.all, now, cfg.rate_window);
            self.all.push_back(now);
            if self.all.len() > cfg.rate_max {
                rate_note = Some(format!(
                    "⚠ loop guard: 最近 {} 秒内已调用搜索工具 {} 次（上限 {}）。\
                     很可能陷入了循环：请停止批量轮换查询，基于已有结果作答，或换用 \
                     find_references / get_repo_skeleton / Read 等方式推进。",
                    cfg.rate_window.as_secs(),
                    self.all.len(),
                    cfg.rate_max
                ));
            }
        }
        if cfg.repeat_max > 0 {
            let window = cfg.repeat_window;
            // Bound memory: drop signatures whose entries all expired.
            if self.per_sig.len() > 4096 {
                self.per_sig.retain(|_, q| {
                    prune(q, now, window);
                    !q.is_empty()
                });
            }
            let q = self
                .per_sig
                .entry(format!("{tool}\u{1f}{signature}"))
                .or_default();
            prune(q, now, window);
            let prior = q.len();
            q.push_back(now);
            if prior >= cfg.repeat_max {
                let mut text = format!(
                    "loop guard: 这条 {tool} 查询在最近 {} 秒内已执行 {prior} 次（上限 {}），本次未重新执行。\
                     结果与之前相同——请直接使用先前的结果；若需要不同信息，请修改查询/path_filter，\
                     或换一种方法（find_references、goto_definition、Read 指定行段）。\
                     不要在同一组查询之间反复轮换。",
                    window.as_secs(),
                    cfg.repeat_max
                );
                if let Some(note) = rate_note {
                    text.push('\n');
                    text.push_str(&note);
                }
                return Verdict::Block(text);
            }
        }
        match rate_note {
            Some(note) => Verdict::RunWithWarning(note),
            None => Verdict::Run,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(repeat_max: usize, rate_max: usize) -> LoopGuardConfig {
        LoopGuardConfig {
            repeat_max,
            repeat_window: Duration::from_secs(600),
            rate_max,
            rate_window: Duration::from_secs(600),
        }
    }

    #[test]
    fn repeats_within_window_are_blocked_after_max() {
        let mut g = LoopGuard::new(cfg(3, 0));
        let t = Instant::now();
        for i in 0..3 {
            assert_eq!(
                g.check_at("search_code", "q", t + Duration::from_secs(i)),
                Verdict::Run
            );
        }
        assert!(matches!(
            g.check_at("search_code", "q", t + Duration::from_secs(4)),
            Verdict::Block(_)
        ));
        // other signature / other tool unaffected
        assert_eq!(g.check_at("search_code", "other", t), Verdict::Run);
        assert_eq!(g.check_at("find_symbol", "q", t), Verdict::Run);
    }

    #[test]
    fn rotating_queries_are_caught_and_window_expires() {
        let mut g = LoopGuard::new(cfg(3, 0));
        let t = Instant::now();
        let mut blocked = 0;
        // 5 queries rotated, never back-to-back, 20 rounds, 1s apart
        for round in 0..20u64 {
            for q in 0..5u64 {
                let now = t + Duration::from_secs(round * 5 + q);
                if matches!(
                    g.check_at("search_code", &q.to_string(), now),
                    Verdict::Block(_)
                ) {
                    blocked += 1;
                }
            }
        }
        assert_eq!(blocked, 100 - 15);
        // after the window passes the query runs again
        let later = t + Duration::from_secs(100 + 600);
        assert_eq!(g.check_at("search_code", "0", later), Verdict::Run);
    }

    #[test]
    fn rate_cap_warns_but_runs() {
        let mut g = LoopGuard::new(cfg(0, 5));
        let t = Instant::now();
        for i in 0..5 {
            assert_eq!(g.check_at("search_code", &i.to_string(), t), Verdict::Run);
        }
        assert!(matches!(
            g.check_at("search_code", "x", t),
            Verdict::RunWithWarning(_)
        ));
        assert_eq!(
            g.check_at("search_code", "y", t + Duration::from_secs(601)),
            Verdict::Run
        );
    }

    #[test]
    fn zero_disables_everything() {
        let mut g = LoopGuard::new(cfg(0, 0));
        let t = Instant::now();
        for _ in 0..1000 {
            assert_eq!(g.check_at("search_code", "q", t), Verdict::Run);
        }
    }

    #[test]
    fn config_from_env_parses_and_falls_back() {
        let c = LoopGuardConfig::from_lookup(|_| None);
        assert_eq!(c, LoopGuardConfig::default());
        let c = LoopGuardConfig::from_lookup(|k| match k {
            "ASTROLABE_SEARCH_REPEAT_MAX" => Some("5".into()),
            "ASTROLABE_SEARCH_RATE_MAX" => Some("bogus".into()),
            "ASTROLABE_SEARCH_REPEAT_WINDOW_SECS" => Some("0".into()),
            _ => None,
        });
        assert_eq!(c.repeat_max, 5);
        assert_eq!(c.rate_max, DEFAULT_RATE_MAX);
        assert_eq!(
            c.repeat_window,
            Duration::from_secs(DEFAULT_REPEAT_WINDOW_SECS)
        );
    }
}
