//! Request coalescing, cache, timeouts, stale discard. Owner: task B.
//!
//! The scheduler is a sans-IO state machine: callers pass in the current time,
//! receive [`FetchOrder`]s to run (on threads, in the worker) and report results
//! back with [`Scheduler::complete`]. It never blocks and never spawns anything,
//! so tests drive it with synthetic `Instant`s.
//!
//! Policy (see docs/decisions/0004-worker-lifecycle.md):
//!
//! * Cache key = (provider key, scope, env revision). Provider keys are unique
//!   in a provider set, so entries survive [`Scheduler::reconfigure`] (a config
//!   reload) for providers that keep their key. Scope is the request cwd for
//!   providers that depend on `Cwd` (or the worktree id once a result for that
//!   cwd reported one), `Global` otherwise. The env revision is part of the key
//!   only for providers that depend on `Env`. A cwd/env change therefore never
//!   reuses an entry made for another cwd/env.
//! * Providers that depend on `Cwd` or `Time` observe state that can change with
//!   no visible input change (files, clock), so every new request revalidates
//!   them: the cached value is shown immediately (`Fresh` while younger than the
//!   TTL, `Stale` after) and a new fetch is scheduled. Other providers are pure
//!   functions of their key and are served from cache while younger than the TTL.
//! * Coalescing: at most one fetch per (provider, key) is in flight. A pure
//!   provider joins it; a revalidating provider queues one rerun that starts when
//!   it finishes (a fetch that started before the request may predate the
//!   command that just ran). At most `max_inflight_per_provider` fetches of one
//!   provider run at once; beyond that only the latest request's fetch is queued
//!   and older queued ones are dropped.
//! * Outer deadline: `spec.timeout + outer_grace`. Past it the fetch is
//!   abandoned (not counted against the in-flight cap), the current request's
//!   slot becomes `Timeout`, and a late result still lands in the cache. Too many
//!   abandoned fetches mark the provider unresponsive (`Error`).
//! * Only the latest submitted request is "current"; results for older requests
//!   only update the cache, so nothing is ever rendered for an old generation.
//! * Provider indices (in [`FetchOrder`], [`SlotView`]) refer to the spec list
//!   last given to `new`/`reconfigure`; completions are matched by fetch id.

use crate::model::{Dependency, Freshness, PromptRequest, ProviderSpec};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub type FetchId = u64;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Scope {
    Global,
    Cwd(PathBuf),
    Worktree(String),
}

impl std::fmt::Display for Scope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Scope::Global => f.write_str("global"),
            Scope::Cwd(p) => write!(f, "cwd:{}", p.display()),
            Scope::Worktree(w) => write!(f, "worktree:{w}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CacheKey {
    /// `ProviderSpec.key` (unique per provider set).
    pub provider: &'static str,
    pub scope: Scope,
    pub env: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SchedulerConfig {
    /// Concurrent fetches per provider (different keys). One per key always.
    pub max_inflight_per_provider: usize,
    /// Abandoned (past deadline, still running) fetches tolerated per provider
    /// before it is reported unresponsive and no longer started.
    pub max_abandoned_per_provider: usize,
    /// Added to the provider's own timeout to form the outer deadline.
    pub outer_grace: Duration,
    /// Used when a provider declares a zero timeout.
    pub default_timeout: Duration,
    pub max_cache_entries: usize,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        SchedulerConfig {
            max_inflight_per_provider: 2,
            max_abandoned_per_provider: 4,
            outer_grace: Duration::from_millis(250),
            default_timeout: Duration::from_millis(2000),
            max_cache_entries: 256,
        }
    }
}

/// A fetch the caller must run: `providers[provider].fetch(&request)`, then
/// report the value with [`Scheduler::complete`] using `id`.
#[derive(Debug, Clone)]
pub struct FetchOrder {
    pub id: FetchId,
    pub provider: usize,
    pub request: PromptRequest,
}

/// What the current request sees for one provider.
#[derive(Debug, Clone, PartialEq)]
pub struct SlotView<V> {
    pub provider: usize,
    pub key: &'static str,
    pub freshness: Freshness,
    pub age_ms: Option<u64>,
    pub value: Option<V>,
    pub error: Option<String>,
    /// A fetch for the current request is running or queued.
    pub fetching: bool,
    pub scope: Scope,
}

