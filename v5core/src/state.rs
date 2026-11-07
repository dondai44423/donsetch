//! State: what each origin accepted, and the cookies that identity earned.
//!
//! Two things are stored, both with bindings:
//!   * a score per (origin, identity) — the selection signal;
//!   * cookies per (host, identity) — the state a request may spend.
//!
//! Cookies are never shared across identities or hosts. Presenting an identity's
//! token to another client is worse than presenting none.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Score {
    pub ok: u32,
    pub fail: u32,
    pub total_ms: f64,
    pub last_class: Option<String>,
    pub last_ms: f64,
}

impl Score {
    pub fn attempts(&self) -> u32 {
        self.ok + self.fail
    }
    pub fn avg_ms(&self) -> f64 {
        if self.attempts() == 0 {
            0.0
        } else {
            self.total_ms / self.attempts() as f64
        }
    }
    /// Ranking: a proven identity first, then fewer failures, then faster.
    pub fn rank(&self) -> (u32, u32, i64) {
        let proven = if self.ok > 0 { 0 } else { 1 };
        (proven, self.fail, self.avg_ms() as i64)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct OriginState {
    /// How many times this fetcher has primed or fetched this origin.
    #[serde(default)]
    pub visited: u32,
    pub scores: BTreeMap<String, Score>,
    pub chosen: Option<String>,
    pub wall_class: Option<String>,
    pub wall_vendor: Option<String>,
    pub evidence: Vec<String>,
    pub updated_unix: i64,
    /// The identity to try next when the current one is walled.
    pub next: Option<String>,
    /// When this origin was last asked for anything (velocity is identity).
    pub last_ms: i64,
    /// The transport that last delivered content when the document did not
    /// (`data`): the next encounter asks it first.
    #[serde(default)]
    pub route: Option<String>,
    /// Until then this origin's validators are not believed: it answered an
    /// unchanged `ETag`/`Last-Modified` for changed text (`cache::validators_lied`).
    #[serde(default)]
    pub distrust_until_unix: i64,
    /// Its validators were seen to change with its text (`cache::validators_tracked`).
    #[serde(default)]
    pub validators_proven: bool,
    /// The last check inside a declared freshness lifetime found the page
    /// unchanged: the next read inside one may be served without asking.
    #[serde(default)]
    pub fresh_earned: bool,
    /// Its page changed inside a lifetime it declared: until then, every read asks.
    #[serde(default)]
    pub fresh_broken_until_unix: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StoredCookie {
    pub name: String,
    pub value: String,
    pub domain: Option<String>,
    pub path: Option<String>,
    pub expires_unix: Option<i64>,
    pub secure: bool,
    pub http_only: bool,
}

/// How many mints one host may cost inside `MINT_WINDOW_SECS`. A mint is a
/// program run plus calls to the vendor; a host that needs more than this is not
/// being solved by minting, and hammering the vendor's verify endpoint is its own
/// signature.
pub const MINT_CAP: usize = 3;
pub const MINT_WINDOW_SECS: i64 = 600;
/// After a mint the origin refused, the next one waits this long, doubling per
/// consecutive failure up to the maximum. A refused mint is a gap in the engine,
/// not noise: retrying it on every fetch only adds seconds to a wall.
pub const BACKOFF_BASE_SECS: i64 = 600;
pub const BACKOFF_MAX_SECS: i64 = 6 * 3600;
/// A credential is renewed before the age at which the origin refused one, so the
/// refusal is paid once and then avoided.
const REFRESH_RATIO: f64 = 0.9;

/// A credential the engine minted for one (host, identity): what it contains and
/// how it has been treated. The cookie values themselves live in `cookies`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Credential {
    pub vendor: String,
    pub minted_unix: i64,
    /// Cookie names the mint produced, so a refused credential is dropped whole.
    pub names: Vec<String>,
    /// The lifetime the site declared on the cookie, in seconds from minting.
    pub declared_ttl: Option<i64>,
    /// Times the origin accepted it.
    pub uses: u32,
}

/// Per-host minting history and what it taught: the cost of minting is bounded
/// here, and the acceptance window is measured here (the cookie's declared
/// lifetime is client-side; the origin's immunity time is its own).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct MintLedger {
    /// Mint times (unix seconds) inside the window.
    pub recent: Vec<i64>,
    /// Consecutive mints the origin refused.
    pub failures: u32,
    pub blocked_until: i64,
    pub last_error: Option<String>,
    /// Longest age (seconds) at which a credential was accepted.
    pub accepted_age: i64,
    /// Shortest age at which a credential that had worked was refused.
    pub refused_age: Option<i64>,
}

/// Which identities each vendor's wall has let through, best first: the
/// identity matrix (`v5core probe`) over the friendly and hard corpora,
/// 2026-11-06, one direct home connection. A starting point that learned
/// `vendor_priors` override, not a belief: re-measure with `probe` and replace.
pub const VENDOR_SEED: &[(&str, &[&str])] = &[
    // Product Hunt, Glassdoor, npm, Medium: the managed challenge is not served
    // to the app client. Indeed refuses its generic agent and passes Chrome on
    // Windows and Firefox, as npm and Medium also do.
    ("cloudflare", &["okhttp", "chrome-win", "firefox"]),
    // Reuters passes Android Chrome only; Tripadvisor passes the Chrome class.
    // Etsy and Yelp passed no identity.
    ("datadome", &["chrome-android", "chrome-win"]),
    // Bloomberg passes the desktop Chrome class; Zillow everything but `chrome`.
    ("perimeterx", &["chrome-win", "chrome-local"]),
    // eBay's edge refusal: Android Chrome only.
    ("akamai", &["chrome-android"]),
];

/// Per-host entries are shared, not copied: a state is cloned for every fetch
/// (a worker's snapshot, the save), and at the origin cap a deep copy cost more
/// than the request. A clone copies pointers; a write copies the one entry it
/// changes ([`Arc::make_mut`]); a fold skips entries that are still the same
/// allocation.
pub type Shared<T> = Arc<T>;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct State {
    pub version: u32,
    pub origins: BTreeMap<String, Shared<OriginState>>,
    /// host -> identity -> cookies
    pub cookies: BTreeMap<String, Shared<BTreeMap<String, Vec<StoredCookie>>>>,
    /// host -> last call to that interface (data paths only).
    #[serde(default)]
    pub interface_calls: BTreeMap<String, i64>,
    /// host -> epoch ms before which no request should be made (429 cooldowns).
    #[serde(default)]
    pub cooldowns: BTreeMap<String, i64>,
    /// vendor -> identity -> successes. A prior learned across origins, so a
    /// cold origin behind a known WAF starts with the identity that works there.
    #[serde(default)]
    pub vendor_priors: BTreeMap<String, BTreeMap<String, u32>>,
    /// host -> identity -> the engine credential currently held.
    #[serde(default)]
    pub credentials: BTreeMap<String, Shared<BTreeMap<String, Credential>>>,
    /// host -> minting history and measured credential lifetime.
    #[serde(default)]
    pub mints: BTreeMap<String, Shared<MintLedger>>,
}

impl State {
    pub fn load() -> Self {
        match state_path() {
            Some(path) => Self::load_from(&path),
            None => Self::default(),
        }
    }

