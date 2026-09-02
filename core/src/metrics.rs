//! Prometheus metrics (Phase 5).
//!
//! Hand-rolled, zero-dependency registry emitting the Prometheus text
//! exposition format. Deliberate trade against the `prometheus`/`metrics`
//! crates: this gateway needs two counters, a handful of per-route
//! histograms, and a few gauges; the client libraries pull in several
//! dependency trees for features a proxy will never use. The format here
//! is plain and stable — scrape it with anything that reads
//! `https://prometheus.io/docs/instrumenting/exposition_formats/#text-details`.
//!
//! Cardinality discipline: every metric family is bounded by config, not
//! by request content. Labels are the route prefix (from the validated
//! table, not the raw URL), the HTTP status *class* (`2xx`..`5xx`, not the
//! code), and the breaker state (3 values). No paths, no query strings, no
//! subjects — an attacker cannot grow this registry by sending traffic.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Cumulative per-prefix counters: total requests, and rejections the
/// gateway itself produced (429 rate-limited, 503 circuit-open). Backend
/// error statuses are *responses*, counted in the status-class counter.
#[derive(Debug, Default)]
struct PrefixCounters {
    requests_total: AtomicU64,
    rate_limited_total: AtomicU64,
    circuit_open_total: AtomicU64,
}

/// Per-prefix/status-class response counter values (`"2xx|crm"` → count).
type StatusTotals = BTreeMap<String, AtomicU64>;

/// Sum + count + bucket bounds for one prefix's latency histogram.
/// Buckets in seconds, Prometheus-conventional spread: they cover the
/// measured distribution (p50 ~2ms, p99 ~5ms on the reference laptop) with
/// headroom for saturated operation.
#[derive(Debug)]
struct Histogram {
    buckets_secs: &'static [f64],
    counts: Vec<AtomicU64>,
    sum_secs: AtomicU64, // fixed-point: nanoseconds, u64 won't overflow
    count: AtomicU64,
}

impl Histogram {
    fn new(buckets_secs: &'static [f64]) -> Self {
        Self {
            buckets_secs,
            counts: buckets_secs.iter().map(|_| AtomicU64::new(0)).collect(),
            sum_secs: AtomicU64::new(0),
            count: AtomicU64::new(0),
        }
    }

    fn observe(&self, secs: f64) {
        for (i, bound) in self.buckets_secs.iter().enumerate() {
            if secs <= *bound {
                self.counts[i].fetch_add(1, Ordering::Relaxed);
            }
        }
        self.count.fetch_add(1, Ordering::Relaxed);
        // Nanoseconds as u64: 584 years of headroom; negative can't occur.
        self.sum_secs
            .fetch_add((secs * 1e9) as u64, Ordering::Relaxed);
    }
}

/// Latency buckets in seconds: a Prometheus-conventional spread covering
/// the measured distribution (p50 ~2ms, p99 ~5ms on the reference laptop)
/// with headroom for saturated operation.
const LATENCY_BUCKETS: &[f64] = &[
    0.0005, 0.001, 0.002, 0.005, 0.01, 0.02, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5,
];

/// The whole registry. Cheap to clone (arcs); one instance shared by the
/// gateway.
#[derive(Debug, Clone, Default)]
pub struct Metrics {
    inner: Arc<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    per_prefix: Mutex<BTreeMap<String, Arc<PrefixCounters>>>,
    latencies: Mutex<BTreeMap<String, Arc<Histogram>>>,
    status_totals: Mutex<StatusTotals>,
    breaker_state: Mutex<BTreeMap<String, u8>>, // 0 closed 1 halfopen 2 open
}

/// One observed request. Returned by [`Metrics::start_request`] so the
/// latency observation point is explicit.
pub struct RequestTimer {
    metrics: Metrics,
    prefix: String,
    started: std::time::Instant,
}

