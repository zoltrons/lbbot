//! Pictures for `,growth`, `,decades` (one bar chart) and `,clock` (a weekday × hour heatmap): SVG drawn here,
//! rendered with `draw`. One series each, so one hue and no legend; the heading names what is counted.

use anyhow::Result;

use crate::draw::{DISPLAY, FALLBACK, MONO, SANS, clip, xml};

const W: f32 = 800.0;
const SURFACE: &str = "#191919";
const BAR: &str = "#338ced";
const INK: &str = "#ececec";
const MUTED: &str = "#9a9a9a";
const GRID: &str = "#ffffff";
/// One-hue sequential ramp (dark to light on the dark surface), for the heatmap.
const RAMP: [(u8, u8, u8); 7] =
    [(0x0f, 0x4a, 0x96), (0x1b, 0x64, 0xbe), (0x25, 0x7d, 0xe0), (0x51, 0x9c, 0xed), (0x84, 0xb9, 0xf3), (0xb6, 0xd5, 0xf8), (0xcc, 0xe3, 0xfd)];
const EMPTY: &str = "#26262e";

/// The text above a chart: what is counted (`kicker`), whose (`title`) and over what (`label`).
pub struct Heading<'a> {
    pub kicker: &'a str,
    pub title: &'a str,
    pub label: &'a str,
}

fn open(h: f32, head: &Heading) -> String {
    format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{h}" font-family="{SANS}, {FALLBACK}">
<rect width="{W}" height="{h}" fill="{SURFACE}"/>
<text x="32" y="42" font-family="{MONO}, {FALLBACK}" font-size="12" letter-spacing="4" fill="{MUTED}">{}</text>
<text x="32" y="84" font-family="{DISPLAY}, {FALLBACK}" font-weight="800" font-size="34" fill="{INK}">{}</text>
<text x="32" y="110" font-family="{MONO}, {FALLBACK}" font-size="12" letter-spacing="2" fill="{MUTED}">{}</text>
"##,
        xml(&head.kicker.to_uppercase()),
        xml(&clip(head.title, 36)),
        xml(&clip(&head.label.to_uppercase(), 64)),
    )
}

fn close(s: &mut String, h: f32) {
    s.push_str(&format!(
        r##"<text x="{}" y="{}" text-anchor="end" font-family="{MONO}, {FALLBACK}" font-size="10" letter-spacing="2" fill="{MUTED}" fill-opacity="0.6">LBBOT · LISTENBRAINZ</text></svg>"##,
        W - 24.0,
        h - 14.0
    ));
}

/// A round step at or above `max / 4`: 1, 2 or 5 times a power of ten.
fn tick_step(max: u64) -> u64 {
    let raw = (max as f64 / 4.0).max(1.0);
    let pow = 10f64.powi(raw.log10().floor() as i32);
    let step = [1.0, 2.0, 5.0, 10.0].into_iter().map(|m| m * pow).find(|s| *s >= raw).unwrap_or(10.0 * pow);
    step as u64
}

fn short(n: u64) -> String {
    match n {
        n if n >= 1_000_000 => format!("{:.1}M", n as f64 / 1e6).replace(".0M", "M"),
        n if n >= 10_000 => format!("{}K", n / 1000),
        n if n >= 1000 => format!("{:.1}K", n as f64 / 1e3).replace(".0K", "K"),
        n => n.to_string(),
    }
}

/// Vertical bars from a zero baseline, with the tallest one labeled. `bars`: (label under the bar, value).
pub fn bars(head: &Heading, bars: &[(String, u64)]) -> Result<Vec<u8>> {
    let h = 480.0;
    let (left, right, top, base) = (72.0, W - 32.0, 150.0, h - 60.0);
    let mut s = open(h, head);
    let max = bars.iter().map(|b| b.1).max().unwrap_or(0).max(1);
    let step = tick_step(max);
    let ceiling = max.div_ceil(step) * step;
    let y = |v: u64| base - (base - top) * v as f32 / ceiling as f32;
    for t in (0..=ceiling).step_by(step as usize) {
        let ty = y(t);
        s.push_str(&format!(
            r##"<line x1="{left}" y1="{ty}" x2="{right}" y2="{ty}" stroke="{GRID}" stroke-opacity="{}"/><text x="{}" y="{}" text-anchor="end" font-family="{MONO}, {FALLBACK}" font-size="11" fill="{MUTED}">{}</text>"##,
            if t == 0 { 0.35 } else { 0.08 },
            left - 10.0,
            ty + 4.0,
            short(t)
        ));
    }
    let n = bars.len().max(1) as f32;
    let slot = (right - left) / n;
    let width = (slot * 0.72).max(2.0).min(slot - 2.0);
    let every = bars.len().div_ceil(14).max(1);
    let tallest = bars.iter().enumerate().max_by_key(|(i, b)| (b.1, std::cmp::Reverse(*i))).map(|(i, _)| i);
    for (i, (label, v)) in bars.iter().enumerate() {
        let x = left + slot * i as f32 + (slot - width) / 2.0;
        let (top_y, height) = (y(*v), base - y(*v));
        if *v > 0 {
            // Rounded at the data end only; the base stays square on the axis.
            let r = 4f32.min(width / 2.0).min(height);
            s.push_str(&format!(
                r##"<path d="M{x},{base} V{} Q{x},{top_y} {},{top_y} H{} Q{},{top_y} {},{} V{base} Z" fill="{BAR}"/>"##,
                top_y + r,
                x + r,
                x + width - r,
                x + width,
                x + width,
                top_y + r
            ));
        }
        let cx = x + width / 2.0;
        if Some(i) == tallest {
            s.push_str(&format!(
                r##"<text x="{cx}" y="{}" text-anchor="middle" font-family="{MONO}, {FALLBACK}" font-size="12" fill="{INK}">{}</text>"##,
                top_y - 8.0,
                short(*v)
            ));
        }
        if i % every == 0 {
            s.push_str(&format!(
                r##"<text x="{cx}" y="{}" text-anchor="middle" font-family="{MONO}, {FALLBACK}" font-size="11" fill="{MUTED}">{}</text>"##,
                base + 20.0,
                xml(&clip(label, 8))
            ));
        }
    }
    close(&mut s, h);
    crate::draw::png(&s)
}