    /// The state in `path`; a missing or unreadable file is an empty state.
    pub fn load_from(path: &std::path::Path) -> Self {
        let raw = std::fs::read_to_string(path).unwrap_or_default();
        let mut state: State = serde_json::from_str(&raw).unwrap_or_default();
        if state.version == 0 {
            state.version = 1;
        }
        state
    }

    /// Fold in what a learner changed: `before` is the snapshot it started from,
    /// `after` is where it ended. Per-host entries it changed replace ours, entries
    /// it removed are removed, entries it left alone keep whatever we learned
    /// meanwhile; vendor priors add its increments. Two learners on one host race
    /// as two processes on one state file do: the later write wins.
    pub fn absorb(&mut self, before: &State, after: &State) {
        fold_shared(&mut self.origins, &before.origins, &after.origins);
        fold_shared(&mut self.cookies, &before.cookies, &after.cookies);
        fold(
            &mut self.interface_calls,
            &before.interface_calls,
            &after.interface_calls,
        );
        fold(&mut self.cooldowns, &before.cooldowns, &after.cooldowns);
        fold_shared(
            &mut self.credentials,
            &before.credentials,
            &after.credentials,
        );
        fold_shared(&mut self.mints, &before.mints, &after.mints);
        for (vendor, counts) in &after.vendor_priors {
            for (identity, count) in counts {
                let was = before
                    .vendor_priors
                    .get(vendor)
                    .and_then(|seen| seen.get(identity))
                    .copied()
                    .unwrap_or(0);
                if *count > was {
                    *self
                        .vendor_priors
                        .entry(vendor.clone())
                        .or_default()
                        .entry(identity.clone())
                        .or_default() += count - was;
                }
            }
        }
    }

    /// This host's entry, created when missing, writable (copied first when
    /// another state still shares it).
    pub fn origin_mut(&mut self, host: &str) -> &mut OriginState {
        Arc::make_mut(self.origins.entry(host.to_string()).or_default())
    }

    /// This host's entry, writable, when it exists.
    pub fn origin_get_mut(&mut self, host: &str) -> Option<&mut OriginState> {
        self.origins.get_mut(host).map(Arc::make_mut)
    }

    /// Identities that have passed `vendor`, best first: what this state has
    /// seen pass it, then the measured seed ([`VENDOR_SEED`]).
    pub fn vendor_order(&self, vendor: &str) -> Vec<String> {
        let mut learned: Vec<(String, u32)> = self
            .vendor_priors
            .get(vendor)
            .map(|scores| {
                scores
                    .iter()
                    .map(|(name, ok)| (name.clone(), *ok))
                    .collect()
            })
            .unwrap_or_default();
        learned.sort_by_key(|entry| std::cmp::Reverse(entry.1));
        let mut ordered: Vec<String> = learned.into_iter().map(|(name, _)| name).collect();
        let seed = VENDOR_SEED
            .iter()
            .find(|(name, _)| *name == vendor)
            .map(|(_, identities)| *identities)
            .unwrap_or(&[]);
        for name in seed {
            if !ordered.iter().any(|known| known == name) {
                ordered.push((*name).to_string());
            }
        }
        ordered
    }

    /// Identities for this host in the order they should be tried: the stored
    /// winner, then this origin's proven identities, then the vendor prior, then
    /// the portfolio order.
    pub fn order(&self, host: &str, prior: &[&str]) -> Vec<String> {
        let mut ordered: Vec<String> = Vec::new();
        let stored = self.origins.get(host);
        if let Some(chosen) = stored.and_then(|state| state.chosen.clone()) {
            ordered.push(chosen);
        }
        // A host met for the first time starts from its site: what a sibling
        // host of the same site (`www.` and `m.` of one domain, one CDN edge for
        // all its subdomains) last let through. Ordering only: cookies and
        // credentials stay bound to the host that earned them.
        let sibling = if stored.is_none_or(|state| state.scores.is_empty()) {
            self.sibling(host)
        } else {
            None
        };
        if let Some(chosen) = sibling.and_then(|state| state.chosen.clone()) {
            if !ordered.contains(&chosen) {
                ordered.push(chosen);
            }
        }
        let vendor = stored
            .and_then(|state| state.wall_vendor.clone())
            .or_else(|| sibling.and_then(|state| state.wall_vendor.clone()));
        if let Some(vendor) = vendor {
            for name in self.vendor_order(&vendor) {
                if !ordered.contains(&name) {
                    ordered.push(name);
                }
            }
        }
        let mut scored: Vec<(String, (u32, u32, i64))> = stored
            .map(|state| {
                state
                    .scores
                    .iter()
                    .filter(|(_, score)| score.attempts() > 0)
                    .map(|(name, score)| (name.clone(), score.rank()))
                    .collect()
            })
            .unwrap_or_default();
        scored.sort_by_key(|entry| entry.1);
        // Proven identities now; ones that have only failed here go last, after
        // every identity not yet tried: a failure is not repeated while an
        // untried identity could answer.
        let mut failed_only: Vec<String> = Vec::new();
        for (name, rank) in scored {
            if rank.0 == 0 {
                if !ordered.contains(&name) {
                    ordered.push(name);
                }
            } else {
                failed_only.push(name);
            }
        }
        // What has worked anywhere, most often first: a prior learned from
        // outcomes, not a belief about browsers.
        let mut global: BTreeMap<String, u32> = BTreeMap::new();
        for origin in self.origins.values() {
            for (name, score) in &origin.scores {
                *global.entry(name.clone()).or_insert(0) += score.ok;
            }
        }
        let mut global: Vec<(String, u32)> = global.into_iter().filter(|(_, ok)| *ok > 0).collect();
        global.sort_by_key(|entry| std::cmp::Reverse(entry.1));
        for (name, _) in global {
            if !ordered.iter().any(|known| known == &name) {
                ordered.push(name);
            }
        }
        for name in prior {
            if !ordered.iter().any(|known| known == name)
                && !failed_only.iter().any(|failed| failed == name)
            {
                ordered.push((*name).to_string());
            }
        }
        for name in failed_only {
            if !ordered.contains(&name) {
                ordered.push(name);
            }
        }
        ordered
    }