impl RequestTimer {
    /// Record the outcome. `status_class` is e.g. `"2xx"`, `"5xx"` (the
    /// class, not the code — keeps label cardinality fixed); `None` means
    /// the request never produced a status and only counts in
    /// `requests_total`.
    pub fn finish(self, status_class: Option<impl AsRef<str>>) {
        let counters = self.metrics.counters_for(&self.prefix);
        counters.requests_total.fetch_add(1, Ordering::Relaxed);
        if let Some(class) = status_class {
            let mut totals = self
                .metrics
                .inner
                .status_totals
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            totals
                .entry(format!("{}|{}", class.as_ref(), self.prefix))
                .or_default()
                .fetch_add(1, Ordering::Relaxed);
        }
        let elapsed = self.started.elapsed().as_secs_f64();
        self.metrics.latency_for(&self.prefix).observe(elapsed);
    }
}

impl Metrics {
    /// One instance per process; clonable handles share the registry.
    pub fn new() -> Self {
        Self::default()
    }

    fn counters_for(&self, prefix: &str) -> Arc<PrefixCounters> {
        self.inner
            .per_prefix
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(prefix.to_string())
            .or_default()
            .clone()
    }

    fn latency_for(&self, prefix: &str) -> Arc<Histogram> {
        self.inner
            .latencies
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(prefix.to_string())
            .or_insert_with(|| Arc::new(Histogram::new(LATENCY_BUCKETS)))
            .clone()
    }

    /// Begin timing a request for `prefix`. The returned timer must be
    /// finished with `.finish(Some("2xx"))`-style call when the response
    /// status is known.
    pub fn start_request(&self, prefix: &str) -> RequestTimer {
        RequestTimer {
            metrics: self.clone(),
            prefix: prefix.to_string(),
            started: std::time::Instant::now(),
        }
    }