/// The ramp color for `v` of `max`, as `#rrggbb`.
fn shade(v: u32, max: u32) -> String {
    if v == 0 || max == 0 {
        return EMPTY.into();
    }
    let t = (v as f32 / max as f32).clamp(0.0, 1.0);
    let pos = t * (RAMP.len() - 1) as f32;
    let (i, f) = (pos.floor() as usize, pos.fract());
    let (a, b) = (RAMP[i], RAMP[(i + 1).min(RAMP.len() - 1)]);
    let mix = |x: u8, y: u8| (f32::from(x) + (f32::from(y) - f32::from(x)) * f).round() as u8;
    format!("#{:02x}{:02x}{:02x}", mix(a.0, b.0), mix(a.1, b.1), mix(a.2, b.2))
}

/// Plays per weekday (rows, Monday first) and hour (columns).
pub fn heatmap(head: &Heading, grid: &[[u32; 24]; 7]) -> Result<Vec<u8>> {
    let h = 440.0;
    let (left, top) = (72.0, 156.0);
    let (cell, gap) = ((W - 32.0 - left) / 24.0, 2.0);
    let mut s = open(h, head);
    let max = grid.iter().flatten().copied().max().unwrap_or(0);
    for hour in (0..24).step_by(3) {
        s.push_str(&format!(
            r##"<text x="{}" y="{}" text-anchor="middle" font-family="{MONO}, {FALLBACK}" font-size="11" fill="{MUTED}">{hour:02}</text>"##,
            left + cell * hour as f32 + cell / 2.0,
            top - 10.0
        ));
    }
    for (row, (name, hours)) in ["MON", "TUE", "WED", "THU", "FRI", "SAT", "SUN"].iter().zip(grid).enumerate() {
        let y = top + 30.0 * row as f32;
        s.push_str(&format!(
            r##"<text x="{}" y="{}" text-anchor="end" font-family="{MONO}, {FALLBACK}" font-size="11" letter-spacing="1" fill="{MUTED}">{name}</text>"##,
            left - 10.0,
            y + 18.0
        ));
        for (hour, v) in hours.iter().enumerate() {
            s.push_str(&format!(
                r##"<rect x="{}" y="{y}" width="{}" height="28" rx="3" fill="{}"/>"##,
                left + cell * hour as f32,
                cell - gap,
                shade(*v, max)
            ));
        }
    }
    // Legend: the ramp from few to many plays.
    let ly = top + 30.0 * 7.0 + 22.0;
    s.push_str(&format!(r##"<text x="{left}" y="{}" font-family="{MONO}, {FALLBACK}" font-size="11" fill="{MUTED}">FEWER</text>"##, ly + 12.0));
    for i in 0..5u32 {
        s.push_str(&format!(r##"<rect x="{}" y="{ly}" width="16" height="16" rx="3" fill="{}"/>"##, left + 52.0 + 20.0 * i as f32, shade(i * 25 + 1, 101)));
    }
    s.push_str(&format!(r##"<text x="{}" y="{}" font-family="{MONO}, {FALLBACK}" font-size="11" fill="{MUTED}">MORE · MAX {} PLAYS</text>"##, left + 160.0, ly + 12.0, short(u64::from(max))));
    close(&mut s, h);
    crate::draw::png(&s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticks_and_shades() {
        assert_eq!(tick_step(1), 1);
        assert_eq!(tick_step(9_000), 5_000);
        assert_eq!(tick_step(12_345), 5_000);
        assert_eq!(tick_step(400), 100);
        assert_eq!(short(12_345), "12K");
        assert_eq!(short(1_500), "1.5K");
        assert_eq!(short(2_000_000), "2M");
        assert_eq!(shade(0, 10), EMPTY);
        assert_eq!(shade(10, 10), "#cce3fd");
        assert_eq!(shade(1, 1_000_000), "#0f4a96");
    }

    #[test]
    fn renders_pngs() {
        let head = Heading { kicker: "Plays per year", title: "Züppe <Vampir> & co", label: "all time · 12,345 plays" };
        let years: Vec<(String, u64)> = (2012..2027).map(|y| (y.to_string(), ((y - 2010) * 731 % 4000) as u64)).collect();
        let png = bars(&head, &years).unwrap();
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        assert!(bars(&head, &[]).is_ok(), "no bars still draws the frame");
        let mut grid = [[0u32; 24]; 7];
        for (d, row) in grid.iter_mut().enumerate() {
            for (h, v) in row.iter_mut().enumerate() {
                *v = ((d * 7 + h * 13) % 40) as u32 * u32::from(h > 7);
            }
        }
        let map = heatmap(&Heading { kicker: "When you listen", title: "鷺巣詩郎", label: "Europe/Istanbul" }, &grid).unwrap();
        // For a look: GRAPH_OUT=/dir cargo test graph
        if let Ok(dir) = std::env::var("GRAPH_OUT") {
            std::fs::write(format!("{dir}/bars.png"), &png).unwrap();
            std::fs::write(format!("{dir}/clock.png"), &map).unwrap();
        }
    }
}