    /// The most recently used other host of `host`'s site that has learned
    /// something: a winner or a named wall.
    fn sibling(&self, host: &str) -> Option<&OriginState> {
        let site = site_of(host);
        let suffix = format!(".{site}");
        self.origins
            .iter()
            .filter(|(other, state)| {
                other.as_str() != host
                    && (other.as_str() == site || other.ends_with(&suffix))
                    && (state.chosen.is_some() || state.wall_vendor.is_some())
            })
            .max_by_key(|(_, state)| state.last_ms.max(state.updated_unix * 1000))
            .map(|(_, state)| &**state)
    }

    // ----- engine credentials: lifetime, renewal, and the cost of minting -------

    /// May the engine mint for this host now? `Err` names why not, so the outcome
    /// can say it instead of pretending the engine ran.
    pub fn engine_gate(&self, host: &str, now: i64) -> Result<(), String> {
        let Some(ledger) = self.mints.get(host) else {
            return Ok(());
        };
        if ledger.blocked_until > now {
            return Err(format!(
                "backing off after {} refused mint(s), next in {} s{}",
                ledger.failures,
                ledger.blocked_until - now,
                ledger
                    .last_error
                    .as_deref()
                    .map(|error| format!(" ({error})"))
                    .unwrap_or_default()
            ));
        }
        let recent: Vec<i64> = ledger
            .recent
            .iter()
            .copied()
            .filter(|at| now - at < MINT_WINDOW_SECS)
            .collect();
        if recent.len() >= MINT_CAP {
            let oldest = recent.iter().copied().min().unwrap_or(now);
            return Err(format!(
                "mint cap of {MINT_CAP} per {MINT_WINDOW_SECS} s reached, next in {} s",
                (oldest + MINT_WINDOW_SECS - now).max(1)
            ));
        }
        Ok(())
    }

    /// Record a mint: the credential it produced (stored by the caller through
    /// `store_cookies`) and its place in the host's mint budget.
    pub fn mint_recorded(
        &mut self,
        host: &str,
        identity: &str,
        vendor: &str,
        cookies: &[StoredCookie],
        now: i64,
    ) {
        let declared_ttl = cookies
            .iter()
            .filter_map(|cookie| cookie.expires_unix)
            .map(|at| at - now)
            .filter(|ttl| *ttl > 0)
            .min();
        Arc::make_mut(self.credentials.entry(host.to_string()).or_default()).insert(
            identity.to_string(),
            Credential {
                vendor: vendor.to_string(),
                minted_unix: now,
                names: cookies.iter().map(|cookie| cookie.name.clone()).collect(),
                declared_ttl,
                uses: 0,
            },
        );
        let ledger = Arc::make_mut(self.mints.entry(host.to_string()).or_default());
        ledger.recent.retain(|at| now - at < MINT_WINDOW_SECS);
        ledger.recent.push(now);
    }

    /// The origin accepted a minted credential's request after a mint: the host's
    /// failure streak ends.
    pub fn mint_succeeded(&mut self, host: &str) {
        if let Some(ledger) = self.mints.get_mut(host).map(Arc::make_mut) {
            ledger.failures = 0;
            ledger.blocked_until = 0;
            ledger.last_error = None;
        }
    }

    /// The mint produced nothing the origin accepts (or did not finish). The
    /// credential, if any, is dropped and the next mint waits.
    pub fn mint_failed(&mut self, host: &str, identity: &str, reason: &str, now: i64) {
        self.drop_credential(host, identity);
        let ledger = Arc::make_mut(self.mints.entry(host.to_string()).or_default());
        ledger.failures = ledger.failures.saturating_add(1);
        let doubling = 1i64 << (ledger.failures - 1).min(10);
        ledger.blocked_until = now + (BACKOFF_BASE_SECS * doubling).min(BACKOFF_MAX_SECS);
        ledger.last_error = Some(reason.to_string());
    }

    /// Age of the held credential in seconds, if there is one.
    pub fn credential_age(&self, host: &str, identity: &str, now: i64) -> Option<i64> {
        self.credentials
            .get(host)?
            .get(identity)
            .map(|credential| (now - credential.minted_unix).max(0))
    }

    #[cfg(test)]
    pub fn has_credential(&self, host: &str, identity: &str) -> bool {
        self.credentials
            .get(host)
            .is_some_and(|per_identity| per_identity.contains_key(identity))
    }

    /// Remove a credential and the cookies it minted.
    pub fn drop_credential(&mut self, host: &str, identity: &str) {
        let removed = self
            .credentials
            .get_mut(host)
            .map(Arc::make_mut)
            .and_then(|per_identity| per_identity.remove(identity));
        if let Some(credential) = removed {
            if let Some(cookies) = self
                .cookies
                .get_mut(host)
                .map(Arc::make_mut)
                .and_then(|per_identity| per_identity.get_mut(identity))
            {
                cookies.retain(|cookie| !credential.names.contains(&cookie.name));
            }
        }
    }

    /// Renew before a refusal rather than after one, once the host has taught us
    /// where its refusals start. Returns true when the credential was dropped.
    pub fn expire_stale_credential(&mut self, host: &str, identity: &str, now: i64) -> bool {
        let Some(age) = self.credential_age(host, identity, now) else {
            return false;
        };
        let declared = self
            .credentials
            .get(host)
            .and_then(|per_identity| per_identity.get(identity))
            .and_then(|credential| credential.declared_ttl);
        let learned = self.mints.get(host).and_then(|ledger| ledger.refused_age);
        let stale = learned.is_some_and(|refused| age as f64 >= refused as f64 * REFRESH_RATIO)
            || declared.is_some_and(|ttl| age >= ttl);
        if stale {
            self.drop_credential(host, identity);
        }
        stale
    }

