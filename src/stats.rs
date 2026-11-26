//! History commands' math: pure functions over a user's listens, oldest first. Days are UTC.

use std::collections::HashMap;

use crate::history::Entry;

pub const DAY: i64 = 86_400;

/// Plays per key, most first (ties by key, so the output is stable).
pub fn top(entries: &[Entry], key: impl Fn(&Entry) -> Option<i32>) -> Vec<(i32, u32)> {
    let mut counts: HashMap<i32, u32> = HashMap::new();
    for k in entries.iter().filter_map(&key) {
        *counts.entry(k).or_default() += 1;
    }
    let mut v: Vec<_> = counts.into_iter().collect();
    v.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    v
}

/// The listens in [from, to).
pub fn range(entries: &[Entry], from: i64, to: i64) -> &[Entry] {
    let a = entries.partition_point(|e| e.ts < from);
    let b = entries.partition_point(|e| e.ts < to);
    &entries[a..b.max(a)]
}

/// The current streak: (key, how many of the newest listens share it, when it started).
pub fn streak(entries: &[Entry], key: impl Fn(&Entry) -> Option<i32>) -> Option<(i32, usize, i64)> {
    let k = key(entries.last()?)?;
    let n = entries.iter().rev().take_while(|e| key(e) == Some(k)).count();
    Some((k, n, entries[entries.len() - n].ts))
}

/// The longest run of each key, at least `min` long, longest first: (key, length, start).
pub fn streaks(entries: &[Entry], key: impl Fn(&Entry) -> Option<i32>, min: usize) -> Vec<(i32, usize, i64)> {
    let mut best: HashMap<i32, (usize, i64)> = HashMap::new();
    let mut i = 0;
    while i < entries.len() {
        let k = key(&entries[i]);
        let j = i + entries[i..].iter().take_while(|e| key(e) == k).count();
        if let Some(k) = k {
            let b = best.entry(k).or_insert((0, 0));
            if j - i > b.0 {
                *b = (j - i, entries[i].ts);
            }
        }
        i = j;
    }
    let mut v: Vec<_> = best.into_iter().filter(|(_, (n, _))| *n >= min).map(|(k, (n, t))| (k, n, t)).collect();
    v.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.2.cmp(&b.2)));
    v
}

/// Each key's longest pause between two plays, longest first: (key, seconds, when they came back).
pub fn gaps(entries: &[Entry], key: impl Fn(&Entry) -> Option<i32>) -> Vec<(i32, i64, i64)> {
    let mut last: HashMap<i32, i64> = HashMap::new();
    let mut best: HashMap<i32, (i64, i64)> = HashMap::new();
    for e in entries {
        let Some(k) = key(e) else { continue };
        if let Some(prev) = last.insert(k, e.ts) {
            let b = best.entry(k).or_insert((0, 0));
            if e.ts - prev > b.0 {
                *b = (e.ts - prev, e.ts);
            }
        }
    }
    let mut v: Vec<_> = best.into_iter().map(|(k, (g, t))| (k, g, t)).collect();
    v.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    v
}

/// When each key was first played.
pub fn first_plays(entries: &[Entry], key: impl Fn(&Entry) -> Option<i32>) -> HashMap<i32, i64> {
    let mut first = HashMap::new();
    for e in entries {
        if let Some(k) = key(e) {
            first.entry(k).or_insert(e.ts);
        }
    }
    first
}

/// fmbot's period words: (days back, label); `None` days = all time. `None` when `word` isn't a period.
pub fn period(word: &str) -> Option<(Option<i64>, String)> {
    let days = |d: i64, label: &str| Some((Some(d), label.to_owned()));
    match word.to_lowercase().as_str() {
        "w" | "week" | "weekly" => days(7, "week"),
        "m" | "month" | "monthly" => days(30, "month"),
        "q" | "quarter" | "quarterly" => days(90, "quarter"),
        "h" | "half" | "halfyear" => days(182, "half year"),
        "y" | "year" | "yearly" => days(365, "year"),
        "a" | "at" | "all" | "alltime" | "overall" => Some((None, "all time".to_owned())),
        w => {
            let n: i64 = w.strip_suffix('d')?.parse().ok().filter(|n| (1..=3650).contains(n))?;
            Some((Some(n), format!("{n} days")))
        }
    }
}

