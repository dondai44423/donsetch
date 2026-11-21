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

pub(super) async fn run<'a>(
    mut lanes: Vec<(String, TaskFut<'a>)>,
    mut outcomes: Vec<(String, EngineResult)>,
    families: &families::Families,
    early: bool,
    news: bool,
    priority: &[String],
) -> (Vec<(String, EngineResult)>, Vec<String>) {
    // Keep general engines beyond the initial 3-engine wave in an ordered
    // reserve queue. When the initial 3 answer cleanly, reserves spend zero
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
    let reserve_target = engines.len().saturating_sub(3);
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
    let soft = start + super::stage_budget(super::FANOUT_SOFT_MARK, Duration::from_millis(300));
    let hard = start + super::stage_budget(super::FANOUT_HARD_MARK, Duration::from_millis(200));
    let news_mark = (start + Duration::from_millis(1800)).min(hard);
    let priority_mark = (start + Duration::from_millis(2200)).min(hard);
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
        while !reserves.is_empty() {
            let need_failover = failed_initial_count(&outcomes) > reserves_launched;
            let need_soft = !soft_hedged && (now >= soft || futures.is_empty());
            if !need_failover && !need_soft {
                break;
            }
            if !independently_answered(&outcomes, families) {
                if let Some((name, future)) = reserves.pop_front() {
                    *pending.entry(name).or_default() += 1;
                    futures.push(future);
                    reserves_launched += 1;
                    if !need_failover {
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
            (soft + super::FANOUT_LATE_MARK).min(hard)
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
}