    /// The origin's verdict on a request that spent the held credential.
    ///
    /// Accepted: the window is at least this old. Refused after having worked: the
    /// window ends by this age, and the credential is dropped. Refused without ever
    /// having worked is not a lifetime, it is a failed mint: nothing is learned
    /// about the window and the caller records the failure.
    pub fn credential_verdict(&mut self, host: &str, identity: &str, accepted: bool, now: i64) {
        let Some(age) = self.credential_age(host, identity, now) else {
            return;
        };
        if accepted {
            if let Some(credential) = self
                .credentials
                .get_mut(host)
                .map(Arc::make_mut)
                .and_then(|per_identity| per_identity.get_mut(identity))
            {
                credential.uses += 1;
            }
            let ledger = Arc::make_mut(self.mints.entry(host.to_string()).or_default());
            ledger.accepted_age = ledger.accepted_age.max(age);
            return;
        }
        let had_worked = self
            .credentials
            .get(host)
            .and_then(|per_identity| per_identity.get(identity))
            .is_some_and(|credential| credential.uses > 0);
        if had_worked {
            let ledger = Arc::make_mut(self.mints.entry(host.to_string()).or_default());
            ledger.refused_age = Some(ledger.refused_age.map_or(age, |known| known.min(age)));
        }
        self.drop_credential(host, identity);
    }

    /// Wait long enough that this origin is not asked twice in a burst.
    /// Returns the milliseconds slept; the caller counts them against its deadline.
    pub fn pace(&mut self, host: &str, minimum_ms: i64) -> i64 {
        let now = now_ms();
        let last = self
            .origins
            .get(host)
            .map(|state| state.last_ms)
            .unwrap_or(0);
        let wait = (last + minimum_ms - now).max(0);
        if let Some(state) = self.origins.get_mut(host).map(Arc::make_mut) {
            state.last_ms = now + wait;
        } else {
            Arc::make_mut(self.origins.entry(host.to_string()).or_default()).last_ms = now + wait;
        }
        if wait > 0 {
            std::thread::sleep(std::time::Duration::from_millis(wait as u64));
        }
        wait
    }

    /// Time still owed to an interface's minimum interval, without sleeping.
    pub fn pace_debt(&self, host: &str, minimum_ms: i64) -> i64 {
        if minimum_ms <= 0 {
            return 0;
        }
        let now = now_ms();
        let last = self.interface_calls.get(host).copied().unwrap_or(0);
        (last + minimum_ms - now).max(0)
    }

    pub fn seen_origin(&self, host: &str) -> bool {
        self.origins
            .get(host)
            .map(|state| state.visited > 0)
            .unwrap_or(false)
    }

    pub fn mark_seen(&mut self, host: &str) {
        Arc::make_mut(self.origins.entry(host.to_string()).or_default()).visited += 1;
    }

    pub fn mark_call(&mut self, host: &str) {
        let now = now_ms();
        self.interface_calls.insert(host.to_string(), now);
    }

    pub fn cooling(&self, host: &str) -> Option<i64> {
        let until = *self.cooldowns.get(host)?;
        let remaining = until - now_ms();
        (remaining > 0).then_some(remaining)
    }

    pub fn set_cooldown(&mut self, host: &str, ms: i64) {
        let until = now_ms() + ms.max(0);
        self.cooldowns.insert(host.to_string(), until);
    }

    pub fn record(&mut self, host: &str, identity: &str, class: &str, ok: bool, ms: f64) {
        let origin = Arc::make_mut(self.origins.entry(host.to_string()).or_default());
        let score = origin.scores.entry(identity.to_string()).or_default();
        // Recent outcomes outweigh old ones: past SCORE_WINDOW attempts every
        // count halves, so a site that changed its mind is believed within weeks
        // of use, not never.
        if score.attempts() >= SCORE_WINDOW {
            score.ok /= 2;
            score.fail /= 2;
            score.total_ms /= 2.0;
        }
        if ok {
            score.ok += 1;
            origin.chosen = Some(identity.to_string());
            origin.wall_class = None;
            origin.evidence.clear();
            // The wall this origin last showed is what this identity got past:
            // that is the cross-origin prior a cold origin behind the same vendor
            // starts from. Recorded on success only, never assumed.
            if let Some(vendor) = origin.wall_vendor.take() {
                *self
                    .vendor_priors
                    .entry(vendor)
                    .or_default()
                    .entry(identity.to_string())
                    .or_insert(0) += 1;
            }
        } else {
            score.fail += 1;
        }
        score.total_ms += ms;
        score.last_class = Some(class.to_string());
        score.last_ms = ms;
        origin.updated_unix = now_unix();
    }

    /// The wall `identity` (when known) was shown on `host`.
    pub fn record_wall(
        &mut self,
        host: &str,
        identity: Option<&str>,
        class: &str,
        vendor: Option<&str>,
        evidence: &[String],
    ) {
        let origin = Arc::make_mut(self.origins.entry(host.to_string()).or_default());
        origin.wall_class = Some(class.to_string());
        origin.wall_vendor = vendor.map(str::to_string);
        // A vendor that walls an identity it used to let through is believed:
        // each refusal takes a quarter of that identity's credit with the vendor,
        // so a few fresh refusals outweigh an old run of passes.
        if let (Some(vendor), Some(identity)) = (vendor, identity) {
            if let Some(counts) = self.vendor_priors.get_mut(vendor) {
                if let Some(count) = counts.get_mut(identity) {
                    *count -= (*count).div_ceil(4);
                    if *count == 0 {
                        counts.remove(identity);
                    }
                }
                if counts.is_empty() {
                    self.vendor_priors.remove(vendor);
                }
            }
        }
        origin.evidence = evidence.to_vec();
        origin.updated_unix = now_unix();
    }

