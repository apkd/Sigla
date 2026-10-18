//! Request-local transport sizing. These are batch guards, never traversal limits.
use std::time::Duration;

const MAX_UNITS: usize = 256;
const PROBE_UNITS: usize = 4;
const BATCH_BYTES: u64 = (super::MEMORY / 8) as u64;

#[derive(Clone, Copy)]
pub(super) enum Kind {
    Commits,
    CommitTrees,
    Trees,
    Blobs,
}

#[derive(Default)]
struct Cost {
    wire: Option<f64>,
    decoded: Option<f64>,
    units: usize,
}

#[derive(Default)]
pub(super) struct Planner {
    costs: [Cost; 4],
    overhead: Option<f64>,
    seconds_per_byte: Option<f64>,
    /// A useful ancestry horizon, not a stop condition. None means discovery
    /// must continue until the caller proves its interval or selected history.
    pub commits: Option<usize>,
}

fn update(estimate: &mut Option<f64>, sample: f64) {
    *estimate = Some(estimate.map_or(sample, |old| (old + sample) / 2.0));
}

impl Planner {
    pub fn object(&mut self, kind: Kind, bytes: usize) {
        update(&mut self.costs[kind as usize].decoded, bytes as f64);
    }

    pub fn observe(
        &mut self,
        kind: Kind,
        units: usize,
        bytes: u64,
        elapsed: Duration,
        streaming: Option<(u64, Duration)>,
    ) {
        let cost = &mut self.costs[kind as usize];
        let units = units.max(1);
        // A narrow request does not erase the capacity measured by a wider one.
        // Byte and transport estimates still shrink batches when costs increase.
        cost.units = cost.units.max(units);
        update(&mut cost.wire, bytes as f64 / units as f64);
        let mut overhead = elapsed.as_secs_f64();
        if let Some((bytes, duration)) = streaming
            && bytes > 0
            && !duration.is_zero()
            && duration <= elapsed
        {
            update(
                &mut self.seconds_per_byte,
                duration.as_secs_f64() / bytes as f64,
            );
            overhead -= duration.as_secs_f64();
        }
        // Includes session setup, negotiation and publication. A small pack
        // updates overhead but does not invent a bandwidth measurement.
        update(&mut self.overhead, overhead);
    }

    /// `available` contains only required, missing wants (or the current useful
    /// ancestry horizon). Local coverage and query breadth constrain every plan.
    pub fn count(&self, kind: Kind, available: usize, remaining: Option<u64>) -> usize {
        if available == 0 {
            return 0;
        }
        let cost = &self.costs[kind as usize];
        let growth = if cost.units == 0 {
            // Tree-filter fallback has unmeasured recursive tree fanout.
            if matches!(kind, Kind::CommitTrees) {
                1
            } else {
                PROBE_UNITS
            }
        } else {
            cost.units.saturating_mul(2)
        };
        // Wire bytes include recursive metadata. Decoded sizes conservatively
        // cover compression and short ancestry responses; neither prices a
        // repository-wide closure or authorizes additional source wants.
        let size = cost
            .wire
            .unwrap_or(0.0)
            .max(cost.decoded.unwrap_or(0.0))
            .max(1.0);
        let amortized = match (self.overhead, self.seconds_per_byte) {
            (Some(overhead), Some(rate)) if rate > 0.0 => {
                // Amortize one observed request's fixed cost, not a deadline.
                (overhead / (rate * size)).ceil() as usize
            }
            _ => growth, // Insufficient evidence: measure another useful probe.
        };
        let bytes = remaining.unwrap_or(BATCH_BYTES).min(BATCH_BYTES);
        available
            .min(MAX_UNITS)
            .min(growth)
            .min(amortized.max(1))
            .min(((bytes as f64 / size) as usize).max(1))
    }

