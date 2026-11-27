//! History commands' math: pure functions over a user's listens, oldest first. Times are Unix seconds; day
//! numbers count from 1970-01-01 in whatever time zone the caller turned them into (see `tz`).

use std::{cmp::Reverse, collections::{BTreeMap, HashMap}};

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

/// Each key's most played (track, album) pair, ties to the lowest ids: the names a chart looks its cover up by.
pub fn top_pairs(entries: &[Entry], key: impl Fn(&Entry) -> Option<i32>) -> HashMap<i32, (i32, i32)> {
    let mut counts: HashMap<(i32, i32, i32), u32> = HashMap::new();
    for e in entries {
        if let Some(k) = key(e) {
            *counts.entry((k, e.track, e.album)).or_default() += 1;
        }
    }
    let mut best: HashMap<i32, (u32, i32, i32)> = HashMap::new();
    for ((k, track, album), n) in counts {
        let b = best.entry(k).or_insert((n, track, album));
        if (n, std::cmp::Reverse((track, album))) > (b.0, std::cmp::Reverse((b.1, b.2))) {
            *b = (n, track, album);
        }
    }
    best.into_iter().map(|(k, (_, track, album))| (k, (track, album))).collect()
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

/// Day number of a date (Howard Hinnant's `days_from_civil`). A day past the month's end rolls over (Feb 30 → Mar 1).
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    // Years start in March here, so a leap day is the last day of the year.
    let doy = (153 * ((i64::from(m) + 9) % 12) + 2) / 5 + i64::from(d) - 1;
    era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468
}

/// The same date as day `today` in each earlier year since `first_year`, newest first: (year, day).
/// February 29th only exists in leap years.
pub fn same_days(today: i64, first_year: i64) -> Vec<(i64, i64)> {
    let (y, m, d) = civil(today);
    (first_year..y).rev().map(|yy| (yy, days_from_civil(yy, m, d))).filter(|&(yy, day)| civil(day) == (yy, m, d)).collect()
}

/// Alistral's overdue factor. For keys played at least `min` times: the usual gap is (now - first play) / plays,
/// and the factor is how many of those gaps have passed since the last play. Over 1 means it's late.
/// (key, factor, last play, usual gap in seconds), most overdue first.
pub fn overdue(entries: &[Entry], key: impl Fn(&Entry) -> Option<i32>, now: i64, min: u32) -> Vec<(i32, f64, i64, i64)> {
    let mut seen: HashMap<i32, (i64, i64, u32)> = HashMap::new();
    for e in entries {
        if let Some(k) = key(e) {
            let s = seen.entry(k).or_insert((e.ts, e.ts, 0));
            s.1 = e.ts;
            s.2 += 1;
        }
    }
    let mut v: Vec<_> = seen
        .into_iter()
        .filter(|(_, s)| s.2 >= min)
        .filter_map(|(k, (first, last, n))| {
            let gap = (now - first) / i64::from(n);
            (gap > 0).then(|| (k, (now - last) as f64 / gap as f64, last, gap))
        })
        .filter(|r| r.1 > 1.0)
        .collect();
    v.sort_unstable_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    v
}

/// Keys played in [from, to) after a break: not played in the same length of time before `from`, but played
/// earlier. (key, plays in the period, last play before the break), most played first.
pub fn comebacks(entries: &[Entry], key: impl Fn(&Entry) -> Option<i32>, from: i64, to: i64) -> Vec<(i32, u32, i64)> {
    let away = from - (to - from);
    let mut last_before: HashMap<i32, i64> = HashMap::new();
    for e in range(entries, i64::MIN, from) {
        if let Some(k) = key(e) {
            last_before.insert(k, e.ts);
        }
    }
    top(range(entries, from, to), &key)
        .into_iter()
        .filter_map(|(k, n)| last_before.get(&k).filter(|t| **t < away).map(|t| (k, n, *t)))
        .collect()
}