    pub fn store_cookies(&mut self, host: &str, identity: &str, cookies: Vec<StoredCookie>) {
        if cookies.is_empty() {
            return;
        }
        let per_identity = Arc::make_mut(self.cookies.entry(host.to_string()).or_default())
            .entry(identity.to_string())
            .or_default();
        for cookie in cookies {
            match per_identity
                .iter_mut()
                .find(|known| known.name == cookie.name && known.path == cookie.path)
            {
                Some(slot) => *slot = cookie,
                None => per_identity.push(cookie),
            }
        }
        // Drop expired entries so a stale token is never presented.
        let now = now_unix();
        per_identity.retain(|cookie| cookie.expires_unix.map(|at| at > now).unwrap_or(true));
    }

    /// Cookies this identity may spend on this host right now.
    pub fn spendable(&self, host: &str, identity: &str) -> Vec<StoredCookie> {
        let now = now_unix();
        self.cookies
            .get(host)
            .and_then(|per_identity| per_identity.get(identity))
            .map(|cookies| {
                cookies
                    .iter()
                    .filter(|cookie| cookie.expires_unix.map(|at| at > now).unwrap_or(true))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn cookie_header(&self, host: &str, identity: &str) -> Option<String> {
        join_cookies(self.spendable(host, identity))
    }

    /// The `Cookie` header for one request to `url`: this identity's cookies
    /// for its host, never a `Secure` one over plain HTTP, and only those whose
    /// `Path` covers the request's path (RFC 6265 §5.1.4).
    pub fn cookie_header_for(&self, url: &str, identity: &str) -> Option<String> {
        let parsed = url::Url::parse(url).ok()?;
        let host = parsed.host_str()?.to_ascii_lowercase();
        let secure = parsed.scheme() == "https";
        let path = parsed.path();
        join_cookies(
            self.spendable(&host, identity)
                .into_iter()
                .filter(|cookie| secure || !cookie.secure)
                .filter(|cookie| path_matches(cookie.path.as_deref(), path))
                .collect(),
        )
    }
}

/// The site a host belongs to, for ordering priors only: its last two labels,
/// or three under a two-letter country code whose second level is a short
/// generic one (`bbc.co.uk`, `abc.net.au`). An IP address is its own site.
fn site_of(host: &str) -> &str {
    if host.parse::<std::net::IpAddr>().is_ok() || host.starts_with('[') {
        return host;
    }
    let labels: Vec<&str> = host.split('.').collect();
    let keep = match labels.as_slice() {
        [.., second, tld]
            if tld.len() == 2
                && matches!(
                    *second,
                    "co" | "com" | "net" | "org" | "ac" | "gov" | "edu" | "ne" | "or" | "go"
                ) =>
        {
            3
        }
        _ => 2,
    };
    if labels.len() <= keep {
        return host;
    }
    let skip: usize = labels[..labels.len() - keep]
        .iter()
        .map(|label| label.len() + 1)
        .sum();
    &host[skip..]
}

/// RFC 6265 §5.1.4: a cookie path covers the request path when they are equal,
/// or the cookie path is a prefix ending at a `/` boundary. A cookie with no
/// stored path is the site's own default and covers every path here.
fn path_matches(cookie_path: Option<&str>, request_path: &str) -> bool {
    let Some(cookie_path) = cookie_path.filter(|path| path.starts_with('/')) else {
        return true;
    };
    let request_path = if request_path.is_empty() {
        "/"
    } else {
        request_path
    };
    request_path == cookie_path
        || (request_path.starts_with(cookie_path)
            && (cookie_path.ends_with('/')
                || request_path.as_bytes().get(cookie_path.len()) == Some(&b'/')))
}

fn join_cookies(cookies: Vec<StoredCookie>) -> Option<String> {
    if cookies.is_empty() {
        return None;
    }
    Some(
        cookies
            .iter()
            .map(|cookie| format!("{}={}", cookie.name, cookie.value))
            .collect::<Vec<_>>()
            .join("; "),
    )
}

/// Attempts per (origin, identity) before old outcomes start to fade.
pub const SCORE_WINDOW: u32 = 32;
/// Origins remembered; the least recently used are forgotten first, with their
/// cookies, credentials and mint history.
pub const ORIGINS_MAX: usize = 4096;

impl State {
    /// Drop what can no longer be used: expired cookies and credentials' hosts
    /// that are gone, finished cooldowns, stale interface clocks, and origins past
    /// [`ORIGINS_MAX`].
    pub fn prune(&mut self) {
        let now = now_unix();
        let now_ms = now_ms();
        for per_identity in self.cookies.values_mut() {
            // Only a jar holding an expired cookie is copied to be pruned.
            let stale = per_identity.values().any(|cookies| {
                cookies.is_empty()
                    || cookies
                        .iter()
                        .any(|cookie| cookie.expires_unix.is_some_and(|at| at <= now))
            });
            if !stale {
                continue;
            }
            let per_identity = Arc::make_mut(per_identity);
            for cookies in per_identity.values_mut() {
                cookies.retain(|cookie| cookie.expires_unix.map(|at| at > now).unwrap_or(true));
            }
            per_identity.retain(|_, cookies| !cookies.is_empty());
        }
        self.cookies
            .retain(|_, per_identity| !per_identity.is_empty());
        self.cooldowns.retain(|_, until| *until > now_ms);
        self.interface_calls
            .retain(|_, at| now_ms - *at < 24 * 3600 * 1000);
        if self.origins.len() > ORIGINS_MAX {
            let mut by_use: Vec<(i64, String)> = self
                .origins
                .iter()
                .map(|(host, origin)| {
                    (origin.last_ms.max(origin.updated_unix * 1000), host.clone())
                })
                .collect();
            by_use.sort();
            for (_, host) in by_use.into_iter().take(self.origins.len() - ORIGINS_MAX) {
                self.origins.remove(&host);
                self.cookies.remove(&host);
                self.credentials.remove(&host);
                self.mints.remove(&host);
            }
        }
    }

    /// Write what this state learned since `base` (the disk state it started
    /// from) into the state file, merged with whatever other processes wrote
    /// meanwhile, under a lock. Returns the merged state, the caller's new base.
    pub fn save_merged(&self, base: &State) -> State {
        match state_path() {
            Some(path) => self.save_merged_at(base, &path),
            None => self.clone(),
        }
    }

    /// [`State::save_merged`] into `path`.
    pub fn save_merged_at(&self, base: &State, path: &std::path::Path) -> State {
        self.save_merged_known(base, path, None).0
    }

    /// [`State::save_merged_at`] for a writer that knows which version of the
    /// file `base` is: when the file is still that version (no other process
    /// wrote since), it is not read and parsed again, and the merge is this
    /// state itself. Returns the merged state and the version just written
    /// (`None` when the write failed, so the next save reads the file).
    pub fn save_merged_known(
        &self,
        base: &State,
        path: &std::path::Path,
        known: Option<FileStamp>,
    ) -> (State, Option<FileStamp>) {
        let _lock = FileLock::take(path);
        let mut merged = match known {
            // Folding our changes onto the base they were made from is exactly
            // this state.
            Some(known) if FileStamp::of(path) == Some(known) => self.clone(),
            _ => {
                let mut merged = State::load_from(path);
                merged.absorb(base, self);
                merged
            }
        };
        merged.prune();
        let written = merged.write(path);
        let stamp = if written { FileStamp::of(path) } else { None };
        (merged, stamp)
    }

    fn write(&self, path: &std::path::Path) -> bool {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let Ok(raw) = serde_json::to_vec(self) else {
            return false;
        };
        write_replacing(path, &raw)
    }
}

/// One version of the state file. Every writer renames a freshly written file
/// over it ([`write_replacing`]), so a new version is a new file: its creation
/// time differs (and its inode, on Unix), whatever the clock's resolution.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileStamp {
    len: u64,
    modified: Option<std::time::SystemTime>,
    created: Option<std::time::SystemTime>,
    inode: u64,
}

impl FileStamp {
    pub fn of(path: &std::path::Path) -> Option<Self> {
        let meta = std::fs::metadata(path).ok()?;
        #[cfg(unix)]
        let inode = std::os::unix::fs::MetadataExt::ino(&meta);
        #[cfg(not(unix))]
        let inode = 0;
        Some(Self {
            len: meta.len(),
            modified: meta.modified().ok(),
            created: meta.created().ok(),
            inode,
        })
    }
}

/// Write `bytes` to `path` whole or not at all: a uniquely named sibling is
/// written, then renamed over `path`, so a reader sees the old file or the new
/// one, never a part. False when the file could not be replaced.
pub(crate) fn write_replacing(path: &std::path::Path, bytes: &[u8]) -> bool {
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".tmp{}-{n}", std::process::id()));
    let temporary = path.with_file_name(name);
    if std::fs::write(&temporary, bytes).is_err() {
        let _ = std::fs::remove_file(&temporary);
        return false;
    }
    // Windows refuses a rename onto a file that a virus scanner or the indexer
    // holds open for a moment; the hold is brief, so the rename is retried.
    for attempt in 1..=5u64 {
        if std::fs::rename(&temporary, path).is_ok() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(10 * attempt));
    }
    let _ = std::fs::remove_file(&temporary);
    false
}