    pub fn depth(&self, kind: Kind, wants: usize, remaining: Option<u64>) -> u32 {
        let horizon = self.commits.unwrap_or(MAX_UNITS).max(1);
        let units = self.count(kind, horizon.saturating_mul(wants), remaining);
        (units / wants.max(1)).max(1) as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn measured(overhead: f64, throughput: f64, size: usize) -> Planner {
        let mut planner = Planner::default();
        planner.object(Kind::Blobs, size);
        let units = 32;
        let bytes = (units * size) as u64;
        let streaming = Duration::from_secs_f64((bytes - size as u64) as f64 / throughput);
        planner.observe(
            Kind::Blobs,
            units,
            bytes,
            Duration::from_secs_f64(overhead) + streaming,
            Some((bytes - size as u64, streaming)),
        );
        planner
    }

    #[test]
    fn overhead_throughput_and_sizes_change_the_plan() {
        let count = |planner: Planner| planner.count(Kind::Blobs, MAX_UNITS, None);
        let fast = count(measured(0.25, 1_000_000.0, 1024));
        assert!(fast > count(measured(0.001, 1_000_000.0, 1024)));
        assert!(fast > count(measured(0.25, 10_000.0, 1024)));
        assert!(fast > count(measured(0.25, 1_000_000.0, 1024 * 1024)));
    }

    #[test]
    fn unknown_throughput_uses_bounded_useful_probes() {
        let mut planner = Planner::default();
        let first = planner.count(Kind::Blobs, usize::MAX, None);
        planner.observe(Kind::Blobs, first, 4096, Duration::from_secs(1), None);
        assert!(planner.seconds_per_byte.is_none());
        let next = planner.count(Kind::Blobs, usize::MAX, None);
        assert!(next > first && next <= first * 2);
        planner.observe(
            Kind::Blobs,
            next,
            8192,
            Duration::from_secs(1),
            Some((8192, Duration::ZERO)),
        );
        assert!(planner.seconds_per_byte.is_none());
        assert!(planner.count(Kind::Blobs, usize::MAX, None) <= MAX_UNITS);
    }

    #[test]
    fn narrow_requests_preserve_measured_capacity() {
        let mut planner = measured(1.0, 1_000_000.0, 1024);
        let broad = planner.count(Kind::Blobs, MAX_UNITS, None);
        planner.observe(Kind::Blobs, 1, 1024, Duration::from_secs(1), None);
        assert_eq!(planner.count(Kind::Blobs, MAX_UNITS, None), broad);
        assert_eq!(planner.count(Kind::Blobs, 1, None), 1);
        // A small request can still reveal expensive objects.
        planner.observe(Kind::Blobs, 1, BATCH_BYTES, Duration::from_secs(1), None);
        assert!(planner.count(Kind::Blobs, MAX_UNITS, None) < broad);
    }

    #[test]
    fn missing_coverage_and_query_horizon_bound_batches_not_completion() {
        let mut planner = measured(1.0, 1_000_000.0, 1024);
        let broad = planner.count(Kind::Blobs, MAX_UNITS, None);
        assert!(broad > 1);
        assert_eq!(planner.count(Kind::Blobs, 1, None), 1);
        assert_eq!(planner.count(Kind::Blobs, 0, None), 0);
        planner.commits = Some(1);
        assert_eq!(planner.depth(Kind::Commits, 1, None), 1);
        planner.commits = None;
        assert!(planner.depth(Kind::Commits, 1, None) > 1);
        let mut missing = MAX_UNITS * 3 + 1;
        while missing > 0 {
            let count = planner.count(Kind::Blobs, missing, None);
            assert!((1..=missing.min(MAX_UNITS)).contains(&count));
            missing -= count;
        }
    }

    #[test]
    fn estimates_react_to_new_measurements_and_respect_byte_guards() {
        let mut planner = measured(1.0, 1_000_000.0, 1024);
        let before = planner.count(Kind::Blobs, MAX_UNITS, None);
        planner.observe(
            Kind::Blobs,
            32,
            32 * 1024,
            Duration::from_secs(33),
            Some((32 * 1024, Duration::from_secs(32))),
        );
        assert!(planner.count(Kind::Blobs, MAX_UNITS, None) < before);
        for remaining in [0, 1, 2048, BATCH_BYTES, u64::MAX] {
            let count = planner.count(Kind::Blobs, usize::MAX, Some(remaining));
            assert!((1..=MAX_UNITS).contains(&count));
            // A single mandatory want must still reach the actual transfer
            // limiter, rather than silently terminating an incomplete query.
            assert!(count == 1 || count as u64 * 1024 <= remaining.min(BATCH_BYTES));
        }
    }

    #[test]
    fn recursive_metadata_and_filter_fallback_have_separate_costs() {
        let mut planner = measured(1.0, 1_000_000.0, 1024);
        planner.observe(Kind::Commits, 64, 16 * 1024, Duration::from_secs(1), None);
        let commits = planner.count(Kind::Commits, MAX_UNITS, None);
        assert!(planner.count(Kind::CommitTrees, MAX_UNITS, None) < commits);
        planner.observe(
            Kind::CommitTrees,
            1,
            BATCH_BYTES,
            Duration::from_secs(1),
            None,
        );
        assert!(planner.count(Kind::CommitTrees, MAX_UNITS, None) < commits);
        assert_eq!(planner.count(Kind::Commits, MAX_UNITS, None), commits);
    }
}