/// Rank changes in the all-time top since `from`, for today's top `limit`: (key, rank now, rank at `from`, plays
/// since). Ranks start at 1; `None` = not played before. Only keys that climbed, biggest climb first; a new key
/// climbs from just below the old list.
pub fn movers(entries: &[Entry], key: impl Fn(&Entry) -> Option<i32>, from: i64, limit: usize) -> Vec<(i32, usize, Option<usize>, u32)> {
    let before = top(range(entries, i64::MIN, from), &key);
    let rank_before: HashMap<i32, usize> = before.iter().enumerate().map(|(i, (k, _))| (*k, i + 1)).collect();
    let since: HashMap<i32, u32> = top(range(entries, from, i64::MAX), &key).into_iter().collect();
    let mut v: Vec<_> = top(entries, &key)
        .into_iter()
        .take(limit)
        .enumerate()
        .filter_map(|(i, (k, _))| {
            let old = rank_before.get(&k).copied();
            old.is_none_or(|o| o > i + 1).then(|| (k, i + 1, old, since.get(&k).copied().unwrap_or(0)))
        })
        .collect();
    v.sort_unstable_by_key(|(k, now, old, _)| (Reverse(old.unwrap_or(before.len() + 1).saturating_sub(*now)), *k));
    v
}

/// Each key's biggest day: (key, day, plays that day), biggest first. `day` turns a time into a day number.
pub fn binges(entries: &[Entry], key: impl Fn(&Entry) -> Option<i32>, day: impl Fn(i64) -> i64) -> Vec<(i32, i64, u32)> {
    let mut per_day: HashMap<(i32, i64), u32> = HashMap::new();
    for e in entries {
        if let Some(k) = key(e) {
            *per_day.entry((k, day(e.ts))).or_default() += 1;
        }
    }
    let mut best: HashMap<i32, (i64, u32)> = HashMap::new();
    for ((k, d), n) in per_day {
        let b = best.entry(k).or_insert((d, 0));
        if n > b.1 || (n == b.1 && d < b.0) {
            *b = (d, n);
        }
    }
    let mut v: Vec<_> = best.into_iter().map(|(k, (d, n))| (k, d, n)).collect();
    v.sort_unstable_by_key(|(k, d, n)| (Reverse(*n), *d, *k));
    v
}

/// Artists with at least `min` plays, at least `share` of them one track: (artist, track, track plays, artist
/// plays), most played first.
pub fn one_track_artists(entries: &[Entry], artist_of: impl Fn(i32) -> Option<i32>, min: u32, share: f64) -> Vec<(i32, i32, u32, u32)> {
    let mut artists: HashMap<i32, (u32, i32, u32)> = HashMap::new();
    // `top` is sorted by plays, so each artist's first track is its most played.
    for (track, n) in top(entries, |e| Some(e.track)) {
        let Some(a) = artist_of(track) else { continue };
        let row = artists.entry(a).or_insert((0, track, n));
        row.0 += n;
    }
    let mut v: Vec<_> = artists
        .into_iter()
        .filter(|(_, (total, _, n))| *total >= min && f64::from(*n) >= share * f64::from(*total))
        .map(|(a, (total, t, n))| (a, t, n, total))
        .collect();
    v.sort_unstable_by_key(|(a, _, _, total)| (Reverse(*total), *a));
    v
}

/// Plays per local weekday (Monday first) and hour. `local` turns a time into local wall-clock seconds.
pub fn clock(entries: &[Entry], local: impl Fn(i64) -> i64) -> [[u32; 24]; 7] {
    let mut grid = [[0u32; 24]; 7];
    for e in entries {
        let t = local(e.ts);
        // 1970-01-01 was a Thursday.
        let weekday = (t.div_euclid(DAY) + 3).rem_euclid(7) as usize;
        grid[weekday][(t.rem_euclid(DAY) / 3600) as usize] += 1;
    }
    grid
}

/// Plays per bucket, e.g. per year: `bucket` turns a time into the bucket's number.
pub fn buckets(entries: &[Entry], bucket: impl Fn(i64) -> i64) -> BTreeMap<i64, u32> {
    let mut out = BTreeMap::new();
    for e in entries {
        *out.entry(bucket(e.ts)).or_default() += 1;
    }
    out
}

/// 0 (everybody knows them) to 100 (nobody does): the play-weighted mean of each artist's place on a log scale
/// from 100,000 LB listeners (0, the iceberg's tip) down to 1 listener (100). `(plays, listeners)` in.
pub fn underground(artists: &[(u32, i64)]) -> f64 {
    let total: f64 = artists.iter().map(|a| f64::from(a.0)).sum();
    if total == 0.0 {
        return 0.0;
    }
    let depth = |listeners: i64| (100.0 * (1.0 - (listeners.max(1) as f64).log10() / 5.0)).clamp(0.0, 100.0);
    artists.iter().map(|&(plays, l)| f64::from(plays) * depth(l)).sum::<f64>() / total
}