/// A lock file beside the state file: `create_new` is atomic on every platform.
/// A lock older than [`LOCK_STALE_MS`] belongs to a process that died and is
/// broken; a lock that cannot be had within the wait is skipped, and the write
/// races as it would have without one.
struct FileLock(Option<PathBuf>);

const LOCK_STALE_MS: u128 = 5_000;

impl FileLock {
    fn take(state: &std::path::Path) -> Self {
        let path = state.with_extension("json.lock");
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut denied = 0;
        for _ in 0..200 {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(_) => return Self(Some(path)),
                // Windows answers a lock file that is being deleted with
                // PermissionDenied, briefly; an unwritable directory answers it
                // for good, and is not waited on.
                Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                    denied += 1;
                    if denied > 10 {
                        return Self(None);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(error) if error.kind() != std::io::ErrorKind::AlreadyExists => {
                    return Self(None)
                }
                Err(_) => {
                    let stale = std::fs::metadata(&path)
                        .and_then(|meta| meta.modified())
                        .ok()
                        .and_then(|at| at.elapsed().ok())
                        .is_some_and(|age| age.as_millis() > LOCK_STALE_MS);
                    if stale {
                        let _ = std::fs::remove_file(&path);
                    } else {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                }
            }
        }
        Self(None)
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// [`fold`] over shared entries: one still the same allocation is unchanged
/// without being compared.
fn fold_shared<V: PartialEq>(
    ours: &mut BTreeMap<String, Shared<V>>,
    before: &BTreeMap<String, Shared<V>>,
    after: &BTreeMap<String, Shared<V>>,
) {
    for (key, value) in after {
        let unchanged = before
            .get(key)
            .is_some_and(|was| Arc::ptr_eq(was, value) || **was == **value);
        if !unchanged {
            ours.insert(key.clone(), Arc::clone(value));
        }
    }
    for key in before.keys() {
        if !after.contains_key(key) {
            ours.remove(key);
        }
    }
}

/// One host-keyed table of `State::absorb`.
fn fold<V: Clone + PartialEq>(
    ours: &mut BTreeMap<String, V>,
    before: &BTreeMap<String, V>,
    after: &BTreeMap<String, V>,
) {
    for (key, value) in after {
        if before.get(key) != Some(value) {
            ours.insert(key.clone(), value.clone());
        }
    }
    for key in before.keys() {
        if !after.contains_key(key) {
            ours.remove(key);
        }
    }
}

pub fn state_path() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("V5_STATE") {
        if explicit.is_empty() {
            return None;
        }
        return Some(PathBuf::from(explicit));
    }
    Some(cache_root()?.join("donsetch").join("v5").join("state.json"))
}

/// The platform's per-user cache directory: `%LOCALAPPDATA%` on Windows,
/// `~/Library/Caches` on macOS, `$XDG_CACHE_HOME` or `~/.cache` elsewhere. An
/// empty variable counts as unset.
fn cache_root() -> Option<PathBuf> {
    let var = |name: &str| {
        std::env::var_os(name)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    };
    if cfg!(windows) {
        return var("LOCALAPPDATA")
            .or_else(|| var("USERPROFILE").map(|home| home.join("AppData").join("Local")));
    }
    if cfg!(target_os = "macos") {
        return var("HOME").map(|home| home.join("Library").join("Caches"));
    }
    var("XDG_CACHE_HOME").or_else(|| var("HOME").map(|home| home.join(".cache")))
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|delta| delta.as_millis() as i64)
        .unwrap_or(0)
}

pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|delta| delta.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stored_winner_first_untried_identities_before_a_failure_repeats() {
        let mut state = State::default();
        state.record("a.test", "chrome", "content", true, 100.0);
        state.record("a.test", "plain", "wall_identity", false, 90.0);
        assert_eq!(
            state.order("a.test", &["plain", "curl", "chrome"]),
            ["chrome", "curl", "plain"]
        );
        // With nothing proven, the untried identity still comes before the failure.
        let mut fresh = State::default();
        fresh.record("b.test", "plain", "incomplete", false, 90.0);
        assert_eq!(
            fresh.order("b.test", &["plain", "chrome"]),
            ["chrome", "plain"]
        );
    }

