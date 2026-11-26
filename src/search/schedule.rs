//! Bounded fan-out: preserve pending requests between marks and hedge once.

use super::{EngineResult, TaskFut, families, rank};
use futures_util::{StreamExt, stream::FuturesUnordered};
use std::collections::{HashMap, VecDeque};
use std::time::Duration;

fn independently_answered(
    outcomes: &[(String, EngineResult)],
    families: &families::Families,
) -> bool {
    let sources: Vec<_> = outcomes
        .iter()
        .filter_map(|(name, answer)| {
            answer
                .as_ref()
                .ok()
                .filter(|a| a.class.credits() && !a.hits.is_empty())
                .map(|_| name.as_str())
        })
        .collect();
    sources.iter().enumerate().any(|(i, a)| {
        sources[i + 1..]
            .iter()
            .any(|b| super::independent_pair(a, b, families))
    })
}

fn failed_initial_count(outcomes: &[(String, EngineResult)]) -> usize {
    outcomes
        .iter()
        .filter(|(name, result)| {
            if rank::is_vertical(super::engine_name(name)) {
                return false;
            }
            match result {
                Ok(answer) => !answer.class.credits() || answer.hits.is_empty(),
                Err((status, _, engine_lane)) => {
                    *engine_lane
                        && !matches!(
                            status.split(':').next().unwrap_or(status),
                            "not-asked" | "rested" | "budget-skipped" | "pacing-timeout"
                        )
                }
            }
        })
        .count()
}

/// Scheduling posture for one search fan-out.
#[derive(Debug, Clone, Copy)]
pub(super) struct ScheduleOpts {
    pub early: bool,
    pub news: bool,
    /// Run all planned lanes strictly one at a time with no early stop (#478).
    pub sequential: bool,
    /// General engines launched in the initial parallel wave (1..=8, default 3).
    pub wave_size: usize,
}

#[cfg(test)]
pub(super) async fn run<'a>(
    lanes: Vec<(String, TaskFut<'a>)>,
    outcomes: Vec<(String, EngineResult)>,
    families: &families::Families,
    early: bool,
    news: bool,
    priority: &[String],
) -> (Vec<(String, EngineResult)>, Vec<String>) {
    let cfg = &crate::config::cfg().search;
    run_with_opts(
        lanes,
        outcomes,
        families,
        ScheduleOpts {
            early,
            news,
            sequential: cfg.sequential,
            wave_size: cfg.wave_size,
        },
        priority,
    )
    .await
}

/// Sequential mode for slow or congested links (#478): poll every planned lane
/// strictly one at a time (concurrency = 1) with no early stop until every
/// engine has had a turn, bounding each lane by the configured search timeout
/// (and any explicit caller `SEARCH_DEADLINE`).
async fn run_sequential<'a>(
    lanes: Vec<(String, TaskFut<'a>)>,
    mut outcomes: Vec<(String, EngineResult)>,
) -> (Vec<(String, EngineResult)>, Vec<String>) {
    let mut late: Vec<String> = Vec::new();
    let mut iter = lanes.into_iter();
    while let Some((name, future)) = iter.next() {
        let per_lane = super::stage_budget(super::fanout_hard_mark(), Duration::from_millis(200));
        if per_lane.is_zero() {
            outcomes.push((name, Err(("not-asked".into(), "deadline".into(), false))));
            for (rem_name, _) in iter {
                outcomes.push((
                    rem_name,
                    Err(("not-asked".into(), "deadline".into(), false)),
                ));
            }
            break;
        }
        match tokio::time::timeout(per_lane, future).await {
            Ok((done_name, answer)) => {
                super::offer_early_page(&done_name, &answer);
                outcomes.push((done_name, answer));
            }
            Err(_) => {
                late.push(name);
            }
        }
    }
    late.sort();
    (outcomes, late)
}

