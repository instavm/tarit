#[derive(Debug, Clone, Copy, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TtiStats {
    pub median: u64,
    pub p95: u64,
    pub p99: u64,
    pub min: u64,
    pub max: u64,
}

pub fn summarize(samples: &[u64]) -> Option<TtiStats> {
    if samples.is_empty() {
        return None;
    }

    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let min = *sorted.first()?;
    let max = *sorted.last()?;
    let trimmed = trim(&sorted);

    Some(TtiStats {
        median: nearest_rank(trimmed, 50.0),
        p95: nearest_rank(trimmed, 95.0),
        p99: nearest_rank(trimmed, 99.0),
        min,
        max,
    })
}

pub fn composite_score(stats: Option<TtiStats>, success_rate: f64) -> f64 {
    let Some(stats) = stats else {
        return 0.0;
    };

    let median_score = metric_score(stats.median);
    let p95_score = metric_score(stats.p95);
    let p99_score = metric_score(stats.p99);
    let timing_score = 0.60 * median_score + 0.25 * p95_score + 0.15 * p99_score;
    timing_score * success_rate
}

fn trim(sorted: &[u64]) -> &[u64] {
    if sorted.len() < 20 {
        return sorted;
    }

    let trim_count = ((sorted.len() as f64) * 0.05).ceil() as usize;
    let end = sorted.len().saturating_sub(trim_count);
    if trim_count >= end {
        sorted
    } else {
        &sorted[trim_count..end]
    }
}

fn nearest_rank(sorted: &[u64], percentile: f64) -> u64 {
    let rank = ((percentile / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

fn metric_score(value_ms: u64) -> f64 {
    (100.0 * (1.0 - (value_ms as f64 / 10_000.0))).clamp(0.0, 100.0)
}

#[cfg(test)]
mod tests {
    use super::summarize;

    #[test]
    fn skips_trim_for_small_sets() {
        let stats = summarize(&[10, 20, 30]).unwrap();
        assert_eq!(stats.median, 20);
        assert_eq!(stats.p95, 30);
        assert_eq!(stats.min, 10);
        assert_eq!(stats.max, 30);
    }

    #[test]
    fn trims_five_percent_for_large_sets() {
        let samples = (1..=100).collect::<Vec<_>>();
        let stats = summarize(&samples).unwrap();
        assert_eq!(stats.min, 1);
        assert_eq!(stats.max, 100);
        assert_eq!(stats.median, 50);
        assert_eq!(stats.p95, 91);
        assert_eq!(stats.p99, 95);
    }
}
