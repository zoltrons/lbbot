//! Time zones. Discord doesn't tell bots a user's time zone, so, like fmbot's `/localization`, users pick one with
//! `/timezone`, whose autocomplete suggests the zones of their Discord language. Days, hours and years in the
//! history commands follow it; without one they are UTC.

use jiff::{Timestamp, civil, tz::TimeZone};

use crate::stats::{DAY, civil};

#[derive(Clone, Debug, Default)]
pub struct Tz(Option<TimeZone>);

impl Tz {
    /// `None` or a name the database doesn't know = UTC.
    pub fn new(name: Option<&str>) -> Self {
        Self(name.and_then(|n| TimeZone::get(n).ok()))
    }

    pub fn is_set(&self) -> bool {
        self.0.is_some()
    }

    pub fn name(&self) -> &str {
        self.0.as_ref().and_then(TimeZone::iana_name).unwrap_or("UTC")
    }

    /// Seconds ahead of UTC at Unix time `ts`.
    pub fn offset(&self, ts: i64) -> i64 {
        match (&self.0, Timestamp::from_second(ts)) {
            (Some(tz), Ok(t)) => i64::from(tz.to_offset(t).seconds()),
            _ => 0,
        }
    }

    /// Local wall-clock time as seconds since 1970-01-01 00:00 local.
    pub fn local(&self, ts: i64) -> i64 {
        ts + self.offset(ts)
    }

    /// Local day number, counted from 1970-01-01.
    pub fn day(&self, ts: i64) -> i64 {
        self.local(ts).div_euclid(DAY)
    }

    /// Unix time when local day `day` starts (the first valid time when midnight is skipped for summer time).
    pub fn start(&self, day: i64) -> i64 {
        let (y, m, d) = civil(day);
        let at = |tz: &TimeZone| {
            let date = civil::Date::new(i16::try_from(y).ok()?, m as i8, d as i8).ok()?;
            Some(tz.to_timestamp(date.at(0, 0, 0, 0)).ok()?.as_second())
        };
        self.0.as_ref().and_then(at).unwrap_or(day * DAY)
    }
}

/// Zones for each Discord language (`locale`), most populous first. Discord has a fixed list of languages.
const LOCALE_ZONES: [(&str, &[&str]); 32] = [
    ("bg", &["Europe/Sofia"]),
    ("cs", &["Europe/Prague"]),
    ("da", &["Europe/Copenhagen"]),
    ("de", &["Europe/Berlin", "Europe/Vienna", "Europe/Zurich"]),
    ("el", &["Europe/Athens"]),
    ("en-GB", &["Europe/London", "Europe/Dublin"]),
    ("en-US", &["America/New_York", "America/Chicago", "America/Denver", "America/Los_Angeles", "America/Anchorage", "Pacific/Honolulu"]),
    ("es-419", &["America/Mexico_City", "America/Bogota", "America/Argentina/Buenos_Aires", "America/Lima", "America/Santiago"]),
    ("es-ES", &["Europe/Madrid", "Atlantic/Canary"]),
    ("fi", &["Europe/Helsinki"]),
    ("fr", &["Europe/Paris", "Europe/Brussels", "America/Toronto"]),
    ("hi", &["Asia/Kolkata"]),
    ("hr", &["Europe/Zagreb"]),
    ("hu", &["Europe/Budapest"]),
    ("id", &["Asia/Jakarta", "Asia/Makassar", "Asia/Jayapura"]),
    ("it", &["Europe/Rome"]),
    ("ja", &["Asia/Tokyo"]),
    ("ko", &["Asia/Seoul"]),
    ("lt", &["Europe/Vilnius"]),
    ("nl", &["Europe/Amsterdam", "Europe/Brussels"]),
    ("no", &["Europe/Oslo"]),
    ("pl", &["Europe/Warsaw"]),
    ("pt-BR", &["America/Sao_Paulo", "America/Manaus", "America/Recife", "Europe/Lisbon"]),
    ("ro", &["Europe/Bucharest"]),
    ("ru", &["Europe/Moscow", "Asia/Yekaterinburg", "Asia/Novosibirsk", "Asia/Krasnoyarsk", "Asia/Vladivostok"]),
    ("sv-SE", &["Europe/Stockholm"]),
    ("th", &["Asia/Bangkok"]),
    ("tr", &["Europe/Istanbul"]),
    ("uk", &["Europe/Kyiv"]),
    ("vi", &["Asia/Ho_Chi_Minh"]),
    ("zh-CN", &["Asia/Shanghai"]),
    ("zh-TW", &["Asia/Taipei"]),
];

/// Every IANA name of the form `Area/City`, plus UTC.
fn names() -> Vec<String> {
    let mut v: Vec<String> = jiff::tz::db()
        .available()
        .map(|n| n.as_str().to_owned())
        .filter(|n| n.contains('/') && !n.starts_with("Etc/") && !n.starts_with("SystemV/") && !n.starts_with("US/"))
        .collect();
    v.push("UTC".into());
    v.sort_unstable();
    v
}

