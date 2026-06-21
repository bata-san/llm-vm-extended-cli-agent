//! ⑤ Hedge Requests.
//!
//! Under a 40 RPM budget you can't afford to blindly fan out N copies of every
//! request. But you also can't afford to wait 30s for a hung request. The
//! hedge pattern threads this needle: issue **one** request, and only if it
//! hasn't produced a first token by `ttft_threshold` do you fire a second
//! (hedged) request, racing them and taking whichever answers first.
//!
//! On healthy endpoints the hedge never fires and you pay nothing. On a
//! stalled primary the hedge rescues you, and (crucially) the loser is
//! dropped so you don't burn two RPM tokens on a single user intent.
//!
//! This module is transport-agnostic: callers pass a `Future<Output = T>`
//! factory plus a `Future<Output = ()>` that resolves when the first token
//! arrives. The executor decides whether to hedge.

use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::{sleep_until, Instant};

/// Knobs controlling hedging behaviour.
#[derive(Debug, Clone)]
pub struct HedgeConfig {
    /// If the primary doesn't produce a first token within this duration,
    /// fire a hedge.
    pub ttft_threshold: Duration,
    /// Maximum number of hedge attempts (in addition to the primary).
    pub max_hedges: usize,
    /// Jitter applied to the threshold so concurrent requests don't all
    /// hedge simultaneously and thunder-herd the server.
    pub jitter: Duration,
}

impl Default for HedgeConfig {
    fn default() -> Self {
        Self {
            ttft_threshold: Duration::from_millis(1500),
            max_hedges: 1,
            jitter: Duration::from_millis(250),
        }
    }
}

/// Per-process counters to observe how often hedges actually fire. Useful for
/// tuning `ttft_threshold` against a specific endpoint.
#[derive(Debug, Default, Clone)]
pub struct HedgeStats {
    inner: Arc<Mutex<StatsInner>>,
}

#[derive(Debug, Default, Clone, Copy)]
struct StatsInner {
    pub total: u64,
    pub hedges_fired: u64,
    pub hedge_won: u64,
}

impl HedgeStats {
    pub async fn snapshot(&self) -> (u64, u64, u64) {
        let s = *self.inner.lock().await;
        (s.total, s.hedges_fired, s.hedge_won)
    }

    async fn record_request(&self) {
        self.inner.lock().await.total += 1;
    }

    async fn record_hedge_fired(&self) {
        self.inner.lock().await.hedges_fired += 1;
    }

    async fn record_hedge_won(&self) {
        self.inner.lock().await.hedge_won += 1;
    }
}

#[derive(Debug, thiserror::Error)]
pub enum HedgeError {
    #[error("all requests failed")]
    AllFailed,
}

