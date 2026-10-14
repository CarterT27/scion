//! SPEC 8.4 / 18.4 lock-free Land (P20).
//!
//! Each attempt runs the SPEC 8.4 states `Resolving` -> stack detection -> `Checking` ->
//! `Rebasing` -> `Committing` with a constant number of metadata transactions besides the
//! per-member reads: one strong ref read, the stack detection (skip-pointer reads, cached, plus
//! one strong `changeset_children` read per stack member; independent of how far behind the tip
//! the stack's base is), one `get_landed` per stack member, the rebases, and one `cas_ref`.
//! No lock or lease is ever taken: progress depends only on the single-row CAS, and a lost race
//! (`VersionMismatch` or a `landed` unique violation) backs off with full jitter and starts over
//! at `Resolving`, re-reading the ref AND the policy.

use crate::history::History;
use crate::hooks::{LandAttemptOutcome, LandAuthorizer, LandAuthorizerSource, Rebaser};
use crate::refs::ref_not_found;
use crate::types::{ChangesetRecord, LandRequest, LandResult};
use futures::future::try_join_all;
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;
use scion_core::fault::points;
use scion_core::{ErrorCode, ErrorPayload, ObjectId, Principal, Result, ScionError, subcode};
use scion_store::{CasOutcome, LandedEntry, RefCas, RefExpect};
use std::collections::{BTreeSet, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// Result of one land attempt that did not fail.
enum Step {
    /// The land finished (committed or idempotent).
    Done(LandResult, LandAttemptOutcome),
    /// The CAS lost a race; back off and retry.
    Retry(LandAttemptOutcome),
}

/// Full-jitter backoff (SPEC 8.4 `Retrying`): a uniform delay in `[0, cap]` where
/// `cap = min(backoff_max, backoff_base * 2^(attempt - 1))`.
pub(crate) fn full_jitter(
    rng: &mut impl Rng,
    base: Duration,
    max: Duration,
    attempt: u32,
) -> Duration {
    let factor = 1u32
        .checked_shl(attempt.saturating_sub(1))
        .unwrap_or(u32::MAX);
    let cap = base.saturating_mul(factor).min(max);
    let cap_nanos = u64::try_from(cap.as_nanos()).unwrap_or(u64::MAX);
    if cap_nanos == 0 {
        return Duration::ZERO;
    }
    Duration::from_nanos(rng.random_range(0..=cap_nanos))
}

/// The backoff RNG seed: the configured seed (or the clock) mixed with the stack head, so
/// concurrent lands sharing a configured seed do not retry in lockstep.
fn backoff_seed(configured: Option<u64>, now: SystemTime, head: &ObjectId) -> u64 {
    let base = configured.unwrap_or_else(|| {
        now.duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
            .unwrap_or(0)
    });
    let mut digest = [0u8; 8];
    digest.copy_from_slice(&head.digest()[..8]);
    base ^ u64::from_le_bytes(digest)
}

/// INVALID_ARGUMENT/merge_in_stack (PLAN 6.7).
fn merge_in_stack(id: &ObjectId, parents: usize) -> ScionError {
    ScionError::invalid(
        subcode::MERGE_IN_STACK,
        format!("stack member {id} has {parents} parents; v1 stacks must be linear"),
    )
}

impl History {
    /// Land using `self` as the [`Rebaser`] (SPEC 8.4; [`History::land_with`]).
    pub async fn land(
        &self,
        req: LandRequest,
        who: &Principal,
        authz: &dyn LandAuthorizerSource,
    ) -> Result<LandResult> {
        self.land_with(req, who, authz, self).await
    }

    /// SPEC 8.4 / 18.4 lock-free Land with an explicit rebaser (tests use a fake).
    ///
    /// Per attempt:
    /// 1. `Resolving`: strong read of the ref `(tip, v)` (NOT_FOUND when absent), then
    ///    `authz.current()` (the policy is re-read on EVERY attempt) and `update` on the ref
    ///    (PERMISSION_DENIED).
    /// 2. Stack: the first-parent chain from `head` back to, and excluding, the first changeset
    ///    that is an ancestor of `tip` (SPEC 18.5: the first-parent form locates the base, the
    ///    full form confirms it when `tip`'s history contains merges). Every member must have
    ///    exactly one parent (INVALID_ARGUMENT/merge_in_stack).
    /// 3. `Checking`: the `landed` row of each member's change id. The same source is skipped and
    ///    recorded; a different source fails ALREADY_LANDED (payload `AlreadyLanded`). When every
    ///    member is skipped the recorded result `{tip, v, recorded}` is returned (idempotent).
    /// 4. `Rebasing`: `write` on every delta path of every remaining member
    ///    (PERMISSION_DENIED), then each member is rebased onto the previous result, starting at
    ///    `tip`. MERGE_CONFLICT fails the land; EMPTY_CHANGE fails unless `allow_empty`, in which
    ///    case the member is recorded with `result` = the current onto changeset (PLAN 6.14).
    ///    Checkpoint `land.after_rebase`.
    /// 5. `Committing`: one `cas_ref(Version(v))` writing the ref, the `ref_log` row and the
    ///    `landed` rows. Checkpoint `land.after_commit`.
    ///
    /// A version mismatch or a `landed` unique violation backs off with full jitter between
    /// `backoff_base` and `backoff_max` and retries; after `max_attempts` the land fails
    /// UNAVAILABLE/land_contention (retryable) and the ref is unchanged. Every attempt is
    /// reported to the configured [`crate::LandObserver`] with its elapsed time.
    pub async fn land_with(
        &self,
        req: LandRequest,
        who: &Principal,
        authz: &dyn LandAuthorizerSource,
        rebaser: &dyn Rebaser,
    ) -> Result<LandResult> {
        let cfg = &self.cfg.land;
        let max_attempts = cfg.max_attempts.max(1);
        let mut rng =
            ChaCha8Rng::seed_from_u64(backoff_seed(cfg.rng_seed, self.clock.now(), &req.head));
        for attempt in 1..=max_attempts {
            let started = tokio::time::Instant::now();
            let step = self.land_attempt(&req, who, authz, rebaser).await;
            let elapsed = started.elapsed();
            match step {
                Ok(Step::Done(result, outcome)) => {
                    self.land_observer
                        .attempt(&req.ref_name, attempt, &outcome, elapsed);
                    tracing::info!(
                        ref_name = req.ref_name.as_str(),
                        attempt,
                        version = result.version,
                        landed = result.landed.len(),
                        outcome = ?outcome,
                        "land finished"
                    );
                    return Ok(result);
                }
                Ok(Step::Retry(outcome)) => {
                    self.land_observer
                        .attempt(&req.ref_name, attempt, &outcome, elapsed);
                    tracing::debug!(
                        ref_name = req.ref_name.as_str(),
                        attempt,
                        outcome = ?outcome,
                        "land attempt lost the CAS race; retrying"
                    );
                    if attempt < max_attempts {
                        let delay =
                            full_jitter(&mut rng, cfg.backoff_base, cfg.backoff_max, attempt);
                        tokio::time::sleep(delay).await;
                    }
                }
                Err(e) => {
                    self.land_observer.attempt(
                        &req.ref_name,
                        attempt,
                        &LandAttemptOutcome::Failed(e.code),
                        elapsed,
                    );
                    tracing::debug!(
                        ref_name = req.ref_name.as_str(),
                        attempt,
                        code = e.code.as_str(),
                        "land failed"
                    );
                    return Err(e);
                }
            }
        }
        tracing::warn!(
            ref_name = req.ref_name.as_str(),
            max_attempts,
            "land gave up after max_attempts"
        );
        Err(ScionError::unavailable(
            subcode::LAND_CONTENTION,
            format!(
                "land onto {} lost the ref CAS {max_attempts} times",
                req.ref_name.as_str()
            ),
        ))
    }

    /// One SPEC 8.4 attempt (`Resolving` through `Committing`).
    async fn land_attempt(
        &self,
        req: &LandRequest,
        who: &Principal,
        authz: &dyn LandAuthorizerSource,
        rebaser: &dyn Rebaser,
    ) -> Result<Step> {
        let name = &req.ref_name;

        // 1. Resolving.
        let tip_row = self
            .meta()
            .get_ref(name)
            .await?
            .ok_or_else(|| ref_not_found(name))?;
        let (tip, version) = (tip_row.target, tip_row.version);
        let az: Arc<dyn LandAuthorizer> = authz.current();
        if !az.can_update(name) {
            return Err(ScionError::permission_denied(format!(
                "update on ref {} denied",
                name.as_str()
            )));
        }

        // 2. Stack (oldest first).
        let stack = self.land_stack(&req.head, &tip).await?;

        // 3. Checking.
        let meta = self.meta();
        let rows = try_join_all(
            stack
                .iter()
                .map(|m| meta.get_landed(name, &m.changeset.change_id)),
        )
        .await?;
        let mut recorded = Vec::new();
        let mut pending = Vec::new();
        for (m, row) in stack.iter().zip(rows) {
            match row {
                Some(l) if l.source == m.id => recorded.push(LandedEntry {
                    change_id: l.change_id,
                    source: l.source,
                    result: l.result,
                }),
                Some(l) => {
                    return Err(ScionError::new(
                        ErrorCode::AlreadyLanded,
                        format!(
                            "change {} already landed on {} from {} (stack member {})",
                            l.change_id.to_hex(),
                            name.as_str(),
                            l.source,
                            m.id
                        ),
                    )
                    .with_payload(ErrorPayload::AlreadyLanded {
                        source: l.source,
                        result: l.result,
                        version: l.version,
                    }));
                }
                None => pending.push(m),
            }
        }
        if pending.is_empty() {
            return Ok(Step::Done(
                LandResult {
                    target: tip,
                    version,
                    landed: recorded,
                },
                LandAttemptOutcome::Idempotent,
            ));
        }
        let mut seen = BTreeSet::new();
        for m in &pending {
            if !seen.insert(m.changeset.change_id) {
                // Two revisions of one change in one stack: the second `landed` row would hit
                // the unique index on every attempt, so fail fast instead of retrying until
                // land_contention.
                return Err(ScionError::invalid(
                    subcode::DUPLICATE,
                    format!(
                        "change {} appears twice in the stack",
                        m.changeset.change_id.to_hex()
                    ),
                ));
            }
        }

        // 4. Rebasing: authorization of every delta path first, then the chained rebases.
        for m in &pending {
            if m.changeset.delta.iter().any(|e| !az.can_write(&e.path)) {
                return Err(ScionError::permission_denied(format!(
                    "write denied on a path changed by stack member {}",
                    m.id
                )));
            }
        }
        let mut onto = tip;
        let mut landed_rows = Vec::with_capacity(pending.len());
        for m in &pending {
            let result = match rebaser.rebase(&m.id, &onto, who).await {
                Ok(r) => r,
                Err(e) if e.code == ErrorCode::EmptyChange && req.allow_empty => onto,
                Err(e) => return Err(e),
            };
            landed_rows.push(LandedEntry {
                change_id: m.changeset.change_id,
                source: m.id,
                result,
            });
            onto = result;
        }
        self.objects
            .storage()
            .faults
            .check(points::LAND_AFTER_REBASE)
            .await?;

        // 5. Committing.
        let outcome = meta
            .cas_ref(RefCas {
                name: name.clone(),
                expect: RefExpect::Version(version),
                new_target: Some(onto),
                principal: who.id_string(),
                at: self.clock.now(),
                landed: landed_rows.clone(),
                audit: None,
            })
            .await?;
        match outcome {
            CasOutcome::Committed(Some(row)) => {
                self.objects
                    .storage()
                    .faults
                    .check(points::LAND_AFTER_COMMIT)
                    .await?;
                recorded.extend(landed_rows);
                Ok(Step::Done(
                    LandResult {
                        target: row.target,
                        version: row.version,
                        landed: recorded,
                    },
                    LandAttemptOutcome::Committed,
                ))
            }
            CasOutcome::Committed(None) => {
                tracing::error!("land cas_ref committed a deletion");
                Err(ScionError::unavailable(
                    subcode::STORE_UNAVAILABLE,
                    "unexpected cas_ref outcome: land committed a deletion",
                ))
            }
            CasOutcome::VersionMismatch { .. } => Ok(Step::Retry(LandAttemptOutcome::CasMismatch)),
            CasOutcome::LandedConflict { .. } => {
                Ok(Step::Retry(LandAttemptOutcome::LandedConflict))
            }
        }
    }

    /// Is `c` (generation `gen_c`) an ancestor of `tip` along `tip`'s first-parent chain?
    async fn tip_fp_ancestor(
        &self,
        c: &ObjectId,
        gen_c: u64,
        tip: &ObjectId,
        tip_gen: u64,
    ) -> Result<bool> {
        if c == tip {
            return Ok(true);
        }
        if gen_c >= tip_gen {
            return Ok(false);
        }
        self.is_first_parent_ancestor(c, tip).await
    }

    /// Is `c` (generation `gen_c`) an ancestor of `tip` by any path (SPEC 18.5 full form)?
    ///
    /// The generation-pruned search down from `tip` costs O(generation gap) on a linear `main`,
    /// which would make a land with a stale base linear in how far behind it is. The answer is
    /// therefore computed bidirectionally, one step of each side in turn, and the first side to
    /// finish decides:
    /// - down: the SPEC 18.5 generation-pruned DFS from `tip` (finds `c`, or exhausts: false);
    /// - up: a BFS over `changeset_children` from `c`, pruned to generations below `tip`'s. A
    ///   descendant of `c` that is `tip` or on `tip`'s first-parent chain (O(log n) skip-pointer
    ///   check) proves ancestry; exhausting every such descendant proves the opposite, because
    ///   every path from `c` up to `tip` runs through ancestors of `tip`.
    ///
    /// For a fresh stack the candidate's descendants are the members above it, so the up side
    /// finishes after O(stack) steps independent of the gap (SPEC 19.18). `children` is a strong
    /// read, and every changeset on a path down from `tip` was committed before `tip` became the
    /// ref target, so no relevant edge can be missed.
    async fn tip_ancestor(
        &self,
        c: &ObjectId,
        gen_c: u64,
        tip: &ObjectId,
        tip_gen: u64,
    ) -> Result<bool> {
        if c == tip {
            return Ok(true);
        }
        if gen_c >= tip_gen {
            return Ok(false);
        }
        let mut down = vec![*tip];
        let mut down_seen: HashSet<ObjectId> = HashSet::new();
        let mut up = VecDeque::from([*c]);
        let mut up_seen: HashSet<ObjectId> = HashSet::from([*c]);
        loop {
            // One step down from `tip`.
            let Some(x) = down.pop() else {
                return Ok(false);
            };
            if x == *c {
                return Ok(true);
            }
            if down_seen.insert(x) {
                let (cs, row) = self.objects.get_changeset(&x).await?;
                if row.generation > gen_c {
                    // Reverse push so the first parent is explored first.
                    for p in cs.parents.iter().rev() {
                        if !down_seen.contains(p) {
                            down.push(*p);
                        }
                    }
                }
            }
            // One step up from `c`.
            let Some(y) = up.pop_front() else {
                return Ok(false);
            };
            for child in self.meta().children(&y).await? {
                if child == *tip {
                    return Ok(true);
                }
                if !up_seen.insert(child) {
                    continue;
                }
                let gen_child = self.objects.get_changeset(&child).await?.1.generation;
                if gen_child >= tip_gen {
                    continue;
                }
                if self.is_first_parent_ancestor(&child, tip).await? {
                    return Ok(true);
                }
                up.push_back(child);
            }
        }
    }

    /// SPEC 8.4 stack detection, oldest member first: the first-parent chain from `head` back
    /// to, and excluding, the first changeset that is an ancestor of `tip`.
    ///
    /// Ancestry of `tip` is monotone along a first-parent chain (the first parent of an
    /// ancestor is an ancestor). The walk stops at the first changeset on `tip`'s first-parent
    /// history (O(log n) via skip pointers, SPEC 18.5). Members reachable from `tip` only
    /// through a merge (possible only when `tip`'s history contains merges) are then trimmed
    /// with the full form ([`History::tip_ancestor`], bidirectional so that a stale base on a
    /// linear `main` costs O(stack), not O(gap)): one check of the oldest candidate, plus a
    /// binary search when it turns out to be an ancestor. A multi-parent (or parentless) member fails
    /// INVALID_ARGUMENT/merge_in_stack.
    async fn land_stack(&self, head: &ObjectId, tip: &ObjectId) -> Result<Vec<ChangesetRecord>> {
        let tip_gen = self.changeset(tip).await?.generation;
        let mut chain: Vec<ChangesetRecord> = Vec::new(); // newest first
        let mut cur = *head;
        loop {
            let rec = self.changeset(&cur).await?;
            if self
                .tip_fp_ancestor(&rec.id, rec.generation, tip, tip_gen)
                .await?
            {
                break;
            }
            let parent = match rec.changeset.parents.as_slice() {
                [p] => *p,
                ps => {
                    if self
                        .tip_ancestor(&rec.id, rec.generation, tip, tip_gen)
                        .await?
                    {
                        break;
                    }
                    return Err(merge_in_stack(&rec.id, ps.len()));
                }
            };
            chain.push(rec);
            cur = parent;
        }
        if let Some(oldest) = chain.last()
            && self
                .tip_ancestor(&oldest.id, oldest.generation, tip, tip_gen)
                .await?
        {
            // Find the newest member that is an ancestor: chain[lo..] are all ancestors.
            let (mut lo, mut hi) = (0usize, chain.len() - 1);
            while lo < hi {
                let mid = lo + (hi - lo) / 2;
                let m = &chain[mid];
                if self.tip_ancestor(&m.id, m.generation, tip, tip_gen).await? {
                    hi = mid;
                } else {
                    lo = mid + 1;
                }
            }
            chain.truncate(lo);
        }
        chain.reverse();
        Ok(chain)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_jitter_stays_within_the_growing_cap() {
        let mut rng = ChaCha8Rng::seed_from_u64(7);
        let base = Duration::from_millis(5);
        let max = Duration::from_millis(500);
        for attempt in 1..=40u32 {
            let cap = base
                .saturating_mul(1u32.checked_shl(attempt - 1).unwrap_or(u32::MAX))
                .min(max);
            for _ in 0..50 {
                let d = full_jitter(&mut rng, base, max, attempt);
                assert!(d <= cap, "attempt {attempt}: {d:?} > {cap:?}");
            }
        }
        assert_eq!(
            full_jitter(&mut rng, Duration::ZERO, Duration::ZERO, 3),
            Duration::ZERO
        );
    }

    #[test]
    fn full_jitter_is_seedable() {
        let draw = |seed| {
            let mut rng = ChaCha8Rng::seed_from_u64(seed);
            (1..=10)
                .map(|a| {
                    full_jitter(
                        &mut rng,
                        Duration::from_millis(5),
                        Duration::from_millis(500),
                        a,
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(draw(1), draw(1));
        assert_ne!(draw(1), draw(2));
    }

    #[test]
    fn backoff_seed_mixes_the_head() {
        let a = ObjectId::new(scion_core::Kind::Changeset, [1; 32]);
        let b = ObjectId::new(scion_core::Kind::Changeset, [2; 32]);
        let now = SystemTime::UNIX_EPOCH;
        assert_eq!(
            backoff_seed(Some(9), now, &a),
            backoff_seed(Some(9), now, &a)
        );
        assert_ne!(
            backoff_seed(Some(9), now, &a),
            backoff_seed(Some(9), now, &b)
        );
    }
}