pub(super) async fn run_with_opts<'a>(
    mut lanes: Vec<(String, TaskFut<'a>)>,
    mut outcomes: Vec<(String, EngineResult)>,
    families: &families::Families,
    opts: ScheduleOpts,
    priority: &[String],
) -> (Vec<(String, EngineResult)>, Vec<String>) {
    if opts.sequential {
        return run_sequential(lanes, outcomes).await;
    }
    let early = opts.early;
    let news = opts.news;
    let wave_size = opts.wave_size.clamp(1, 8);

    // Keep general engines beyond the initial `wave_size` wave in an ordered
    // reserve queue. When the initial wave answers cleanly, reserves spend zero
    // requests; when any active lane fails or returns thin/off-query, the next
    // healthy reserve fires immediately.
    let engines: Vec<_> = lanes
        .iter()
        .enumerate()
        .filter(|(_, (n, _))| {
            !rank::is_vertical(n)
                && !matches!(n.as_str(), "bing-news" | "feed" | "gdelt" | "registry")
        })
        .map(|(i, _)| i)
        .collect();
    let mut family_counts = HashMap::<String, usize>::new();
    for &i in &engines {
        *family_counts
            .entry(rank::family_name(families, super::engine_name(&lanes[i].0)))
            .or_default() += 1;
    }
    let eligible: Vec<_> = engines
        .iter()
        .rev()
        .copied()
        .filter(|i| !priority.contains(&lanes[*i].0))
        .collect();
    let reserve_target = engines.len().saturating_sub(wave_size);
    let mut chosen: Vec<usize> = Vec::with_capacity(reserve_target);
    // Reserve measured duplicates before delaying the only planned source in
    // another ranking family. Unknown suppliers are never hard-coded as peers.
    for &i in &eligible {
        if chosen.len() >= reserve_target {
            break;
        }
        let fam = rank::family_name(families, super::engine_name(&lanes[i].0));
        if family_counts.get(&fam).copied().unwrap_or(0) > 1 {
            chosen.push(i);
            *family_counts.entry(fam).or_default() -= 1;
        }
    }
    for &i in &eligible {
        if chosen.len() >= reserve_target {
            break;
        }
        if !chosen.contains(&i) {
            chosen.push(i);
        }
    }
    // If `wave_size` is smaller than the number of priority sources, demote
    // trailing priority engines into the reserve queue too so the initial
    // wave strictly honors `wave_size`.
    if chosen.len() < reserve_target {
        let priority_tail: Vec<_> = engines
            .iter()
            .rev()
            .copied()
            .filter(|i| !chosen.contains(i))
            .collect();
        for &i in &priority_tail {
            if chosen.len() >= reserve_target {
                break;
            }
            let fam = rank::family_name(families, super::engine_name(&lanes[i].0));
            if family_counts.get(&fam).copied().unwrap_or(0) > 1 {
                chosen.push(i);
                *family_counts.entry(fam).or_default() -= 1;
            }
        }
        for &i in &priority_tail {
            if chosen.len() >= reserve_target {
                break;
            }
            if !chosen.contains(&i) {
                chosen.push(i);
            }
        }
    }
    // Remove highest indices first so Vec::remove doesn't shift earlier ones,
    // then reverse so reserves fire highest-ranked first.
    chosen.sort_unstable_by(|a, b| b.cmp(a));
    let mut removed: Vec<(usize, (String, TaskFut<'a>))> = chosen
        .into_iter()
        .map(|idx| (idx, lanes.remove(idx)))
        .collect();
    removed.sort_unstable_by_key(|(idx, _)| *idx);
    let mut reserves: VecDeque<(String, TaskFut<'a>)> =
        removed.into_iter().map(|(_, lane)| lane).collect();
    let mut reserves_launched = 0usize;
    let mut soft_hedged = false;
    let start = tokio::time::Instant::now();
    let soft_dur = super::stage_budget(super::fanout_soft_mark(), Duration::from_millis(300));
    let hard_dur = super::stage_budget(super::fanout_hard_mark(), Duration::from_millis(200));
    let soft = start + soft_dur;
    let hard = start + hard_dur;
    let news_mark = (start + (hard_dur * 9 / 20).max(Duration::from_millis(1800))).min(hard);
    let priority_mark = (start + (hard_dur * 11 / 20).max(Duration::from_millis(2200))).min(hard);
    let mut pending: HashMap<String, usize> = HashMap::new();
    let mut futures: FuturesUnordered<TaskFut<'a>> = FuturesUnordered::new();
    for (name, future) in lanes {
        *pending.entry(name).or_default() += 1;
        futures.push(future);
    }
    loop {
        // A resolved owner/item is an exact answer; broad search cannot
        // disambiguate it further. Ambiguous multi-registry records keep waiting.
        if outcomes.iter().any(|(label, answer)| {
            let exact = label == "docs"
                || label
                    .strip_prefix("registry@")
                    .is_some_and(|owner| owner != "registry");
            exact
                && answer
                    .as_ref()
                    .is_ok_and(|answer| answer.class.credits() && !answer.hits.is_empty())
        }) {
            break;
        }
        let now = tokio::time::Instant::now();
        let news_pending = news
            && ["bing-news", "feed", "gdelt"]
                .iter()
                .any(|n| pending.get(*n).copied().unwrap_or(0) > 0);
        let priority_pending = now < priority_mark
            && priority
                .iter()
                .any(|name| pending.get(name).copied().unwrap_or(0) > 0);
        if !priority_pending
            && super::enough_answered(&outcomes, early, families)
            && (!news_pending || now >= news_mark)
        {
            break;
        }
        if now >= hard {
            break;
        }
        let mut wave_refill = if futures.is_empty() { wave_size } else { 0 };
        while !reserves.is_empty() {
            let need_failover = failed_initial_count(&outcomes) > reserves_launched;
            let need_soft = !soft_hedged && now >= soft;
            let need_wave = wave_refill > 0;
            if !need_failover && !need_soft && !need_wave {
                break;
            }
            if !independently_answered(&outcomes, families) {
                if let Some((name, future)) = reserves.pop_front() {
                    *pending.entry(name).or_default() += 1;
                    futures.push(future);
                    reserves_launched += 1;
                    if need_wave {
                        wave_refill -= 1;
                    } else if !need_failover {
                        soft_hedged = true;
                    }
                }
            } else {
                while let Some((name, _)) = reserves.pop_front() {
                    outcomes.push((name, Err(("not-asked".into(), "hedge".into(), false))));
                }
                break;
            }
        }
        if futures.is_empty() {
            break;
        }
        let until = if now < soft {
            soft.min(hard)
        } else if super::answered(&outcomes).1 == 0 {
            hard
        } else if priority_pending {
            priority_mark
        } else if news_pending {
            news_mark
        } else {
            (soft + super::fanout_late_mark()).min(hard)
        };
        if now >= until {
            break;
        }
        match tokio::time::timeout_at(until, futures.next()).await {
            Ok(Some((name, answer))) => {
                super::offer_early_page(&name, &answer);
                if let Some(n) = pending.get_mut(super::lane_source(super::engine_name(&name))) {
                    *n = n.saturating_sub(1);
                }
                outcomes.push((name, answer));
            }
            Ok(None) => break,
            // Pending requests remain polled at the next mark. Never restart
            // them: cancellation/requeue was burning their own pacing slots.
            Err(_) => continue,
        }
    }
    while let Some((name, _)) = reserves.pop_front() {
        outcomes.push((name, Err(("not-asked".into(), "hedge".into(), false))));
    }
    let mut late: Vec<_> = pending
        .into_iter()
        .filter(|(_, n)| *n > 0)
        .map(|(n, _)| n)
        .collect();
    late.sort();
    (outcomes, late)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn a_measured_duplicate_is_hedged_before_a_distinct_planned_source() {
        for known in [false, true] {
            let mut families = families::Families::default();
            if known {
                for _ in 0..3 {
                    families.observe(&[
                        (
                            "bing".into(),
                            (0..5).map(|i| format!("https://a.example/{i}")).collect(),
                        ),
                        (
                            "ddg".into(),
                            (0..5).map(|i| format!("https://a.example/{i}")).collect(),
                        ),
                    ]);
                }
            }
            let start = tokio::time::Instant::now();
            let starts = std::sync::Arc::new(std::sync::Mutex::new(HashMap::new()));
            let lanes = ["bing", "ddg", "yahoo", "yandex"]
                .into_iter()
                .map(|name| {
                    let starts = std::sync::Arc::clone(&starts);
                    let future: TaskFut<'_> = Box::pin(async move {
                        starts.lock().unwrap().insert(name, start.elapsed());
                        tokio::time::sleep(Duration::from_secs(10)).await;
                        (name.into(), Err(("late".into(), "direct".into(), true)))
                    });
                    (name.into(), future)
                })
                .collect();
            run(lanes, vec![], &families, false, false, &["yahoo".into()]).await;
            let starts = starts.lock().unwrap();
            let deferred = if known { "ddg" } else { "yandex" };
            let initial = if known { "yandex" } else { "ddg" };
            assert_eq!(starts[initial], Duration::ZERO);
            assert!(starts[deferred] >= super::super::FANOUT_SOFT_MARK);
            assert_eq!(
                starts["yahoo"],
                Duration::ZERO,
                "priority recall keeps its initial slot"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn an_empty_soft_mark_keeps_listening_to_the_original_request() {
        let lane: TaskFut<'_> = Box::pin(async {
            tokio::time::sleep(Duration::from_millis(1600)).await;
            (
                "bing".into(),
                Err(("no-results".into(), "direct".into(), true)),
            )
        });
        let (done, late) = run(
            vec![("bing".into(), lane)],
            vec![],
            &Default::default(),
            false,
            false,
            &[],
        )
        .await;
        assert_eq!(done.len(), 1);
        assert!(late.is_empty());
        assert_eq!(done[0].1.as_ref().unwrap_err().0, "no-results");
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_initial_source_starts_the_hedge_without_waiting_for_the_soft_mark() {
        for (status, actual, early) in [("blocked:403", true, true), ("not-asked", false, false)] {
            let start = tokio::time::Instant::now();
            let hedge_at = std::sync::Arc::new(std::sync::Mutex::new(None));
            let mut lanes = Vec::new();
            for name in ["bing", "ddg", "yahoo", "brave"] {
                let hedge_at = std::sync::Arc::clone(&hedge_at);
                let future: TaskFut<'_> = Box::pin(async move {
                    if name == "brave" {
                        *hedge_at.lock().unwrap() = Some(start.elapsed());
                    } else {
                        tokio::time::sleep(if name == "bing" {
                            Duration::from_millis(20)
                        } else {
                            Duration::from_secs(10)
                        })
                        .await;
                    }
                    (name.into(), Err((status.into(), "direct".into(), actual)))
                });
                lanes.push((name.into(), future));
            }
            let _ = run(lanes, vec![], &Default::default(), false, false, &[]).await;
            let at = hedge_at.lock().unwrap().unwrap();
            if early {
                assert!(at < Duration::from_millis(100), "{at:?}");
            } else {
                assert!(at >= super::super::FANOUT_SOFT_MARK, "{at:?}");
            }
        }
        // A thin list may contribute URLs, but cannot suppress the backup that
        // is meant to supply the second complete independent answer.
        use super::super::{
            engines::{AnswerClass, Hit},
            tasks::LaneAnswer,
        };
        for (class, measurement) in [
            (AnswerClass::Thin, 1),
            (AnswerClass::Answered, 0),
            (AnswerClass::Answered, 1),
            (AnswerClass::Answered, 2),
        ] {
            let mut families = families::Families::default();
            if measurement != 0 {
                let lists: Vec<_> = ["bing", "ddg"]
                    .into_iter()
                    .map(|name| {
                        let host = if measurement == 2 { "same" } else { name };
                        (
                            name.to_string(),
                            (0..5)
                                .map(|i| format!("https://{host}.example/{i}"))
                                .collect(),
                        )
                    })
                    .collect();
                for _ in 0..3 {
                    families.observe(&lists);
                }
            }
            let answer = |class| {
                Ok(LaneAnswer {
                    hits: vec![Hit {
                        title: "An answer".into(),
                        url: "https://example.com/record".into(),
                        snippet: "Source evidence".into(),
                        rank: 0,
                        published: None,
                    }],
                    items: 1,
                    ms: 0,
                    egress: "direct".into(),
                    engine_lane: true,
                    instant: None,
                    class,
                    requests: 1,
                    cache_age: None,
                })
            };
            let initial = vec![
                ("bing".into(), answer(AnswerClass::Answered)),
                ("ddg".into(), answer(class)),
            ];
            let started = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let lanes = ["yahoo", "brave", "yandex", "backup"]
                .into_iter()
                .map(|name| {
                    let started = std::sync::Arc::clone(&started);
                    let f: TaskFut<'_> = Box::pin(async move {
                        if name == "backup" {
                            started.store(true, std::sync::atomic::Ordering::Relaxed);
                        }
                        tokio::time::sleep(Duration::from_secs(10)).await;
                        (name.into(), Err(("late".into(), "direct".into(), true)))
                    });
                    (name.into(), f)
                })
                .collect();
            run(lanes, initial, &families, false, false, &[]).await;
            assert_eq!(
                started.load(std::sync::atomic::Ordering::Relaxed),
                class == AnswerClass::Thin || measurement != 1,
                "{class:?}, measurement={measurement}"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_hedge_starts_once_when_the_initial_sources_are_silent() {
        let lanes = ["bing", "ddg", "yahoo", "brave"]
            .into_iter()
            .map(|name| {
                let future: TaskFut<'_> = Box::pin(async move {
                    if name != "brave" {
                        tokio::time::sleep(Duration::from_secs(10)).await;
                    }
                    (
                        name.into(),
                        Err(("no-results".into(), "direct".into(), true)),
                    )
                });
                (name.into(), future)
            })
            .collect();
        let (done, late) = run(lanes, vec![], &Default::default(), false, false, &[]).await;
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].0, "brave");
        assert_eq!(late.len(), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn multiple_failures_promote_multiple_reserves_in_order() {
        let start = tokio::time::Instant::now();
        let starts = std::sync::Arc::new(std::sync::Mutex::new(HashMap::new()));
        let mut lanes = Vec::new();
        for name in ["google-cse", "keenable", "exa-mcp", "parallel-mcp", "bing"] {
            let starts = std::sync::Arc::clone(&starts);
            let future: TaskFut<'_> = Box::pin(async move {
                starts.lock().unwrap().insert(name, start.elapsed());
                match name {
                    "google-cse" => {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                        (
                            name.into(),
                            Err(("rate-limited:60".into(), "direct".into(), true)),
                        )
                    }
                    "keenable" => {
                        tokio::time::sleep(Duration::from_millis(40)).await;
                        (
                            name.into(),
                            Err(("blocked:403".into(), "direct".into(), true)),
                        )
                    }
                    _ => {
                        tokio::time::sleep(Duration::from_secs(10)).await;
                        (name.into(), Err(("late".into(), "direct".into(), true)))
                    }
                }
            });
            lanes.push((name.into(), future));
        }
        let _ = run(
            lanes,
            vec![],
            &Default::default(),
            false,
            false,
            &["google-cse".into(), "keenable".into(), "exa-mcp".into()],
        )
        .await;
        let starts = starts.lock().unwrap();
        // Both reserves must promote promptly in priority order before the soft mark.
        assert!(
            starts["parallel-mcp"] < Duration::from_millis(100),
            "{starts:?}"
        );
        assert!(starts["bing"] < Duration::from_millis(100), "{starts:?}");
        assert!(starts["parallel-mcp"] <= starts["bing"], "{starts:?}");
    }

    #[test]
    fn issue_470_fanout_marks_preserve_defaults_and_scale_up() {
        let (soft, hard, late) = super::super::fanout_marks_for(4000);
        assert_eq!(soft, Duration::from_millis(1200));
        assert_eq!(hard, Duration::from_millis(4000));
        assert_eq!(late, Duration::from_millis(600));

        let (soft, hard, late) = super::super::fanout_marks_for(12_000);
        assert_eq!(soft, Duration::from_millis(3600));
        assert_eq!(hard, Duration::from_millis(12_000));
        assert_eq!(late, Duration::from_millis(1800));

        // Clamped extremes remain sane (soft < hard).
        let (soft, hard, late) = super::super::fanout_marks_for(1000);
        assert_eq!(hard, Duration::from_millis(1000));
        assert!(soft < hard && late > Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn issue_478_sequential_runs_one_at_a_time_with_no_early_stop() {
        use super::super::{
            engines::{AnswerClass, Hit},
            tasks::LaneAnswer,
        };
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let mut families = families::Families::default();
        for _ in 0..3 {
            families.observe(&[
                (
                    "bing".into(),
                    (0..5).map(|i| format!("https://b.example/{i}")).collect(),
                ),
                (
                    "brave".into(),
                    (0..5).map(|i| format!("https://r.example/{i}")).collect(),
                ),
            ]);
        }

        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let order = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));

        let lanes = ["github", "bing", "brave", "yahoo"]
            .into_iter()
            .map(|name| {
                let active = Arc::clone(&active);
                let max_active = Arc::clone(&max_active);
                let order = Arc::clone(&order);
                let future: TaskFut<'_> = Box::pin(async move {
                    let cur = active.fetch_add(1, Ordering::SeqCst) + 1;
                    max_active.fetch_max(cur, Ordering::SeqCst);
                    order.lock().unwrap().push(name.to_string());
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    active.fetch_sub(1, Ordering::SeqCst);
                    (
                        name.into(),
                        Ok(LaneAnswer {
                            hits: vec![Hit {
                                title: "Shared top hit".into(),
                                url: "https://consensus.example/page".into(),
                                snippet: "Exact agreement".into(),
                                rank: 0,
                                published: None,
                            }],
                            items: 1,
                            ms: 250,
                            egress: "direct".into(),
                            engine_lane: name != "github",
                            instant: None,
                            class: AnswerClass::Answered,
                            requests: 1,
                            cache_age: None,
                        }),
                    )
                });
                (name.into(), future)
            })
            .collect();

        let (done, late) = run_with_opts(
            lanes,
            vec![],
            &families,
            ScheduleOpts {
                early: true,
                news: false,
                sequential: true,
                wave_size: 3,
            },
            &["bing".into()],
        )
        .await;

        assert_eq!(
            max_active.load(Ordering::SeqCst),
            1,
            "sequential mode must never run two lanes concurrently"
        );
        assert_eq!(
            *order.lock().unwrap(),
            vec!["github", "bing", "brave", "yahoo"],
            "every planned lane must get a turn in order without early stop"
        );
        assert_eq!(done.len(), 4);
        assert!(late.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn issue_478_sequential_marks_timed_out_lane_late_and_continues_to_next() {
        use super::super::{
            engines::{AnswerClass, Hit},
            tasks::LaneAnswer,
        };

        let lanes = ["slow", "fast"]
            .into_iter()
            .map(|name| {
                let future: TaskFut<'_> = Box::pin(async move {
                    if name == "slow" {
                        tokio::time::sleep(Duration::from_secs(30)).await;
                    } else {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    (
                        name.into(),
                        Ok(LaneAnswer {
                            hits: vec![Hit {
                                title: "Fast hit".into(),
                                url: "https://fast.example/1".into(),
                                snippet: "ok".into(),
                                rank: 0,
                                published: None,
                            }],
                            items: 1,
                            ms: 100,
                            egress: "direct".into(),
                            engine_lane: true,
                            instant: None,
                            class: AnswerClass::Answered,
                            requests: 1,
                            cache_age: None,
                        }),
                    )
                });
                (name.into(), future)
            })
            .collect();

        let (done, late) = run_with_opts(
            lanes,
            vec![],
            &Default::default(),
            ScheduleOpts {
                early: true,
                news: false,
                sequential: true,
                wave_size: 1,
            },
            &[],
        )
        .await;

        assert_eq!(late, vec!["slow"]);
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].0, "fast");
        assert!(done[0].1.is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn issue_478_wave_size_caps_initial_wave_even_with_priority_and_advances() {
        use super::super::{
            engines::{AnswerClass, Hit},
            tasks::LaneAnswer,
        };
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let start = tokio::time::Instant::now();
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let starts = Arc::new(std::sync::Mutex::new(HashMap::<String, Duration>::new()));

        let lanes = ["google-cse", "keenable", "exa-mcp", "bing"]
            .into_iter()
            .map(|name| {
                let active = Arc::clone(&active);
                let max_active = Arc::clone(&max_active);
                let starts = Arc::clone(&starts);
                let future: TaskFut<'_> = Box::pin(async move {
                    starts.lock().unwrap().insert(name.into(), start.elapsed());
                    let cur = active.fetch_add(1, Ordering::SeqCst) + 1;
                    max_active.fetch_max(cur, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(80)).await;
                    active.fetch_sub(1, Ordering::SeqCst);
                    (
                        name.into(),
                        Ok(LaneAnswer {
                            hits: vec![Hit {
                                title: format!("{name} hit"),
                                url: format!("https://{name}.example/1"),
                                snippet: "ok".into(),
                                rank: 0,
                                published: None,
                            }],
                            items: 1,
                            ms: 80,
                            egress: "direct".into(),
                            engine_lane: true,
                            instant: None,
                            class: AnswerClass::Answered,
                            requests: 1,
                            cache_age: None,
                        }),
                    )
                });
                (name.into(), future)
            })
            .collect();

        // Even with 3 priority sources, wave_size = 1 must launch only 1 at t = 0
        // and advance cleanly one wave at a time.
        let (done, late) = run_with_opts(
            lanes,
            vec![],
            &Default::default(),
            ScheduleOpts {
                early: false,
                news: false,
                sequential: false,
                wave_size: 1,
            },
            &["google-cse".into(), "keenable".into(), "exa-mcp".into()],
        )
        .await;

        assert_eq!(max_active.load(Ordering::SeqCst), 1);
        let starts = starts.lock().unwrap();
        assert_eq!(starts["google-cse"], Duration::ZERO);
        assert!(starts["keenable"] >= Duration::from_millis(80));
        assert!(starts["exa-mcp"] >= Duration::from_millis(160));
        assert_eq!(done.len(), 4);
        assert!(late.is_empty());
    }
}