    #[test]
    fn old_outcomes_fade_so_a_site_that_changed_is_believed() {
        let mut state = State::default();
        for _ in 0..40 {
            state.record("c.test", "chrome", "content", true, 100.0);
        }
        let score = &state.origins["c.test"].scores["chrome"];
        assert!(score.attempts() < SCORE_WINDOW + 1, "{}", score.attempts());
        assert!(
            (score.avg_ms() - 100.0).abs() < 1.0,
            "fading keeps the average"
        );
    }

    #[test]
    fn the_state_stays_bounded_and_drops_what_cannot_be_used() {
        let mut state = State::default();
        for n in 0..ORIGINS_MAX + 10 {
            let host = format!("h{n}.test");
            state.record(&host, "chrome", "content", true, 1.0);
            state
                .origins
                .get_mut(&host)
                .map(Arc::make_mut)
                .unwrap()
                .last_ms = n as i64;
        }
        state.store_cookies(
            "h0.test",
            "chrome",
            vec![StoredCookie {
                name: "a".into(),
                value: "1".into(),
                domain: None,
                path: None,
                expires_unix: None,
                secure: false,
                http_only: false,
            }],
        );
        state.cooldowns.insert("h5.test".into(), 0);
        state.prune();
        assert_eq!(state.origins.len(), ORIGINS_MAX);
        assert!(
            !state.origins.contains_key("h0.test"),
            "the least recently used origin goes first"
        );
        assert!(!state.cookies.contains_key("h0.test"), "with its cookies");
        assert!(state.cooldowns.is_empty(), "a finished cooldown is dropped");
    }