/// Effect of a completion or expiry on the scheduler.
#[derive(Debug, Default)]
pub struct Update {
    pub orders: Vec<FetchOrder>,
    /// Actual executions that crossed their deadline (including old cycles).
    pub timed_out: Vec<(FetchId, &'static str, Duration)>,
    /// Something displayed for the current request changed.
    pub changed_current: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProviderStats {
    pub started: u64,
    pub completed: u64,
    pub cache_hits: u64,
    pub coalesced: u64,
    pub timeouts: u64,
    pub failed: u64,
    pub late: u64,
    pub dropped_queued: u64,
    pub abandoned_running: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Need {
    Satisfied,
    Waiting(FetchId),
    Queued,
    TimedOut(FetchId),
    Failed(String),
}

struct Entry<V> {
    value: V,
    started_at: Instant,
    completed_at: Instant,
}

struct Flight {
    id: FetchId,
    key: CacheKey,
    cwd: PathBuf,
    started_at: Instant,
    deadline: Instant,
    abandoned: bool,
}

struct ProviderState {
    spec: ProviderSpec,
    flights: Vec<Flight>,
    queued: Option<(CacheKey, PromptRequest)>,
    abandoned: usize,
    worktrees: HashMap<PathBuf, String>,
    stats: ProviderStats,
}

impl ProviderState {
    fn active(&self) -> usize {
        self.flights.iter().filter(|f| !f.abandoned).count()
    }
    fn has_active_key(&self, key: &CacheKey) -> Option<FetchId> {
        self.flights.iter().find(|f| !f.abandoned && &f.key == key).map(|f| f.id)
    }
    fn revalidates(&self) -> bool {
        self.spec.deps.iter().any(|d| matches!(d, Dependency::Cwd | Dependency::Time))
    }
    fn depends(&self, d: Dependency) -> bool {
        self.spec.deps.contains(&d)
    }
}

struct Current {
    seq: u64,
    request: PromptRequest,
    needs: Vec<Need>,
}

pub struct Scheduler<V> {
    cfg: SchedulerConfig,
    providers: Vec<ProviderState>,
    cache: HashMap<CacheKey, Entry<V>>,
    current: Option<Current>,
    next_id: FetchId,
    next_seq: u64,
}

const MAX_WORKTREE_MAPPINGS: usize = 1024;

fn new_state(spec: ProviderSpec) -> ProviderState {
    ProviderState {
        spec,
        flights: Vec::new(),
        queued: None,
        abandoned: 0,
        worktrees: HashMap::new(),
        stats: ProviderStats::default(),
    }
}

impl<V: Clone> Scheduler<V> {
    /// `specs` must have unique keys (the cache is keyed by them).
    pub fn new(specs: Vec<ProviderSpec>, cfg: SchedulerConfig) -> Self {
        debug_assert!(unique_keys(&specs), "provider keys must be unique");
        let providers = specs.into_iter().map(new_state).collect();
        Scheduler { cfg, providers, cache: HashMap::new(), current: None, next_id: 1, next_seq: 1 }
    }

    /// Replace the provider set (config reload). A provider whose key is kept
    /// keeps its in-flight fetches, abandon count, worktree map and stats.
    /// Cache entries survive only for kept keys whose spec is unchanged and for
    /// which `retain(key)` is true. There is no current request afterwards:
    /// call [`submit`](Self::submit) next.
    pub fn reconfigure(&mut self, specs: Vec<ProviderSpec>, retain: impl Fn(&str) -> bool) {
        debug_assert!(unique_keys(&specs), "provider keys must be unique");
        let mut old: HashMap<&'static str, ProviderState> =
            std::mem::take(&mut self.providers).into_iter().map(|st| (st.spec.key, st)).collect();
        let mut keep_cache = std::collections::HashSet::new();
        self.providers = specs
            .into_iter()
            .map(|spec| match old.remove(spec.key) {
                Some(mut st) => {
                    if st.spec == spec && retain(spec.key) {
                        keep_cache.insert(spec.key);
                    }
                    st.spec = spec;
                    st.queued = None;
                    st
                }
                None => new_state(spec),
            })
            .collect();
        self.cache.retain(|k, _| keep_cache.contains(k.provider));
        self.current = None;
    }

    /// Index of the provider with this key.
    pub fn index_of(&self, key: &str) -> Option<usize> {
        self.providers.iter().position(|st| st.spec.key == key)
    }

    pub fn provider_count(&self) -> usize {
        self.providers.len()
    }

    pub fn spec(&self, provider: usize) -> &ProviderSpec {
        &self.providers[provider].spec
    }

    pub fn stats(&self, provider: usize) -> ProviderStats {
        let st = &self.providers[provider];
        ProviderStats { abandoned_running: st.abandoned as u64, ..st.stats.clone() }
    }

    pub fn current_request(&self) -> Option<&PromptRequest> {
        self.current.as_ref().map(|c| &c.request)
    }

    /// Monotonic sequence number of the current request (arrival order).
    pub fn current_seq(&self) -> Option<u64> {
        self.current.as_ref().map(|c| c.seq)
    }

    /// Cache key the given request uses for a provider.
    pub fn key_for(&self, provider: usize, req: &PromptRequest) -> CacheKey {
        let st = &self.providers[provider];
        let scope = if st.depends(Dependency::Cwd) {
            match st.worktrees.get(&req.cwd) {
                Some(w) => Scope::Worktree(w.clone()),
                None => Scope::Cwd(req.cwd.clone()),
            }
        } else {
            Scope::Global
        };
        let env = st.depends(Dependency::Env).then(|| req.env.clone());
        CacheKey { provider: st.spec.key, scope, env }
    }

    /// Make `req` the current request; returns fetches to start now. Any request
    /// queued for an older generation is dropped.
    pub fn submit(&mut self, req: PromptRequest, now: Instant) -> Vec<FetchOrder> {
        let seq = self.next_seq;
        self.next_seq += 1;
        let mut orders = Vec::new();
        let mut needs = Vec::with_capacity(self.providers.len());
        for p in 0..self.providers.len() {
            let key = self.key_for(p, &req);
            let ttl = self.providers[p].spec.ttl;
            let cached_fresh = self
                .cache
                .get(&key)
                .map(|e| now.saturating_duration_since(e.completed_at) < ttl)
                .unwrap_or(false);
            let st = &mut self.providers[p];
            if st.queued.take().is_some() {
                st.stats.dropped_queued += 1;
            }
            let reval = st.revalidates();
            let need = if !reval && cached_fresh {
                st.stats.cache_hits += 1;
                Need::Satisfied
            } else if let Some(id) = st.has_active_key(&key) {
                st.stats.coalesced += 1;
                if reval {
                    st.queued = Some((key, req.clone()));
                    Need::Queued
                } else {
                    Need::Waiting(id)
                }
            } else if st.abandoned >= self.cfg.max_abandoned_per_provider {
                Need::Failed(unresponsive(st))
            } else if st.active() < self.cfg.max_inflight_per_provider {
                let order = start_flight(st, &mut self.next_id, &self.cfg, p, key, &req, now);
                let id = order.id;
                orders.push(order);
                Need::Waiting(id)
            } else {
                st.queued = Some((key, req.clone()));
                Need::Queued
            };
            needs.push(need);
        }
        self.current = Some(Current { seq, request: req, needs });
        orders
    }

    /// Revalidate only affected providers without changing generation, request
    /// sequence, other slots, or their in-flight work. External and agent
    /// pushes use this instead of resubmitting the entire prompt.
    pub fn refresh(&mut self, keys: &[&str], now: Instant) -> Vec<FetchOrder> {
        let Some(req) = self.current_request().cloned() else {
            return Vec::new();
        };
        let mut orders = Vec::new();
        for p in 0..self.providers.len() {
            if !keys.contains(&self.providers[p].spec.key) {
                continue;
            }
            let key = self.key_for(p, &req);
            let st = &mut self.providers[p];
            if st.queued.take().is_some() {
                st.stats.dropped_queued += 1;
            }
            let need = if st.has_active_key(&key).is_some() {
                st.stats.coalesced += 1;
                st.queued = Some((key, req.clone()));
                Need::Queued
            } else if st.abandoned >= self.cfg.max_abandoned_per_provider {
                Need::Failed(unresponsive(st))
            } else if st.active() < self.cfg.max_inflight_per_provider {
                let order = start_flight(st, &mut self.next_id, &self.cfg, p, key, &req, now);
                let id = order.id;
                orders.push(order);
                Need::Waiting(id)
            } else {
                st.queued = Some((key, req.clone()));
                Need::Queued
            };
            self.current.as_mut().expect("current request").needs[p] = need;
        }
        orders
    }

    /// Report a finished fetch. `worktree` is the worktree identity the value
    /// revealed (Git), used to share cache entries across cwds of one worktree.
    pub fn complete(&mut self, id: FetchId, value: V, worktree: Option<String>, now: Instant) -> Update {
        let mut update = Update::default();
        let Some((p, idx)) = self.find_flight(id) else {
            return update;
        };
        let st = &mut self.providers[p];
        let flight = st.flights.remove(idx);
        st.stats.completed += 1;
        if flight.abandoned {
            st.abandoned -= 1;
            st.stats.late += 1;
        }
        let mut key = flight.key.clone();
        if st.depends(Dependency::Cwd) {
            if st.worktrees.len() >= MAX_WORKTREE_MAPPINGS {
                st.worktrees.clear();
            }
            key.scope = match worktree {
                Some(w) => {
                    st.worktrees.insert(flight.cwd.clone(), w.clone());
                    Scope::Worktree(w)
                }
                None => {
                    st.worktrees.remove(&flight.cwd);
                    Scope::Cwd(flight.cwd.clone())
                }
            };
        }
        let newer = self.cache.get(&key).map(|e| flight.started_at >= e.started_at).unwrap_or(true);
        if newer {
            self.cache.insert(key.clone(), Entry { value, started_at: flight.started_at, completed_at: now });
            self.evict();
        }
        let cur_key = self.current.as_ref().map(|c| self.key_for(p, &c.request));
        if let Some(cur) = self.current.as_mut() {
            match cur.needs[p] {
                Need::Waiting(w) | Need::TimedOut(w) if w == id => {
                    cur.needs[p] = Need::Satisfied;
                    update.changed_current = true;
                }
                _ => {
                    if newer && cur_key.as_ref() == Some(&key) {
                        update.changed_current = true;
                    }
                }
            }
        }
        if self.try_start_queued(p, now, &mut update.orders) {
            update.changed_current = true;
        }
        update
    }

    /// Report a fetch that produced no value (the provider panicked or its
    /// thread could not be started). The current request's slot becomes `Error`
    /// if it was waiting for this fetch; nothing is cached.
    pub fn fail(&mut self, id: FetchId, error: String, now: Instant) -> Update {
        let mut update = Update::default();
        let Some((p, idx)) = self.find_flight(id) else {
            return update;
        };
        let st = &mut self.providers[p];
        let flight = st.flights.remove(idx);
        st.stats.failed += 1;
        if flight.abandoned {
            st.abandoned -= 1;
            st.stats.late += 1;
        }
        if let Some(cur) = self.current.as_mut() {
            if matches!(cur.needs[p], Need::Waiting(w) | Need::TimedOut(w) if w == id) {
                cur.needs[p] = Need::Failed(error);
                update.changed_current = true;
            }
        }
        if self.try_start_queued(p, now, &mut update.orders) {
            update.changed_current = true;
        }
        update
    }

    /// Abandon fetches past their outer deadline. Call when `next_deadline()`
    /// has passed.
    pub fn expire(&mut self, now: Instant) -> Update {
        let mut update = Update::default();
        for p in 0..self.providers.len() {
            let mut expired = Vec::new();
            let st = &mut self.providers[p];
            for f in st.flights.iter_mut().filter(|f| !f.abandoned && f.deadline <= now) {
                f.abandoned = true;
                expired.push(f.id);
                update.timed_out.push((f.id, st.spec.key, now.saturating_duration_since(f.started_at)));
            }
            if expired.is_empty() {
                continue;
            }
            st.abandoned += expired.len();
            st.stats.timeouts += expired.len() as u64;
            if let Some(cur) = self.current.as_mut() {
                if let Need::Waiting(w) = cur.needs[p] {
                    if expired.contains(&w) {
                        cur.needs[p] = Need::TimedOut(w);
                        update.changed_current = true;
                    }
                }
            }
            if self.try_start_queued(p, now, &mut update.orders) {
                update.changed_current = true;
            }
        }
        update
    }

    /// Earliest outer deadline among running fetches.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.providers.iter().flat_map(|st| st.flights.iter()).filter(|f| !f.abandoned).map(|f| f.deadline).min()
    }

    /// Every slot of the current request reached a final state.
    pub fn is_settled(&self) -> bool {
        self.current
            .as_ref()
            .map(|c| c.needs.iter().all(|n| matches!(n, Need::Satisfied | Need::TimedOut(_) | Need::Failed(_))))
            .unwrap_or(true)
    }

    /// A provider without a `Cwd` dependency (session-global, expected to be
    /// cheap: host, user, SSH) is still running for the current request and has
    /// no cached value at all. The worker briefly waits for these before the
    /// first `partial` so it does not flash a prompt without them.
    pub fn awaiting_uncached_global(&self) -> bool {
        let Some(cur) = &self.current else { return false };
        (0..self.providers.len()).any(|p| {
            matches!(cur.needs[p], Need::Waiting(_) | Need::Queued)
                && !self.providers[p].depends(Dependency::Cwd)
                && !self.cache.contains_key(&self.key_for(p, &cur.request))
        })
    }

    /// Slot views for the current request, one per provider, in provider order.
    pub fn views(&self, now: Instant) -> Vec<SlotView<V>> {
        self.map_views(now, Clone::clone)
    }

    /// State-only views preserve value presence without cloning provider data.
    /// Diagnostics and scheduling observers do not need the cached payload.
    pub fn metadata_views(&self, now: Instant) -> Vec<SlotView<()>> {
        self.map_views(now, |_| ())
    }

    fn map_views<T>(&self, now: Instant, map_value: impl Fn(&V) -> T) -> Vec<SlotView<T>> {
        let Some(cur) = &self.current else { return Vec::new() };
        (0..self.providers.len())
            .map(|p| {
                let st = &self.providers[p];
                let key = self.key_for(p, &cur.request);
                let entry = self.cache.get(&key);
                let age = entry.map(|e| now.saturating_duration_since(e.completed_at));
                let value = entry.map(|e| map_value(&e.value));
                let age_ms = age.map(|a| a.as_millis().min(u64::MAX as u128) as u64);
                let (freshness, error) = match &cur.needs[p] {
                    Need::Satisfied if entry.is_some() => (Freshness::Fresh, None),
                    Need::Satisfied => (Freshness::Pending, None),
                    Need::Waiting(_) | Need::Queued => match age {
                        Some(a) if a < st.spec.ttl => (Freshness::Fresh, None),
                        Some(_) => (Freshness::Stale, None),
                        None => (Freshness::Pending, None),
                    },
                    Need::TimedOut(_) => {
                        let t = effective_timeout(&st.spec, &self.cfg);
                        (Freshness::Timeout, Some(format!("no result within {} ms", t.as_millis())))
                    }
                    Need::Failed(msg) => (Freshness::Error, Some(msg.clone())),
                };
                SlotView {
                    provider: p,
                    key: st.spec.key,
                    freshness,
                    age_ms,
                    value,
                    error,
                    fetching: matches!(cur.needs[p], Need::Waiting(_) | Need::Queued),
                    scope: key.scope,
                }
            })
            .collect()
    }

    fn find_flight(&self, id: FetchId) -> Option<(usize, usize)> {
        self.providers
            .iter()
            .enumerate()
            .find_map(|(p, st)| st.flights.iter().position(|f| f.id == id).map(|i| (p, i)))
    }

    /// Start the queued fetch for `p` if allowed. Returns true if the current
    /// request's need changed to a final state (unresponsive provider).
    fn try_start_queued(&mut self, p: usize, now: Instant, orders: &mut Vec<FetchOrder>) -> bool {
        let st = &mut self.providers[p];
        let Some((key, _)) = &st.queued else { return false };
        if st.abandoned >= self.cfg.max_abandoned_per_provider {
            st.queued = None;
            let msg = unresponsive(st);
            if let Some(cur) = self.current.as_mut() {
                cur.needs[p] = Need::Failed(msg);
                return true;
            }
            return false;
        }
        if st.has_active_key(key).is_some() || st.active() >= self.cfg.max_inflight_per_provider {
            return false;
        }
        let (key, req) = st.queued.take().expect("checked above");
        let order = start_flight(st, &mut self.next_id, &self.cfg, p, key, &req, now);
        if let Some(cur) = self.current.as_mut() {
            cur.needs[p] = Need::Waiting(order.id);
        }
        orders.push(order);
        false
    }

    fn evict(&mut self) {
        while self.cache.len() > self.cfg.max_cache_entries {
            let oldest = self.cache.iter().min_by_key(|(_, e)| e.completed_at).map(|(k, _)| k.clone());
            match oldest {
                Some(k) => {
                    self.cache.remove(&k);
                }
                None => break,
            }
        }
    }
}

fn unique_keys(specs: &[ProviderSpec]) -> bool {
    let mut seen = std::collections::HashSet::new();
    specs.iter().all(|s| seen.insert(s.key))
}

fn unresponsive(st: &ProviderState) -> String {
    format!("provider {} unresponsive ({} fetches past deadline)", st.spec.key, st.abandoned)
}

fn effective_timeout(spec: &ProviderSpec, cfg: &SchedulerConfig) -> Duration {
    if spec.timeout.is_zero() {
        cfg.default_timeout
    } else {
        spec.timeout
    }
}

fn start_flight(
    st: &mut ProviderState,
    next_id: &mut FetchId,
    cfg: &SchedulerConfig,
    provider: usize,
    key: CacheKey,
    req: &PromptRequest,
    now: Instant,
) -> FetchOrder {
    let id = *next_id;
    *next_id += 1;
    let budget = effective_timeout(&st.spec, cfg).saturating_add(cfg.outer_grace);
    let deadline = now.checked_add(budget).unwrap_or_else(|| now + Duration::from_secs(86_400));
    st.flights.push(Flight { id, key, cwd: req.cwd.clone(), started_at: now, deadline, abandoned: false });
    st.stats.started += 1;
    FetchOrder { id, provider, request: req.clone() }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIT_DEPS: &[Dependency] = &[Dependency::Cwd];
    const HOST_DEPS: &[Dependency] = &[Dependency::Env];
    const CLOCK_DEPS: &[Dependency] = &[Dependency::Time];

    fn specs() -> Vec<ProviderSpec> {
        vec![
            ProviderSpec { key: "git", deps: GIT_DEPS, ttl: ms(5000), timeout: ms(1500) },
            ProviderSpec { key: "host", deps: HOST_DEPS, ttl: ms(60_000), timeout: ms(100) },
        ]
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn req(gen: u64, cwd: &str) -> PromptRequest {
        PromptRequest {
            gen,
            session_id: "s".into(),
            cwd: PathBuf::from(cwd),
            cols: 80,
            lines: 24,
            last_exit: 0,
            last_duration_ms: 0,
            env: "e1".into(),
            jobs: 0,
        }
    }

    fn sched() -> Scheduler<String> {
        Scheduler::new(specs(), SchedulerConfig::default())
    }

    fn ids(orders: &[FetchOrder]) -> Vec<(usize, u64)> {
        orders.iter().map(|o| (o.provider, o.request.gen)).collect()
    }

    #[test]
    fn metadata_views_preserve_states_and_presence_without_cloning_payloads() {
        use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};
        struct Counted(Arc<AtomicUsize>);
        impl Clone for Counted {
            fn clone(&self) -> Self {
                self.0.fetch_add(1, Ordering::Relaxed);
                Self(Arc::clone(&self.0))
            }
        }
        fn check(s: &Scheduler<Counted>, now: Instant, count: &AtomicUsize) {
            let before = count.load(Ordering::Relaxed);
            let metadata = s.metadata_views(now);
            assert_eq!(count.load(Ordering::Relaxed), before, "metadata cloned a payload");
            let full: Vec<_> = s.views(now).into_iter().map(|v| SlotView {
                provider: v.provider, key: v.key, freshness: v.freshness,
                age_ms: v.age_ms, value: v.value.map(|_| ()), error: v.error,
                fetching: v.fetching, scope: v.scope,
            }).collect();
            assert_eq!(metadata, full);
            assert_eq!(count.load(Ordering::Relaxed) - before,
                metadata.iter().filter(|v| v.value.is_some()).count());
        }
        let count = Arc::new(AtomicUsize::new(0));
        let mut s = Scheduler::new(vec![specs()[0].clone()], SchedulerConfig::default());
        let now = Instant::now();
        let orders = s.submit(req(1, "/r"), now);
        check(&s, now, &count); // pending
        s.complete(orders[0].id, Counted(Arc::clone(&count)), None, now + ms(1));
        check(&s, now + ms(2), &count); // fresh, settled
        s.submit(req(2, "/r"), now + ms(10));
        check(&s, now + ms(11), &count); // fresh, revalidating
        check(&s, now + ms(6000), &count); // stale, revalidating
        s.expire(now + ms(6000));
        check(&s, now + ms(6000), &count); // timeout with cached value
        let orders = s.submit(req(3, "/other"), now + ms(6100));
        s.fail(orders[0].id, "failed".into(), now + ms(6101));
        check(&s, now + ms(6101), &count); // error without value
    }

    #[test]
    fn cold_request_fetches_everything_and_settles() {
        let t0 = Instant::now();
        let mut s = sched();
        let orders = s.submit(req(1, "/r"), t0);
        assert_eq!(ids(&orders), vec![(0, 1), (1, 1)]);
        assert!(!s.is_settled());
        assert!(s.awaiting_uncached_global());
        let v = s.views(t0);
        assert_eq!(v[0].freshness, Freshness::Pending);
        assert!(v[0].fetching);

        let u = s.complete(orders[1].id, "host-a".into(), None, t0 + ms(1));
        assert!(u.changed_current && u.orders.is_empty());
        assert!(!s.awaiting_uncached_global());
        assert!(!s.is_settled());
        let u = s.complete(orders[0].id, "main".into(), None, t0 + ms(30));
        assert!(u.changed_current);
        assert!(s.is_settled());
        let v = s.views(t0 + ms(40));
        assert_eq!((v[0].freshness, v[0].value.as_deref(), v[0].age_ms), (Freshness::Fresh, Some("main"), Some(10)));
        assert_eq!(v[1].value.as_deref(), Some("host-a"));
    }

    #[test]
    fn pure_provider_served_from_cache_and_revalidating_one_refetched() {
        let t0 = Instant::now();
        let mut s = sched();
        let o = s.submit(req(1, "/r"), t0);
        s.complete(o[0].id, "main".into(), None, t0 + ms(10));
        s.complete(o[1].id, "host".into(), None, t0 + ms(1));
        let o = s.submit(req(2, "/r"), t0 + ms(100));
        assert_eq!(ids(&o), vec![(0, 2)], "host cached, git revalidated");
        assert_eq!(s.stats(1).cache_hits, 1);
        let v = s.views(t0 + ms(100));
        assert_eq!(v[0].freshness, Freshness::Fresh, "cached git shown while revalidating");
        assert!(v[0].fetching);
        assert!(!s.is_settled());
    }

    #[test]
    fn stale_after_ttl_with_refetch_in_flight() {
        let t0 = Instant::now();
        let mut s = sched();
        let o = s.submit(req(1, "/r"), t0);
        s.complete(o[0].id, "main".into(), None, t0 + ms(10));
        s.complete(o[1].id, "host".into(), None, t0);
        let o = s.submit(req(2, "/r"), t0 + ms(6000));
        assert_eq!(o.len(), 1);
        let v = s.views(t0 + ms(6000));
        assert_eq!((v[0].freshness, v[0].value.as_deref()), (Freshness::Stale, Some("main")));
        assert!(v[0].fetching);
        // Host TTL is 60 s; expired pure entries are refetched and shown stale.
        let o = s.submit(req(3, "/r"), t0 + ms(61_000));
        assert_eq!(ids(&o), vec![(1, 3)], "git still in flight (queued rerun), host refetched");
        assert_eq!(s.views(t0 + ms(61_000))[1].freshness, Freshness::Stale);
    }

    #[test]
    fn cwd_and_env_changes_do_not_reuse_entries() {
        let t0 = Instant::now();
        let mut s = sched();
        let o = s.submit(req(1, "/a"), t0);
        s.complete(o[0].id, "a-main".into(), None, t0);
        s.complete(o[1].id, "host-e1".into(), None, t0);
        let _ = s.submit(req(2, "/b"), t0 + ms(1));
        let v = s.views(t0 + ms(1));
        assert_eq!(v[0].freshness, Freshness::Pending, "no git data for /b");
        assert_eq!(v[1].value.as_deref(), Some("host-e1"));
        let mut r = req(3, "/b");
        r.env = "e2".into();
        let o = s.submit(r, t0 + ms(2));
        assert!(o.iter().any(|o| o.provider == 1), "env change refetches host");
        assert_eq!(s.views(t0 + ms(2))[1].freshness, Freshness::Pending);
    }

    #[test]
    fn worktree_scope_is_shared_across_cwds() {
        let t0 = Instant::now();
        let mut s = sched();
        let o = s.submit(req(1, "/repo"), t0);
        s.complete(o[0].id, "main".into(), Some("wt1".into()), t0);
        let o = s.submit(req(2, "/repo/src"), t0 + ms(1));
        s.complete(o[0].id, "main".into(), Some("wt1".into()), t0 + ms(2));
        assert_eq!(s.key_for(0, &req(9, "/repo/src")).scope, Scope::Worktree("wt1".into()));
        // A later result for /repo/src updates the shared entry seen from /repo.
        let o = s.submit(req(3, "/repo/src"), t0 + ms(3));
        s.complete(o[0].id, "main*".into(), Some("wt1".into()), t0 + ms(4));
        let o = s.submit(req(4, "/repo"), t0 + ms(5));
        assert_eq!(s.views(t0 + ms(5))[0].value.as_deref(), Some("main*"));
        // A cwd that stopped being a repo forgets its mapping.
        s.complete(o[0].id, "none".into(), None, t0 + ms(7));
        assert_eq!(s.key_for(0, &req(9, "/repo")).scope, Scope::Cwd("/repo".into()));
    }

    #[test]
    fn revalidating_provider_coalesces_into_one_rerun() {
        let t0 = Instant::now();
        let mut s = sched();
        let first = s.submit(req(1, "/r"), t0);
        let git1 = first[0].id;
        s.complete(first[1].id, "host".into(), None, t0);
        for g in 2..=10 {
            let o = s.submit(req(g, "/r"), t0 + ms(g));
            assert!(o.iter().all(|o| o.provider != 0), "no second git for the same key");
        }
        assert_eq!(s.stats(0).started, 1);
        assert_eq!(s.stats(0).coalesced, 9);
        assert_eq!(s.stats(0).dropped_queued, 8);
        // The old fetch lands: cached, but gen 10 waits for its own rerun.
        let u = s.complete(git1, "old".into(), None, t0 + ms(50));
        assert!(u.changed_current, "cached value changed what gen 10 displays");
        assert_eq!(ids(&u.orders), vec![(0, 10)]);
        assert!(!s.is_settled());
        let u = s.complete(u.orders[0].id, "new".into(), None, t0 + ms(80));
        assert!(u.changed_current);
        assert!(s.is_settled());
        assert_eq!(s.views(t0 + ms(80))[0].value.as_deref(), Some("new"));
    }

    #[test]
    fn pure_provider_joins_in_flight_fetch() {
        let t0 = Instant::now();
        let mut s = sched();
        let o1 = s.submit(req(1, "/r"), t0);
        let o2 = s.submit(req(2, "/r"), t0 + ms(1));
        assert!(o2.iter().all(|o| o.provider != 1));
        assert_eq!(s.stats(1).coalesced, 1);
        let u = s.complete(o1[1].id, "host".into(), None, t0 + ms(2));
        assert!(u.changed_current, "joined fetch satisfies the newer request");
        assert_eq!(s.views(t0 + ms(2))[1].freshness, Freshness::Fresh);
    }

    #[test]
    fn burst_of_cwds_is_bounded_and_serves_only_latest() {
        let t0 = Instant::now();
        let mut s = sched();
        let mut started = Vec::new();
        for g in 1..=10u64 {
            let o = s.submit(req(g, &format!("/d{g}")), t0 + ms(g));
            started.extend(o.into_iter().filter(|o| o.provider == 0));
        }
        assert_eq!(started.iter().map(|o| o.request.gen).collect::<Vec<_>>(), vec![1, 2]);
        assert_eq!(s.current_request().unwrap().gen, 10);
        // Old fetches finishing start only the latest queued request.
        let u = s.complete(started[0].id, "d1".into(), None, t0 + ms(20));
        assert!(!u.changed_current, "result for /d1 does not affect gen 10");
        assert_eq!(ids(&u.orders), vec![(0, 10)]);
        let u = s.complete(started[1].id, "d2".into(), None, t0 + ms(21));
        assert!(u.orders.is_empty() && !u.changed_current);
        assert_eq!(s.views(t0 + ms(21))[0].freshness, Freshness::Pending);
    }

    #[test]
    fn outer_deadline_marks_timeout_and_late_result_still_lands() {
        let t0 = Instant::now();
        let mut s = sched();
        let o = s.submit(req(1, "/r"), t0);
        s.complete(o[1].id, "host".into(), None, t0);
        let deadline = s.next_deadline().unwrap();
        assert_eq!(deadline, t0 + ms(1500 + 250));
        let u = s.expire(t0 + ms(1000));
        assert!(!u.changed_current);
        let u = s.expire(deadline);
        assert!(u.changed_current);
        assert!(s.is_settled());
        let v = s.views(deadline);
        assert_eq!(v[0].freshness, Freshness::Timeout);
        assert!(v[0].error.as_deref().unwrap().contains("1500"));
        assert_eq!(s.next_deadline(), None);
        assert_eq!(s.stats(0).abandoned_running, 1);
        // The late result for the same request replaces the timeout.
        let u = s.complete(o[0].id, "main".into(), None, deadline + ms(10));
        assert!(u.changed_current);
        assert_eq!(s.views(deadline + ms(10))[0].freshness, Freshness::Fresh);
        assert_eq!(s.stats(0).late, 1);
        assert_eq!(s.stats(0).abandoned_running, 0);
    }

    #[test]
    fn abandoned_fetch_frees_the_slot_for_queued_work() {
        let t0 = Instant::now();
        let mut s = sched();
        let o = s.submit(req(1, "/r"), t0);
        let _ = s.submit(req(2, "/r"), t0 + ms(5));
        let u = s.expire(t0 + ms(1750));
        assert_eq!(ids(&u.orders), vec![(0, 2)], "rerun starts once the hung fetch is abandoned");
        let _ = o;
    }

    #[test]
    fn too_many_hung_fetches_mark_provider_unresponsive() {
        let t0 = Instant::now();
        let mut s = sched();
        let mut t = t0;
        for g in 1..=4u64 {
            let o = s.submit(req(g, &format!("/h{g}")), t);
            assert!(o.iter().any(|o| o.provider == 0));
            t += ms(2000);
            s.expire(t);
        }
        assert_eq!(s.stats(0).abandoned_running, 4);
        let o = s.submit(req(5, "/h5"), t);
        assert!(o.iter().all(|o| o.provider != 0));
        let v = s.views(t);
        assert_eq!(v[0].freshness, Freshness::Error);
        assert!(v[0].error.as_deref().unwrap().contains("unresponsive"));
    }

    #[test]
    fn time_dependency_revalidates_and_zero_timeout_uses_default() {
        let t0 = Instant::now();
        let spec = ProviderSpec { key: "clock", deps: CLOCK_DEPS, ttl: ms(1000), timeout: Duration::ZERO };
        let mut s: Scheduler<u32> = Scheduler::new(vec![spec], SchedulerConfig::default());
        let o = s.submit(req(1, "/"), t0);
        assert_eq!(s.next_deadline(), Some(t0 + ms(2250)));
        s.complete(o[0].id, 1, None, t0);
        assert_eq!(s.submit(req(2, "/"), t0 + ms(10)).len(), 1, "Time dep refetches even when fresh");
        assert_eq!(s.views(t0 + ms(10))[0].scope, Scope::Global);
    }

    #[test]
    fn huge_ttl_and_timeout_do_not_overflow() {
        let t0 = Instant::now();
        let spec = ProviderSpec { key: "user", deps: &[], ttl: Duration::MAX, timeout: Duration::MAX };
        let mut s: Scheduler<u32> = Scheduler::new(vec![spec], SchedulerConfig::default());
        let o = s.submit(req(1, "/"), t0);
        assert!(s.next_deadline().is_some());
        s.complete(o[0].id, 7, None, t0);
        assert!(s.submit(req(2, "/"), t0 + ms(10)).is_empty());
        assert!(s.is_settled());
    }

    #[test]
    fn out_of_order_completion_never_regresses_cache() {
        let t0 = Instant::now();
        let mut s = sched();
        let a = s.submit(req(1, "/a"), t0);
        s.complete(a[1].id, "host".into(), None, t0);
        s.expire(t0 + ms(1750)); // fetch A for /a abandoned, still running
        let b = s.submit(req(2, "/a"), t0 + ms(1800));
        assert_eq!(ids(&b), vec![(0, 2)], "abandoned fetch does not block a new one");
        s.complete(b[0].id, "new".into(), None, t0 + ms(1810));
        let u = s.complete(a[0].id, "old".into(), None, t0 + ms(1900));
        assert!(!u.changed_current, "older data never replaces newer data");
        assert_eq!(s.views(t0 + ms(1900))[0].value.as_deref(), Some("new"));
        assert_eq!(s.stats(0).late, 1);
        // Completing an unknown id is ignored.
        assert!(!s.complete(9999, "x".into(), None, t0).changed_current);
    }

    #[test]
    fn queued_rerun_waits_for_same_key_fetch() {
        let t0 = Instant::now();
        let mut s = sched();
        let a = s.submit(req(1, "/a"), t0);
        let b = s.submit(req(2, "/b"), t0 + ms(1));
        let _ = s.submit(req(3, "/a"), t0 + ms(2));
        let u = s.complete(b[0].id, "b".into(), None, t0 + ms(3));
        assert!(u.orders.is_empty(), "one fetch per key: rerun waits for /a's first fetch");
        let u = s.complete(a[0].id, "a-old".into(), None, t0 + ms(4));
        assert!(u.changed_current);
        assert_eq!(ids(&u.orders), vec![(0, 3)]);
        s.complete(u.orders[0].id, "a-new".into(), None, t0 + ms(6));
        assert_eq!(s.views(t0 + ms(6))[0].value.as_deref(), Some("a-new"));
    }

    #[test]
    fn failed_fetch_marks_error_and_unblocks_rerun() {
        let t0 = Instant::now();
        let mut s = sched();
        let o = s.submit(req(1, "/r"), t0);
        s.complete(o[1].id, "host".into(), None, t0);
        let u = s.fail(o[0].id, "provider panicked".into(), t0 + ms(1));
        assert!(u.changed_current && s.is_settled());
        let v = s.views(t0 + ms(1));
        assert_eq!((v[0].freshness, v[0].error.as_deref()), (Freshness::Error, Some("provider panicked")));
        assert_eq!(s.stats(0).failed, 1);
        // A failure for an old request starts the latest queued rerun.
        let o = s.submit(req(2, "/r"), t0 + ms(2));
        let _ = s.submit(req(3, "/r"), t0 + ms(3));
        let u = s.fail(o[0].id, "boom".into(), t0 + ms(4));
        assert_eq!(ids(&u.orders), vec![(0, 3)]);
        assert!(!s.is_settled());
    }

    #[test]
    fn reconfigure_keeps_state_by_key() {
        let t0 = Instant::now();
        let mut s = sched();
        let o = s.submit(req(1, "/r"), t0);
        s.complete(o[1].id, "host".into(), None, t0);
        let git_fetch = o[0].id;
        let node = ProviderSpec { key: "node", deps: GIT_DEPS, ttl: ms(600_000), timeout: ms(500) };
        // New order: node first, then host, then git (still in flight).
        let mut specs = vec![node.clone()];
        specs.extend(specs_reordered());
        s.reconfigure(specs, |k| k != "node");
        assert_eq!(s.current_request(), None);
        assert_eq!((s.index_of("git"), s.index_of("host"), s.index_of("node")), (Some(2), Some(1), Some(0)));
        let o = s.submit(req(2, "/r"), t0 + ms(1));
        assert_eq!(ids(&o), vec![(0, 2)], "node starts; host cached; git's flight is kept");
        assert_eq!(s.stats(2).coalesced, 1, "git joins its in-flight fetch across the reload");
        let v = s.views(t0 + ms(1));
        assert_eq!(v[1].value.as_deref(), Some("host"), "cache entry survived under its key");
        // The old fetch id still completes the (moved) git provider.
        let u = s.complete(git_fetch, "main".into(), None, t0 + ms(5));
        assert!(u.changed_current);
        assert_eq!(s.views(t0 + ms(5))[2].value.as_deref(), Some("main"));
        // A provider whose spec changes, or that `retain` rejects, loses its entries.
        s.complete(o[0].id, "22.0.0".into(), None, t0 + ms(6));
        let mut host = specs_reordered()[0].clone();
        host.ttl = ms(1);
        s.reconfigure(vec![node, host], |_| true);
        let _ = s.submit(req(3, "/r"), t0 + ms(7));
        let v = s.views(t0 + ms(7));
        assert_eq!(v[0].value.as_deref(), Some("22.0.0"), "node kept: same spec, retained");
        assert_eq!(v[1].value, None, "host dropped: spec changed");
        // A completion for a removed provider is ignored.
        assert!(!s.complete(9999, "x".into(), None, t0 + ms(8)).changed_current);
    }

    fn specs_reordered() -> Vec<ProviderSpec> {
        let mut v = specs();
        v.reverse();
        v
    }

    #[test]
    fn cache_is_bounded() {
        let t0 = Instant::now();
        let cfg = SchedulerConfig { max_cache_entries: 3, ..SchedulerConfig::default() };
        let mut s: Scheduler<String> = Scheduler::new(specs(), cfg);
        for g in 0..10u64 {
            let o = s.submit(req(g, &format!("/c{g}")), t0 + ms(g));
            for order in o {
                s.complete(order.id, format!("v{g}"), None, t0 + ms(g));
            }
        }
        assert!(s.cache.len() <= 3);
    }
    #[test]
    fn targeted_refresh_preserves_other_slots_and_current_sequence() {
        let t = Instant::now();
        let mut s = sched();
        for o in s.submit(req(7, "/r"), t) {
            s.complete(o.id, format!("v{}", o.provider), None, t);
}
        let seq = s.current_seq();
        let orders = s.refresh(&["git"], t + ms(1));
        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0].provider, 0);
        assert_eq!(s.current_seq(), seq);
        assert_eq!(s.current_request().unwrap().gen, 7);
        assert_eq!(s.views(t + ms(1))[1].value.as_deref(), Some("v1"));
        assert!(!s.views(t + ms(1))[1].fetching);
        assert!(s.refresh(&["git"], t + ms(2)).is_empty());
        assert_eq!(s.stats(0).coalesced, 1);
    }
}
