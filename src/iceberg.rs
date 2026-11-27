//! The `.iceberg` picture: an SVG drawn here, rendered to PNG with resvg. Tiers go from the tip down.

use anyhow::Result;

use crate::draw::{DISPLAY, FALLBACK, MONO, SANS, clip, xml};

const W: f32 = 640.0;
const H: f32 = 960.0;
const WATER: f32 = 330.0;
/// The tip tier is the sky above the water; the other tiers split the sea evenly down to `BOTTOM`.
const TIP_TOP: f32 = 168.0;
const BOTTOM: f32 = 930.0;
/// Zone names next to `stats::TIERS`, top first.
const ZONES: [&str; 6] = ["THE TIP", "SHALLOWS", "TWILIGHT ZONE", "MIDNIGHT ZONE", "THE ABYSS", "THE TRENCH"];

pub fn render(title: &str, label: &str, tiers: &[Vec<String>]) -> Result<Vec<u8>> {
    crate::draw::png(&svg(title, label, tiers))
}

/// Top and bottom of each tier band.
fn bands(n: usize) -> Vec<(f32, f32)> {
    let sea = (BOTTOM - WATER) / n.saturating_sub(1).max(1) as f32;
    (0..n).map(|i| if i == 0 { (TIP_TOP, WATER) } else { (WATER + sea * (i - 1) as f32, WATER + sea * i as f32) }).collect()
}

fn threshold(i: usize) -> String {
    let min = crate::stats::TIERS.get(i).map_or(0, |t| t.0);
    match (min, crate::stats::TIERS.get(i.wrapping_sub(1)).map(|t| t.0)) {
        (0, Some(above)) => format!("UNDER {}", short(above)),
        (m, _) => format!("{}+ LISTENERS", short(m)),
    }
}

fn short(n: i64) -> String {
    if n >= 1000 { format!("{}K", n / 1000) } else { n.to_string() }
}

/// Low-poly facets: (points, fill, opacity).
const TIP: [(&str, &str, f32); 8] = [
    ("320,176 290,222 318,262", "#ffffff", 1.0),
    ("320,176 318,262 362,212", "#eef6ff", 1.0),
    ("362,212 318,262 392,268", "#d6e9fb", 1.0),
    ("392,268 318,262 326,330", "#c0ddf6", 1.0),
    ("392,268 326,330 414,330", "#a9cff0", 1.0),
    ("290,222 258,262 318,262", "#f7fbff", 1.0),
    ("258,262 226,330 318,262", "#e4f0fc", 1.0),
    ("318,262 226,330 326,330", "#d0e6f9", 1.0),
];
const BODY: [(&str, &str, f32); 10] = [
    ("226,330 150,470 320,520", "#9fe3ee", 0.30),
    ("226,330 320,520 414,330", "#c4f1f6", 0.34),
    ("414,330 320,520 495,455", "#7fd3e3", 0.26),
    ("150,470 130,640 320,520", "#7fd3e3", 0.22),
    ("495,455 320,520 515,620", "#5fc1d8", 0.20),
    ("130,640 320,720 320,520", "#9fe3ee", 0.18),
    ("320,520 320,720 515,620", "#5fc1d8", 0.15),
    ("130,640 190,800 320,720", "#5fc1d8", 0.13),
    ("515,620 320,720 455,815", "#4aa9c8", 0.12),
    ("190,800 320,930 455,815", "#4aa9c8", 0.10),
];
const STARS: [(f32, f32, f32); 14] = [
    (40.0, 30.0, 1.2), (110.0, 70.0, 0.8), (178.0, 22.0, 1.0), (560.0, 36.0, 1.3), (610.0, 96.0, 0.8), (470.0, 18.0, 0.9),
    (520.0, 128.0, 0.7), (70.0, 140.0, 0.9), (24.0, 210.0, 0.7), (600.0, 190.0, 0.8), (420.0, 60.0, 0.6), (220.0, 120.0, 0.6),
    (140.0, 230.0, 0.5), (580.0, 250.0, 0.5),
];