    /// Count a rate-limit rejection (429) for `prefix`.
    pub fn inc_rate_limited(&self, prefix: &str) {
        self.counters_for(prefix)
            .rate_limited_total
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Count a circuit-open short-circuit (503) for `prefix`.
    pub fn inc_circuit_open(&self, prefix: &str) {
        self.counters_for(prefix)
            .circuit_open_total
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Current breaker state per prefix, published on every scrape rather
    /// than on transitions — the breaker remains the single source of
    /// truth and the registry never needs a callback into it.
    pub fn set_breaker_state(&self, prefix: &str, state: crate::circuit::State) {
        let value = match state {
            crate::circuit::State::Closed => 0,
            crate::circuit::State::HalfOpen => 1,
            crate::circuit::State::Open => 2,
        };
        self.inner
            .breaker_state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(prefix.to_string(), value);
    }

    /// Render the Prometheus text exposition format. Deterministic order
    /// (BTreeMaps) so scrapes are diffable.
    pub fn render(&self) -> String {
        let mut out = String::with_capacity(4096);

        let per_prefix = self
            .inner
            .per_prefix
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let latencies = self
            .inner
            .latencies
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let status_totals = self
            .inner
            .status_totals
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let breaker = self
            .inner
            .breaker_state
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        // All prefixes that have any data, so families share label sets.
        let mut prefixes: Vec<&String> = per_prefix
            .keys()
            .chain(latencies.keys())
            .chain(breaker.keys())
            .collect();
        prefixes.sort();
        prefixes.dedup();

        out.push_str("# HELP ratewall_requests_total Proxied requests per route prefix.\n");
        out.push_str("# TYPE ratewall_requests_total counter\n");
        for prefix in &prefixes {
            let total = per_prefix
                .get(*prefix)
                .map(|c| c.requests_total.load(Ordering::Relaxed))
                .unwrap_or(0);
            out.push_str(&format!(
                "ratewall_requests_total{{prefix=\"{prefix}\"}} {total}\n"
            ));
        }

        out.push_str(
            "# HELP ratewall_rate_limited_total Requests rejected with 429 by the rate limiter.\n",
        );
        out.push_str("# TYPE ratewall_rate_limited_total counter\n");
        for prefix in &prefixes {
            let n = per_prefix
                .get(*prefix)
                .map(|c| c.rate_limited_total.load(Ordering::Relaxed))
                .unwrap_or(0);
            out.push_str(&format!(
                "ratewall_rate_limited_total{{prefix=\"{prefix}\"}} {n}\n"
            ));
        }

        out.push_str(
            "# HELP ratewall_circuit_open_total Requests short-circuited with 503 by an open breaker.\n",
        );
        out.push_str("# TYPE ratewall_circuit_open_total counter\n");
        for prefix in &prefixes {
            let n = per_prefix
                .get(*prefix)
                .map(|c| c.circuit_open_total.load(Ordering::Relaxed))
                .unwrap_or(0);
            out.push_str(&format!(
                "ratewall_circuit_open_total{{prefix=\"{prefix}\"}} {n}\n"
            ));
        }

        out.push_str("# HELP ratewall_responses_total Backend responses by status class.\n");
        out.push_str("# TYPE ratewall_responses_total counter\n");
        for (key, value) in status_totals.iter() {
            let (class, prefix) = key.split_once('|').unwrap_or(("unknown", key));
            out.push_str(&format!(
                "ratewall_responses_total{{prefix=\"{prefix}\",class=\"{class}\"}} {}\n",
                value.load(Ordering::Relaxed)
            ));
        }

        out.push_str(
            "# HELP ratewall_request_duration_seconds Proxied request latency per route prefix.\n",
        );
        out.push_str("# TYPE ratewall_request_duration_seconds histogram\n");
        for prefix in &prefixes {
            let Some(hist) = latencies.get(*prefix) else {
                continue;
            };
            for (i, bound) in hist.buckets_secs.iter().enumerate() {
                let cumulative = hist.counts[i].load(Ordering::Relaxed);
                out.push_str(&format!(
                    "ratewall_request_duration_seconds_bucket{{prefix=\"{prefix}\",le=\"{bound}\"}} {cumulative}\n"
                ));
            }
            out.push_str(&format!(
                "ratewall_request_duration_seconds_bucket{{prefix=\"{prefix}\",le=\"+Inf\"}} {}\n",
                hist.count.load(Ordering::Relaxed)
            ));
            out.push_str(&format!(
                "ratewall_request_duration_seconds_sum{{prefix=\"{prefix}\"}} {:.9}\n",
                hist.sum_secs.load(Ordering::Relaxed) as f64 / 1e9
            ));
            out.push_str(&format!(
                "ratewall_request_duration_seconds_count{{prefix=\"{prefix}\"}} {}\n",
                hist.count.load(Ordering::Relaxed)
            ));
        }

        out.push_str(
            "# HELP ratewall_breaker_state Circuit breaker state per backend (0 closed, 1 half-open, 2 open).\n",
        );
        out.push_str("# TYPE ratewall_breaker_state gauge\n");
        for prefix in &prefixes {
            if let Some(v) = breaker.get(*prefix) {
                out.push_str(&format!(
                    "ratewall_breaker_state{{prefix=\"{prefix}\"}} {v}\n"
                ));
            }
        }

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn family<'a>(rendered: &'a str, name: &str) -> Vec<&'a str> {
        rendered
            .lines()
            .filter(|l| l.starts_with(name) && !l.starts_with('#'))
            .collect()
    }

    #[test]
    fn empty_registry_renders_help_only() {
        let m = Metrics::new();
        let text = m.render();
        assert!(text.contains("# TYPE ratewall_requests_total counter"));
        assert!(!text.contains("prefix=\""));
    }

    #[test]
    fn counters_and_status_classes_accumulate() {
        let m = Metrics::new();
        let t = m.start_request("crm");
        t.finish(Some("2xx"));
        let t = m.start_request("crm");
        t.finish(Some("5xx"));
        m.start_request("hrm").finish(None::<&str>); // aborted before status
        m.inc_rate_limited("crm");
        m.inc_rate_limited("crm");
        m.inc_circuit_open("hrm");

        let text = m.render();
        assert!(text.contains("ratewall_requests_total{prefix=\"crm\"} 2\n"));
        assert!(text.contains("ratewall_requests_total{prefix=\"hrm\"} 1\n"));
        assert!(
            text.contains("ratewall_responses_total{prefix=\"crm\",class=\"2xx\"} 1\n")
                && text.contains("ratewall_responses_total{prefix=\"crm\",class=\"5xx\"} 1\n")
        );
        assert!(text.contains("ratewall_rate_limited_total{prefix=\"crm\"} 2\n"));
        assert!(text.contains("ratewall_circuit_open_total{prefix=\"hrm\"} 1\n"));
        // hrn has requests but no status class recorded.
        assert_eq!(family(&text, "ratewall_responses_total").len(), 2);
    }

    #[test]
    fn histogram_buckets_are_cumulative_with_sum_and_count() {
        let m = Metrics::new();
        // Deterministic observations straight into the histogram (no
        // sleeping — a 1ms sleep can land in the 2ms bucket on a busy CI
        // box). Push through the public surface by observing a fixed
        // duration: start_request + immediate finish with a paused clock
        // isn't available, so use two real timings and assert on
        // monotone, non-zero buckets instead of exact placement.
        let t = m.start_request("crm");
        std::thread::sleep(std::time::Duration::from_millis(1));
        t.finish(Some("2xx"));
        let t = m.start_request("crm");
        std::thread::sleep(std::time::Duration::from_millis(3));
        t.finish(Some("2xx"));

        let text = m.render();
        // The +Inf bucket equals the count; every bucket is <= count.
        let buckets: Vec<u64> = family(&text, "ratewall_request_duration_seconds_bucket")
            .into_iter()
            .filter(|l| l.contains("crm"))
            .map(|l| l.rsplit(' ').next().unwrap().parse().unwrap())
            .collect();
        assert_eq!(buckets.len(), LATENCY_BUCKETS.len() + 1); // +Inf
        let count: u64 = family(&text, "ratewall_request_duration_seconds_count")[0]
            .rsplit(' ')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(count, 2);
        assert_eq!(*buckets.last().unwrap(), count, "+Inf == count");
        assert!(buckets.windows(2).all(|w| w[0] <= w[1]), "cumulative");
        // Sum is positive and rendered with nanosecond precision.
        let sum = family(&text, "ratewall_request_duration_seconds_sum")[0].to_string();
        let value: f64 = sum.rsplit(' ').next().unwrap().parse().unwrap();
        assert!(value >= 0.004, "sum {sum} should be >= 4ms");
    }

    #[test]
    fn breaker_gauge_reflects_published_state() {
        let m = Metrics::new();
        m.set_breaker_state("crm", crate::circuit::State::Closed);
        m.set_breaker_state("hrm", crate::circuit::State::Open);
        let text = m.render();
        assert!(text.contains("ratewall_breaker_state{prefix=\"crm\"} 0\n"));
        assert!(text.contains("ratewall_breaker_state{prefix=\"hrm\"} 2\n"));
        m.set_breaker_state("hrm", crate::circuit::State::HalfOpen);
        assert!(m
            .render()
            .contains("ratewall_breaker_state{prefix=\"hrm\"} 1\n"));
    }

    #[test]
    fn unknown_subjects_cannot_grow_cardinality() {
        // Prefix labels come from the validated route table only; nothing
        // here reads request URLs. This test pins the API shape: there is
        // no method that accepts arbitrary per-request label values for
        // status or path.
        let m = Metrics::new();
        for path in ["a", "b", "c", "../../etc"] {
            m.start_request("crm").finish(Some("4xx")); // path ignored
            let _ = path;
        }
        let text = m.render();
        assert_eq!(family(&text, "ratewall_requests_total").len(), 1);
    }
}