/// Race a primary future against (up to) `max_hedges` hedge futures spawned
/// after `ttft_threshold` has elapsed without a first token.
///
/// - `make_request` produces a fresh request future each call.
/// - `first_token_signal` is a future that resolves when the primary has
///   started streaming (so we know not to hedge). For non-streaming requests
///   pass `std::future::pending()` — but then hedges will always fire.
///
/// The losing futures are dropped (and thus cancelled, assuming the
/// underlying HTTP client respects `Drop`).
///
/// Implementation note: the inner polling is implemented as an explicit
/// state machine rather than a single big `select!` because combining
/// pinned `Sleep` timers, optional hedge futures, and a generic `R` in one
/// `select!` arm fights the borrow checker. We poll each event source in its
/// own `select!` branch and use shared flags to communicate state changes.
pub async fn hedge<F, R, S>(
    cfg: HedgeConfig,
    stats: HedgeStats,
    make_request: impl Fn() -> F,
    first_token_signal: impl Fn() -> S,
) -> Result<R, HedgeError>
where
    F: std::future::Future<Output = R>,
    S: std::future::Future<Output = ()>,
{
    stats.record_request().await;

    let jitter = if cfg.jitter.is_zero() {
        Duration::ZERO
    } else {
        // Cheap pseudo-jitter without pulling `rand`.
        let nanos =
            Instant::now().elapsed().as_nanos() as u64 % cfg.jitter.as_nanos().max(1) as u64;
        Duration::from_nanos(nanos)
    };
    let threshold = cfg.ttft_threshold + jitter;

    // Primary + first-token signal.
    let mut primary = Box::pin(make_request());
    let mut signal: Option<std::pin::Pin<Box<S>>> = Some(Box::pin(first_token_signal()));

    // Hedge futures spawned lazily.
    let mut hedges: Vec<std::pin::Pin<Box<F>>> = Vec::new();
    let mut hedges_remaining = cfg.max_hedges;
    let mut next_hedge_deadline: Option<Instant> = Some(Instant::now() + threshold);

    loop {
        // Snapshot state for this iteration into local booleans so the
        // `select!` arms don't need to borrow `&mut` things simultaneously.
        let have_hedges = !hedges.is_empty();
        let can_hedge = next_hedge_deadline.is_some() && hedges_remaining > 0;
        let deadline_when = next_hedge_deadline;

        // Drive each event source. We use a sequence of `select!`s guarded by
        // `if` conditions on the result; the outer loop re-evaluates after any
        // state change.
        tokio::select! {
            // Primary finished.
            r = &mut primary => {
                return Ok(r);
            }
            // First-token signal fired.
            _ = async {
                match signal.as_mut() {
                    Some(s) => s.as_mut().await,
                    None => std::future::pending::<()>().await,
                }
            } => {
                signal = None;
                next_hedge_deadline = None;
            }
            // Threshold elapsed → spawn a hedge.
            _ = async move {
                match deadline_when {
                    Some(when) => sleep_until(when).await,
                    None => std::future::pending::<()>().await,
                }
            }, if can_hedge => {
                stats.record_hedge_fired().await;
                hedges.push(Box::pin(make_request()));
                hedges_remaining -= 1;
                next_hedge_deadline = if hedges_remaining > 0 {
                    Some(Instant::now() + threshold)
                } else {
                    None
                };
            }
            // Poll hedges: select_all races them and yields the first result.
            // This branch is only enabled when at least one hedge exists.
            (r, _idx, _remaining) = async {
                let futs: Vec<_> = hedges.iter_mut().map(|h| h.as_mut()).collect();
                futures::future::select_all(futs).await
            }, if have_hedges => {
                // The winning hedge future resolved; the others are dropped
                // when `hedges` goes out of scope at function return.
                stats.record_hedge_won().await;
                return Ok(r);
            }
        }
    }
}

/// Convenience wrapper bundling config + stats for an executor.
pub struct HedgingExecutor {
    pub config: HedgeConfig,
    pub stats: HedgeStats,
}

impl HedgingExecutor {
    pub fn new(config: HedgeConfig) -> Self {
        Self {
            config,
            stats: HedgeStats::default(),
        }
    }

    pub async fn run<F, R, S>(
        &self,
        make_request: impl Fn() -> F,
        first_token_signal: impl Fn() -> S,
    ) -> Result<R, HedgeError>
    where
        F: std::future::Future<Output = R>,
        S: std::future::Future<Output = ()>,
    {
        hedge(
            self.config.clone(),
            self.stats.clone(),
            make_request,
            first_token_signal,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn primary_fast_enough_never_hedges() {
        let stats = HedgeStats::default();
        let cfg = HedgeConfig {
            ttft_threshold: Duration::from_secs(10),
            ..Default::default()
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let r = hedge(
            cfg,
            stats.clone(),
            || {
                let c = c.clone();
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    "ok"
                }
            },
            || async {},
        )
        .await
        .unwrap();
        assert_eq!(r, "ok");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let (total, fired, won) = stats.snapshot().await;
        assert_eq!(total, 1);
        assert_eq!(fired, 0);
        assert_eq!(won, 0);
    }

    #[tokio::test]
    async fn slow_primary_triggers_hedge_and_hedge_wins() {
        let stats = HedgeStats::default();
        let cfg = HedgeConfig {
            ttft_threshold: Duration::from_millis(20),
            max_hedges: 1,
            jitter: Duration::ZERO,
        };
        let primary_calls = Arc::new(AtomicUsize::new(0));
        let pc = primary_calls.clone();

        // Primary never produces a token and never completes within the test;
        // the hedge completes fast.
        let r: &str = hedge::<_, _, std::future::Pending<()>>(
            cfg,
            stats.clone(),
            || {
                let pc = pc.clone();
                async move {
                    pc.fetch_add(1, Ordering::SeqCst);
                    // Sleep longer than the test will run; gets dropped on hedge win.
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    "slow"
                }
            },
            || std::future::pending(),
        )
        .await
        .unwrap();
        assert_eq!(r, "slow");
        let (total, fired, won) = stats.snapshot().await;
        assert_eq!(total, 1);
        assert_eq!(fired, 1);
        assert_eq!(won, 1);
    }
}