/// Alistral's underrated score: up to 100 for a high place in your own top (`rank` 0 = your most played, 1000 and
/// below = 0) plus up to 100 for the share of all ListenBrainz listens that are yours. LB's count includes yours;
/// when it lags behind (0 or fewer than yours) you count as the only listener.
pub fn underrated(rank: usize, yours: i64, everyone: i64) -> f64 {
    let share = if everyone > 0 { (yours as f64 / everyone as f64).min(1.0) } else { 1.0 };
    1000usize.saturating_sub(rank) as f64 / 10.0 + share * 100.0
}

/// The play-weighted average track length of `entries` from the known lengths (ms), and the share of plays that
/// had one. `None` when no length is known.
pub fn average_length(entries: &[Entry], length: impl Fn(i32) -> Option<i64>) -> Option<(f64, f64)> {
    let known: Vec<i64> = entries.iter().filter_map(|e| length(e.track)).collect();
    (!known.is_empty()).then(|| (known.iter().sum::<i64>() as f64 / known.len() as f64, known.len() as f64 / entries.len() as f64))
}

/// Time per key in ms, most first; `length` gives each play's length.
pub fn time_by(entries: &[Entry], key: impl Fn(&Entry) -> Option<i32>, length: impl Fn(&Entry) -> i64) -> Vec<(i32, i64)> {
    let mut out: HashMap<i32, i64> = HashMap::new();
    for e in entries {
        if let Some(k) = key(e) {
            *out.entry(k).or_default() += length(e);
        }
    }
    let mut v: Vec<_> = out.into_iter().collect();
    v.sort_unstable_by_key(|(k, ms)| (Reverse(*ms), *k));
    v
}