/// "new york" → "new_york": IANA names use underscores.
fn normalize(q: &str) -> String {
    q.trim().to_lowercase().split_whitespace().collect::<Vec<_>>().join("_")
}

fn city(name: &str) -> String {
    name.rsplit('/').next().unwrap_or(name).to_lowercase()
}

/// Whether two zones keep the same clock now and over the next year (an old alias such as Asia/Istanbul).
fn same_clock(a: &str, b: &str) -> bool {
    let (a, b) = (Tz::new(Some(a)), Tz::new(Some(b)));
    let now = Timestamp::now().as_second();
    (0..4).all(|q| a.offset(now + q * 91 * DAY) == b.offset(now + q * 91 * DAY))
}

/// The zone `query` means: an IANA name in any case, or a city ("istanbul", "New York"). `None` when nothing
/// matches, or several cities with different clocks do.
pub fn find(query: &str) -> Option<String> {
    let q = normalize(query);
    let all = names();
    if let Some(n) = all.iter().find(|n| n.to_lowercase() == q) {
        return Some(n.clone());
    }
    let hits: Vec<&String> = all.iter().filter(|n| city(n) == q).collect();
    let first = *hits.first()?;
    if !hits.iter().all(|h| same_clock(first, h)) {
        return None;
    }
    // Aliases keep the same clock; prefer the name a Discord language lists (Europe/Istanbul over Asia/Istanbul).
    let listed = hits.iter().find(|h| LOCALE_ZONES.iter().any(|(_, zones)| zones.contains(&h.as_str())));
    Some((*listed.unwrap_or(&first)).clone())
}

/// Up to 25 zones (Discord's limit for autocomplete) for what the user typed: names whose city starts with it
/// first, then names containing it. With nothing typed, the zones of the user's and the server's Discord language.
pub fn suggest(query: &str, locales: &[&str]) -> Vec<String> {
    let q = normalize(query);
    if q.is_empty() {
        let mut out: Vec<String> = Vec::new();
        for zone in locales.iter().filter_map(|l| LOCALE_ZONES.iter().find(|(k, _)| k == l)).flat_map(|(_, z)| z.iter()) {
            if !out.iter().any(|o| o == zone) {
                out.push((*zone).to_owned());
            }
        }
        out.push("UTC".into());
        return out;
    }
    let all = names();
    let mut out: Vec<String> = all.iter().filter(|n| city(n).starts_with(&q)).cloned().collect();
    out.extend(all.iter().filter(|n| !city(n).starts_with(&q) && n.to_lowercase().contains(&q)).cloned());
    out.truncate(25);
    out
}

/// "Europe/Istanbul (UTC+03:00)" for a choice list.
pub fn label(name: &str) -> String {
    let secs = Tz::new(Some(name)).offset(Timestamp::now().as_second());
    let sign = if secs < 0 { '-' } else { '+' };
    format!("{name} (UTC{sign}{:02}:{:02})", secs.abs() / 3600, secs.abs() % 3600 / 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_days_and_midnights() {
        let utc = Tz::new(None);
        let ist = Tz::new(Some("Europe/Istanbul"));
        // 2024-06-01 22:30 UTC is already June 2nd, 01:30 in Istanbul.
        let ts = 1_717_281_000;
        assert_eq!(utc.day(ts), 19_875);
        assert_eq!(ist.day(ts), 19_876);
        assert_eq!(ist.start(19_876), 19_876 * DAY - 3 * 3600);
        assert_eq!(ist.name(), "Europe/Istanbul");
        assert_eq!(utc.name(), "UTC");
        assert!(!Tz::new(Some("Mars/Olympus")).is_set(), "an unknown name is UTC");
        // New York: UTC-5 in winter, UTC-4 in summer.
        let ny = Tz::new(Some("America/New_York"));
        assert_eq!(ny.start(19_723), 19_723 * DAY + 5 * 3600, "2024-01-01");
        assert_eq!(ny.start(19_875), 19_875 * DAY + 4 * 3600, "2024-06-01");
    }

    #[test]
    fn finding_and_suggesting_zones() {
        assert_eq!(find("europe/istanbul").as_deref(), Some("Europe/Istanbul"));
        assert_eq!(find("istanbul").as_deref(), Some("Europe/Istanbul"), "the alias Asia/Istanbul keeps the same clock");
        assert_eq!(find(" New York ").as_deref(), Some("America/New_York"));
        assert_eq!(find("utc").as_deref(), Some("UTC"));
        assert_eq!(find("nowhere"), None);
        assert_eq!(suggest("", &["tr", "tr"]), ["Europe/Istanbul", "UTC"]);
        assert_eq!(suggest("", &["en-US"])[0], "America/New_York");
        assert!(suggest("berl", &[]).contains(&"Europe/Berlin".to_owned()));
        assert!(suggest("a", &[]).len() <= 25);
        assert_eq!(label("UTC"), "UTC (UTC+00:00)");
        assert_eq!(label("Asia/Kolkata"), "Asia/Kolkata (UTC+05:30)");
    }
}