/// `title` is the person's name; the picture says "the iceberg of" above it.
fn svg(title: &str, label: &str, tiers: &[Vec<String>]) -> String {
    let pill = 40.0 + 9.5 * label.chars().count() as f32;
    let mut s = format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" font-family="{SANS}, {FALLBACK}">
<defs>
<linearGradient id="sky" x1="0" y1="0" x2="0" y2="1">
<stop offset="0" stop-color="#14123a"/><stop offset="0.45" stop-color="#4b2d7a"/><stop offset="0.8" stop-color="#c0608a"/><stop offset="1" stop-color="#f7a072"/>
</linearGradient>
<linearGradient id="sea" x1="0" y1="0" x2="0" y2="1">
<stop offset="0" stop-color="#1c9aa8"/><stop offset="0.25" stop-color="#11607f"/><stop offset="0.6" stop-color="#0a2c52"/><stop offset="1" stop-color="#04081a"/>
</linearGradient>
<radialGradient id="glow"><stop offset="0" stop-color="#ffd9a3" stop-opacity="0.9"/><stop offset="1" stop-color="#ffb37a" stop-opacity="0"/></radialGradient>
<linearGradient id="ray" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="#ffffff" stop-opacity="0.16"/><stop offset="1" stop-color="#ffffff" stop-opacity="0"/></linearGradient>
</defs>
<rect width="{W}" height="{WATER}" fill="url(#sky)"/>
"##
    );
    for (x, y, r) in STARS {
        s.push_str(&format!(r##"<circle cx="{x}" cy="{y}" r="{r}" fill="#fff4e0" fill-opacity="0.85"/>"##));
    }
    s.push_str(&format!(
        r##"<circle cx="500" cy="{WATER}" r="150" fill="url(#glow)"/><circle cx="500" cy="{WATER}" r="46" fill="#ffe2b8"/>
<rect y="{WATER}" width="{W}" height="{}" fill="url(#sea)"/>
<polygon points="250,{WATER} 300,{WATER} 200,720 90,720" fill="url(#ray)"/>
<polygon points="420,{WATER} 455,{WATER} 560,640 470,640" fill="url(#ray)"/>
<polygon points="330,{WATER} 350,{WATER} 380,560 320,560" fill="url(#ray)"/>
"##,
        H - WATER
    ));
    for (points, fill, opacity) in BODY {
        s.push_str(&format!(r##"<polygon points="{points}" fill="{fill}" fill-opacity="{opacity}"/>"##));
    }
    // Sun glitter and waves on the surface.
    s.push_str(&format!(
        r##"<path d="M0,{WATER} {}" fill="none" stroke="#ffffff" stroke-opacity="0.75" stroke-width="2"/>
<g stroke="#ffd9a3" stroke-opacity="0.6" stroke-width="2" stroke-linecap="round">
<line x1="450" y1="340" x2="550" y2="340"/><line x1="470" y1="350" x2="530" y2="350"/><line x1="485" y1="360" x2="515" y2="360"/>
</g>
"##,
        (0..16).map(|_| "q20,-7 40,0").collect::<Vec<_>>().join(" ")
    ));
    for (points, fill, _) in TIP {
        s.push_str(&format!(r##"<polygon points="{points}" fill="{fill}" stroke="{fill}" stroke-width="0.6"/>"##));
    }
    for (cx, cy, r) in [(268.0, 600.0, 4.0), (282.0, 560.0, 2.5), (276.0, 520.0, 3.0), (372.0, 690.0, 3.5), (380.0, 650.0, 2.0), (210.0, 860.0, 3.0)] {
        s.push_str(&format!(r##"<circle cx="{cx}" cy="{cy}" r="{r}" fill="none" stroke="#ffffff" stroke-opacity="0.35"/>"##));
    }
    s.push_str(&format!(
        r##"<text x="{x}" y="46" text-anchor="middle" font-family="{MONO}, {FALLBACK}" font-size="12" letter-spacing="5" fill="#ffd9c2" fill-opacity="0.8">THE ICEBERG OF</text>
<text x="{x}" y="100" text-anchor="middle" font-family="{DISPLAY}, {FALLBACK}" font-weight="800" font-size="{size}" letter-spacing="-1" fill="#ffffff">{name}</text>
<rect x="{pill_x}" y="116" width="{pill}" height="24" rx="12" fill="#ffffff" fill-opacity="0.12" stroke="#ffd9c2" stroke-opacity="0.45"/>
<text x="{x}" y="132" text-anchor="middle" font-family="{MONO}, {FALLBACK}" font-size="11" letter-spacing="3" fill="#ffe8d6">{label}</text>
"##,
        x = W / 2.0,
        // Long names get smaller so they stay on one line.
        size = if title.chars().count() > 16 { 34 } else { 46 },
        name = xml(&clip(title, 24)),
        pill = pill,
        label = xml(&label.to_uppercase()),
        pill_x = W / 2.0 - pill / 2.0,
    ));
    for (i, ((top, bottom), names)) in bands(tiers.len()).into_iter().zip(tiers).enumerate() {
        if i > 1 {
            s.push_str(&format!(r##"<line x1="16" y1="{top}" x2="{}" y2="{top}" stroke="#ffffff" stroke-opacity="0.18" stroke-dasharray="4 6"/>"##, W - 16.0));
        }
        s.push_str(&format!(
            r##"<text x="18" y="{}" font-family="{MONO}, {FALLBACK}" font-size="10.5" letter-spacing="2.5"><tspan fill="#ffffff" fill-opacity="0.85">{}</tspan><tspan fill="#ffffff" fill-opacity="0.45"> · {}</tspan></text>"##,
            top + 20.0,
            ZONES.get(i).unwrap_or(&""),
            threshold(i)
        ));
        let h = bottom - top;
        let rows = [top + h * 0.45, top + h * 0.75];
        let slots = [(115.0, rows[0]), (525.0, rows[0]), (320.0, rows[0]), (115.0, rows[1]), (525.0, rows[1]), (320.0, rows[1])];
        for (name, (x, y)) in names.iter().zip(slots) {
            s.push_str(&format!(
                r##"<text x="{x}" y="{y}" text-anchor="middle" font-size="16.5" letter-spacing="-0.2" fill="#ffffff" stroke="#0a1630" stroke-opacity="0.85" stroke-width="4" stroke-linejoin="round" paint-order="stroke">{}</text>"##,
                xml(&clip(name, 18))
            ));
        }
    }
    s.push_str(&format!(
        r##"<text x="{}" y="{}" text-anchor="end" font-family="{MONO}, {FALLBACK}" font-size="10" letter-spacing="2" fill="#ffffff" fill-opacity="0.35">LBBOT · LISTENBRAINZ</text></svg>"##,
        W - 16.0,
        H - 12.0
    ));
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_a_png_with_awkward_names() {
        let tiers: Vec<Vec<String>> = [
            vec!["Taylor Swift", "The Weeknd", "Billie Eilish", "Dua Lipa", "Lady Gaga"],
            vec!["Aurora", "Lauv"],
            vec![],
            vec!["Tom & Jerry <live>", "\"Quoted\""],
            vec!["鷺巣詩郎", "Broken\u{1}Tag"],
            vec!["A very long artist name that does not fit anywhere"],
        ]
        .into_iter()
        .map(|t| t.into_iter().map(String::from).collect())
        .collect();
        let png = render("Züppe Vampir Eleştirmeni", "all time", &tiers).unwrap();
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        // For a look: ICEBERG_OUT=/path/to.png cargo test iceberg
        if let Ok(path) = std::env::var("ICEBERG_OUT") {
            std::fs::write(path, &png).unwrap();
        }
    }
}