/// Plays per decade of release (1990 = the 1990s) and the play-weighted mean release year. `(year, plays)` in.
pub fn decades(rows: &[(i32, u32)]) -> (BTreeMap<i32, u32>, Option<f64>) {
    let mut out = BTreeMap::new();
    for &(year, plays) in rows {
        *out.entry(year / 10 * 10).or_default() += plays;
    }
    let plays: u32 = rows.iter().map(|r| r.1).sum();
    let mean = (plays > 0).then(|| rows.iter().map(|&(y, n)| f64::from(y) * f64::from(n)).sum::<f64>() / f64::from(plays));
    (out, mean)
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
    fn top_pairs_pick_the_most_played_then_lowest_ids() {
        let p = |track, album| Entry { ts: 0, track, album };
        // Album 7: track 2 twice beats track 1 once; album 8: a tie goes to the lower track id.
        let h = [p(1, 7), p(2, 7), p(2, 7), p(5, 8), p(4, 8)];
        let pairs = top_pairs(&h, |x| Some(x.album));
        assert_eq!((pairs[&7], pairs[&8]), ((2, 7), (4, 8)));
        // By track: the album it was played from most.
        let h = [p(3, 9), p(3, 6), p(3, 9)];
        assert_eq!(top_pairs(&h, |x| Some(x.track))[&3], (3, 9));
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
        assert_eq!(days_from_civil(2024, 1, 1) * DAY, 1_704_067_200);
        assert_eq!(days_from_civil(2000, 1, 1) * DAY, 946_684_800);
        assert_eq!(days_from_civil(2024, 2, 29), 19_782);
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        // Day 19_782 is 2024-02-29: earlier leap years only.
        assert_eq!(same_days(19_782, 2015), [(2020, days_from_civil(2020, 2, 29)), (2016, days_from_civil(2016, 2, 29))]);
        assert_eq!(same_days(19_723, 2022).iter().map(|d| d.0).collect::<Vec<_>>(), [2023, 2022]);
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
    fn alistral_stats() {
        let day = |d: i64| d * DAY;
        // Track 1: 10 plays over days 0-9 (usually every 3 days by day 30), track 2: 5 plays on day 0 (every 6 days).
        let mut h: Vec<Entry> = (0..10).map(|d| e(day(d), 1)).collect();
        h.extend((0..5).map(|i| e(i, 2)));
        h.sort_by_key(|x| x.ts);
        let key = |x: &Entry| Some(x.track);
        let od = overdue(&h, key, day(30), 5);
        assert_eq!(od.iter().map(|r| r.0).collect::<Vec<_>>(), [1, 2]);
        assert!((od[0].1 - 7.0).abs() < 1e-9, "21 days since, usually every 3 days: {od:?}");
        assert!(overdue(&h, key, day(30), 11).is_empty(), "fewer plays than the minimum");

        // Track 3: played on day 0, away, back on days 60-61; track 4 played all along.
        let h = [e(0, 3), e(day(1), 4), e(day(50), 4), e(day(60), 3), e(day(61), 3), e(day(61), 4)];
        assert_eq!(comebacks(&h, key, day(55), day(65)), [(3, 2, 0)]);
        assert!(comebacks(&h, key, day(10), day(65)).is_empty(), "the break must be as long as the period");

        let h = [e(1, 5), e(2, 5), e(3, 6), e(10, 7), e(11, 7), e(12, 7), e(13, 6), e(14, 6)];
        // Before 10: 5 (2 plays), 6 (1). Now: 6 (3), 7 (3), 5 (2).
        assert_eq!(movers(&h, key, 10, 10), [(6, 1, Some(2), 2), (7, 2, None, 3)]);
        assert_eq!(movers(&h, key, 100, 10), [], "nothing moved without new plays");

        let h = [e(0, 1), e(10, 1), e(day(1), 2), e(day(1) + 5, 2), e(day(1) + 9, 2), e(day(2), 1)];
        assert_eq!(binges(&h, key, |t| t.div_euclid(DAY)), [(2, 1, 3), (1, 0, 2)]);
        let artist_of = |t: i32| Some(if t == 1 { 100 } else { 200 });
        let h = [e(0, 1), e(1, 1), e(2, 1), e(3, 1), e(4, 2)];
        assert_eq!(one_track_artists(&h, artist_of, 2, 0.8), [(100, 1, 4, 4)]);

        // 1970-01-05 was a Monday; 13:00 UTC is 16:00 three hours ahead.
        let grid = clock(&[e(day(4) + 13 * 3600, 1)], |t| t + 3 * 3600);
        assert_eq!(grid[0][16], 1);
        assert_eq!(grid.iter().flatten().sum::<u32>(), 1);
        assert_eq!(buckets(&[e(0, 1), e(day(400), 1), e(day(401), 1)], |t| civil(t.div_euclid(DAY)).0), BTreeMap::from([(1970, 1), (1971, 2)]));

        assert_eq!(underground(&[(10, 100_000)]), 0.0);
        assert_eq!(underground(&[(10, 1)]), 100.0);
        assert!((underground(&[(1, 1), (1, 100_000)]) - 50.0).abs() < 1e-9);
        assert_eq!(underground(&[]), 0.0);
        assert!((underrated(0, 50, 100) - 150.0).abs() < 1e-9);
        assert!((underrated(2000, 10, 0) - 100.0).abs() < 1e-9, "no LB count yet: the only listener");
        assert!((underrated(999, 300, 100) - 100.1).abs() < 1e-9, "share capped at 100%");

        let h = [e(0, 1), e(1, 1), e(2, 2)];
        let len = |t: i32| (t == 1).then_some(200_000);
        assert_eq!(average_length(&h, len), Some((200_000.0, 2.0 / 3.0)));
        assert_eq!(average_length(&h, |_| None), None);
        assert_eq!(time_by(&h, key, |x| len(x.track).unwrap_or(100_000)), [(1, 400_000), (2, 100_000)]);
        let (by_decade, mean) = decades(&[(1994, 3), (1999, 1), (2012, 4)]);
        assert_eq!(by_decade, BTreeMap::from([(1990, 4), (2010, 4)]));
        assert!((mean.unwrap() - 2003.625).abs() < 1e-9);
    }

    #[test]
    fn affinity_is_symmetric_overlap() {
        let a = HashMap::from([(1, 50), (2, 50)]);
        let b = HashMap::from([(1, 10)]);
        assert!((affinity(&a, 100, &b, 10) - 0.5).abs() < 1e-9);
        assert!((affinity(&a, 100, &a, 100) - 1.0).abs() < 1e-9);
    }
}