    #[test]
    fn two_processes_saving_one_file_keep_each_others_learning() {
        let path =
            std::env::temp_dir().join(format!("v5state-{}-{}.json", std::process::id(), now_ms()));
        let base = State::load_from(&path);
        let mut one = base.clone();
        let mut two = base.clone();
        one.record("one.test", "chrome", "content", true, 1.0);
        two.record("two.test", "firefox", "content", true, 1.0);
        let after_one = one.save_merged_at(&base, &path);
        let after_two = two.save_merged_at(&base, &path);
        assert!(after_one.origins.contains_key("one.test"));
        assert!(
            after_two.origins.contains_key("one.test")
                && after_two.origins.contains_key("two.test")
        );
        let on_disk = State::load_from(&path);
        assert_eq!(on_disk.origins.len(), 2);
        assert!(
            !path.with_extension("json.lock").exists(),
            "the lock is released"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn passing_a_named_wall_teaches_the_vendor_prior() {
        // The encounter records the wall, then the identity that got past it:
        // the prior must survive the success clearing the origin's wall.
        let mut state = State::default();
        state.record_wall(
            "a.test",
            Some("plain"),
            "wall_program",
            Some("cloudflare"),
            &[],
        );
        state.record("a.test", "plain", "wall_program", false, 300.0);
        state.record("a.test", "okhttp", "content", true, 600.0);
        assert_eq!(state.vendor_priors["cloudflare"]["okhttp"], 1);
        assert_eq!(state.origins["a.test"].wall_vendor, None);
        // No wall seen, nothing credited.
        state.record("b.test", "chrome", "content", true, 300.0);
        assert_eq!(state.vendor_priors.len(), 1);
        // A cold origin behind the same vendor now starts with what passed it.
        state.record_wall("c.test", None, "wall_program", Some("cloudflare"), &[]);
        assert_eq!(
            state.order("c.test", &["plain", "chrome", "okhttp"])[0],
            "okhttp"
        );
        // The vendor changes its mind: refusals wear the credit down to nothing.
        for _ in 0..4 {
            *state
                .vendor_priors
                .get_mut("cloudflare")
                .unwrap()
                .get_mut("okhttp")
                .unwrap() += 1;
        }
        state.record_wall(
            "c.test",
            Some("okhttp"),
            "wall_program",
            Some("cloudflare"),
            &[],
        );
        assert_eq!(state.vendor_priors["cloudflare"]["okhttp"], 3);
        for _ in 0..3 {
            state.record_wall(
                "c.test",
                Some("okhttp"),
                "wall_program",
                Some("cloudflare"),
                &[],
            );
        }
        assert!(!state.vendor_priors.contains_key("cloudflare"));
    }

    /// A state at the origin cap, shaped like a real one: scores for a few
    /// identities, a wall on some, cookies on most.
    pub(crate) fn full_state() -> State {
        let mut state = State::default();
        for n in 0..ORIGINS_MAX {
            let host = format!("www.site{n}.test");
            for identity in ["plain", "chrome", "chrome-local"] {
                state.record(&host, identity, "content", n % 3 != 0, 420.0);
            }
            if n % 5 == 0 {
                state.record_wall(
                    &host,
                    Some("plain"),
                    "wall_program",
                    Some("cloudflare"),
                    &["cf-mitigated".into()],
                );
            }
            state
                .origins
                .get_mut(&host)
                .map(Arc::make_mut)
                .unwrap()
                .last_ms = now_ms() - n as i64;
            state.store_cookies(
                &host,
                "chrome",
                (0..4)
                    .map(|c| StoredCookie {
                        name: format!("cookie{c}"),
                        value: "v".repeat(48),
                        domain: Some(host.clone()),
                        path: Some("/".into()),
                        expires_unix: Some(now_unix() + 86_400),
                        secure: true,
                        http_only: c % 2 == 0,
                    })
                    .collect(),
            );
        }
        state
    }

    #[test]
    #[ignore = "a measurement, not a check: cargo test --release -- --ignored save_cost --nocapture"]
    fn save_cost_at_the_origin_cap() {
        let dir = std::env::temp_dir().join(format!("v5-save-cost-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("state.json");
        let base = full_state();
        let base = base.save_merged_at(&State::default(), &path);
        let bytes = std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
        let mut ours = base.clone();
        let rounds = 20;
        let started = std::time::Instant::now();
        let mut current = base;
        let mut stamp = FileStamp::of(&path);
        for round in 0..rounds {
            let before = ours.clone();
            let mut after = before.clone();
            after.record(
                "www.site7.test",
                "chrome",
                "content",
                true,
                300.0 + round as f64,
            );
            ours.absorb(&before, &after);
            (current, stamp) = ours.save_merged_known(&current, &path, stamp);
            ours = current.clone();
        }
        let time = |label: &str, job: &mut dyn FnMut()| {
            let at = std::time::Instant::now();
            for _ in 0..5 {
                job();
            }
            eprintln!("  {label}: {:.1} ms", at.elapsed().as_secs_f64() * 200.0);
        };
        let probe = current.clone();
        time("clone", &mut || drop(probe.clone()));
        let mut folded = probe.clone();
        time("absorb", &mut || folded.absorb(&probe, &probe));
        let mut pruned = probe.clone();
        time("prune", &mut || pruned.prune());
        time("serialize", &mut || drop(serde_json::to_vec(&probe)));
        let raw = serde_json::to_vec(&probe).unwrap();
        time("write", &mut || {
            let _ = write_replacing(&path, &raw);
        });
        time("parse", &mut || drop(State::load_from(&path)));
        time("stamp", &mut || {
            let _ = FileStamp::of(&path);
        });
        eprintln!(
            "state file {} KB, {:.1} ms per learning fetch (clone, absorb, merge, write)",
            bytes / 1024,
            started.elapsed().as_secs_f64() * 1000.0 / rounds as f64
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_writer_that_knows_the_file_still_merges_another_processes_write() {
        let dir = std::env::temp_dir().join(format!("v5-known-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("state.json");
        let _ = std::fs::remove_file(&path);
        // Process A writes, and knows the version it wrote.
        let mut a = State::default();
        a.record("a.test", "chrome", "content", true, 100.0);
        let (base_a, stamp_a) = a.save_merged_known(&State::default(), &path, None);
        assert!(stamp_a.is_some());
        // Process B writes in between.
        let mut b = State::load_from(&path);
        let base_b = b.clone();
        b.record("b.test", "okhttp", "content", true, 100.0);
        b.save_merged_at(&base_b, &path);
        // A saves again on its stale stamp: B's learning survives.
        let mut a2 = base_a.clone();
        a2.record("c.test", "plain", "content", true, 100.0);
        let (merged, _) = a2.save_merged_known(&base_a, &path, stamp_a);
        for host in ["a.test", "b.test", "c.test"] {
            assert!(merged.origins.contains_key(host), "{host}");
        }
        assert_eq!(State::load_from(&path).origins.len(), 3);
        // Unchanged since A's last write: the merge is A's state, and it is on disk.
        let (merged, stamp) =
            merged.save_merged_known(&State::default(), &path, FileStamp::of(&path));
        assert_eq!(merged.origins.len(), 3);
        assert_eq!(stamp, FileStamp::of(&path));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_new_host_starts_from_what_its_site_learned() {
        assert_eq!(site_of("www.example.com"), "example.com");
        assert_eq!(site_of("example.com"), "example.com");
        assert_eq!(site_of("news.bbc.co.uk"), "bbc.co.uk");
        assert_eq!(site_of("a.b.github.io"), "github.io");
        assert_eq!(site_of("10.0.0.1"), "10.0.0.1");
        assert_eq!(site_of("localhost"), "localhost");
        let mut state = State::default();
        state.record_wall(
            "www.shop.test",
            Some("plain"),
            "wall_program",
            Some("cloudflare"),
            &[],
        );
        state.record("www.shop.test", "plain", "wall_program", false, 300.0);
        state.record("www.shop.test", "okhttp", "content", true, 600.0);
        // Elsewhere, chrome has passed most often: the global prior.
        for host in ["a.test", "b.test"] {
            state.record(host, "chrome", "content", true, 300.0);
        }
        let prior = ["plain", "chrome", "okhttp"];
        assert_eq!(state.order("m.shop.test", &prior)[0], "okhttp");
        // Another site learns nothing from it: the global prior leads there.
        assert_eq!(state.order("www.other.test", &prior)[0], "chrome");
        // A host's own history outranks its site's.
        state.record("m.shop.test", "chrome", "content", true, 300.0);
        assert_eq!(state.order("m.shop.test", &prior)[0], "chrome");
        // Cookies never cross.
        state.store_cookies(
            "www.shop.test",
            "okhttp",
            vec![StoredCookie {
                name: "s".into(),
                value: "1".into(),
                domain: None,
                path: None,
                expires_unix: None,
                secure: false,
                http_only: false,
            }],
        );
        assert!(state.cookie_header("m.shop.test", "okhttp").is_none());
    }

    #[test]
    fn a_vendor_prior_lifts_the_identity_that_worked_there() {
        let mut state = State::default();
        state.record_wall("b.test", None, "wall_program", Some("cloudflare"), &[]);
        *state
            .vendor_priors
            .entry("cloudflare".to_string())
            .or_default()
            .entry("chrome".to_string())
            .or_insert(0) += 3;
        let order = state.order("b.test", &["plain", "curl", "chrome"]);
        assert_eq!(order[0], "chrome");
    }

    #[test]
    fn cookies_are_bound_to_host_and_identity() {
        let mut state = State::default();
        state.store_cookies(
            "c.test",
            "plain",
            vec![StoredCookie {
                name: "cf_clearance".into(),
                value: "abc".into(),
                domain: None,
                path: None,
                expires_unix: None,
                secure: true,
                http_only: true,
            }],
        );
        assert!(state.cookie_header("c.test", "plain").is_some());
        assert!(state.cookie_header("c.test", "chrome").is_none());
        assert!(state.cookie_header("d.test", "plain").is_none());
    }

    #[test]
    fn expired_state_is_never_presented() {
        let mut state = State::default();
        state.store_cookies(
            "e.test",
            "plain",
            vec![StoredCookie {
                name: "stale".into(),
                value: "x".into(),
                domain: None,
                path: None,
                expires_unix: Some(1),
                secure: false,
                http_only: false,
            }],
        );
        assert!(state.cookie_header("e.test", "plain").is_none());
    }
}