/// (year, month, day) of a day number counted from 1970-01-01 (Howard Hinnant's `civil_from_days`).
pub fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// Unix time of January 1st of `year`, 00:00 UTC.
pub fn year_start(year: i64) -> i64 {
    let y = year - 1;
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    // March 1st based: January 1st is day 306 of the previous March-year.
    let doe = 365 * yoe + yoe / 4 - yoe / 100 + 306;
    (era * 146_097 + doe - 719_468) * DAY
}

/// The next round number above `n` for pace goals: the next 1,000 below 10,000, else the next 10,000.
pub fn next_goal(n: i64, small: bool) -> i64 {
    let step = match (small, n) {
        (true, n) if n < 100 => 10,
        (true, n) if n < 1000 => 100,
        (_, n) if n < 10_000 => 1000,
        _ => 10_000,
    };
    (n / step + 1) * step
}

/// The largest round milestone reached: 1, 10, 100, 1000…, then every 10,000.
pub fn last_milestone(n: i64) -> i64 {
    match n {
        n if n < 1 => 0,
        n if n < 10_000 => 10i64.pow(n.ilog10()) * (n / 10i64.pow(n.ilog10())),
        n => n / 10_000 * 10_000,
    }
}

/// Iceberg tiers by ListenBrainz listener count, top of the iceberg first.
pub const TIERS: [(i64, &str); 6] = [
    (100_000, "The tip: everybody knows them"),
    (25_000, "Just below the surface"),
    (5_000, "Getting deeper"),
    (1_000, "Deep water"),
    (200, "The depths"),
    (0, "The abyss"),
];

pub fn tier(listeners: i64) -> usize {
    TIERS.iter().position(|(min, _)| listeners >= *min).unwrap_or(TIERS.len() - 1)
}

/// How much two listeners' artist shares overlap, 0..=1: the sum over artists of the smaller share.
pub fn affinity(a: &HashMap<i32, i64>, a_total: i64, b: &HashMap<i32, i64>, b_total: i64) -> f64 {
    if a_total <= 0 || b_total <= 0 {
        return 0.0;
    }
    a.iter()
        .filter_map(|(k, pa)| b.get(k).map(|pb| (*pa as f64 / a_total as f64).min(*pb as f64 / b_total as f64)))
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(ts: i64, track: i32) -> Entry {
        Entry { ts, track, album: 0 }
    }

    #[test]
    fn runs_gaps_and_tops() {
        let h = [e(10, 1), e(20, 1), e(30, 2), e(40, 1), e(50, 1), e(60, 1), e(1000, 2)];
        let key = |x: &Entry| Some(x.track);
        assert_eq!(top(&h, key), [(1, 5), (2, 2)]);
        assert_eq!(streak(&h, key), Some((2, 1, 1000)));
        assert_eq!(streak(&h[..6], key), Some((1, 3, 40)));
        assert_eq!(streaks(&h, key, 2), [(1, 3, 40)]);
        assert_eq!(gaps(&h, key), [(2, 970, 1000), (1, 20, 40)]);
        assert_eq!(range(&h, 20, 50).len(), 3);
        assert_eq!(range(&h, 50, 20).len(), 0);
        assert_eq!(first_plays(&h, key)[&2], 30);
    }

    #[test]
    fn dates_and_goals() {
        assert_eq!(civil(0), (1970, 1, 1));
        assert_eq!(civil(19_723), (2024, 1, 1));
        assert_eq!(civil(19_782), (2024, 2, 29));
        assert_eq!(year_start(1970), 0);
        assert_eq!(year_start(2024), 1_704_067_200);
        assert_eq!(year_start(2000), 946_684_800);
        assert_eq!(next_goal(12_345, false), 20_000);
        assert_eq!(next_goal(999, false), 1000);
        assert_eq!(next_goal(42, true), 50);
        assert_eq!(last_milestone(12_345), 10_000);
        assert_eq!(last_milestone(4_321), 4_000);
        assert_eq!(last_milestone(7), 7);
        assert_eq!(period("30d"), Some((Some(30), "30 days".into())));
        assert_eq!(period("alltime"), Some((None, "all time".into())));
        assert_eq!(period("radiohead"), None);
        assert_eq!(tier(2_000_000), 0);
        assert_eq!(tier(3), 5);
    }

    #[test]
    fn affinity_is_symmetric_overlap() {
        let a = HashMap::from([(1, 50), (2, 50)]);
        let b = HashMap::from([(1, 10)]);
        assert!((affinity(&a, 100, &b, 10) - 0.5).abs() < 1e-9);
        assert!((affinity(&a, 100, &a, 100) - 1.0).abs() < 1e-9);
    }
}
