//! Replies as fmbot's Components V2 cards. Commands still build an `Embed`; `card` lays it out the way fmbot does:
//! one container in the accent colour, the picture on top, the header with the cover beside it, the text, then the
//! small footer, with the buttons inside the card (a lone button sits beside the footer).

use twilight_model::channel::message::{
    Component, Embed, MessageFlags,
    component::{
        Container, MediaGallery, MediaGalleryItem, Section, Separator, SeparatorSpacingSize, TextDisplay, Thumbnail,
        UnfurledMediaItem,
    },
};

use crate::bot::{escape, link_text};

pub(crate) const V2: MessageFlags = MessageFlags::IS_COMPONENTS_V2;
/// Discord allows this much text in a whole message.
const MESSAGE_TEXT: usize = 4000;
/// twilight's limit for one text display.
const TEXT: usize = 2000;

pub(crate) fn card(e: &Embed, rows: &[Component]) -> Vec<Component> {
    // Discord shows escapes inside link text as they are, so a link's text only loses its square brackets.
    let link = |t: &str, url: Option<&String>| match url {
        Some(u) => format!("[{}]({u})", link_text(t)),
        None => escape(t),
    };
    let mut header = Vec::new();
    if let Some(a) = &e.author {
        header.push(format!("-# {}", link(&a.name, a.url.as_ref())));
    }
    if let Some(t) = &e.title {
        header.push(format!("**{}**", link(t, e.url.as_ref())));
    }
    // Embed footers show no Markdown, so their text is escaped; every line is small.
    let footer = e.footer.as_ref().map(|f| {
        f.text.lines().filter(|l| !l.trim().is_empty()).map(|l| format!("-# {}", escape(l))).collect::<Vec<_>>().join("\n")
    });
    let footer = footer.filter(|f| !f.is_empty());
    let used = header.iter().map(|h| h.chars().count() + 1).sum::<usize>() + footer.as_ref().map_or(0, |f| f.chars().count());
    let body = chunks(e.description.as_deref().unwrap_or_default(), MESSAGE_TEXT.saturating_sub(used + 2));

    let mut parts = Vec::new();
    if let Some(img) = &e.image {
        parts.push(Component::MediaGallery(MediaGallery {
            id: None,
            items: vec![MediaGalleryItem { description: None, media: media(&img.url), spoiler: None }],
        }));
    }
    // A title starts a card of its own (WhoKnows, lists); without one, the header and text are one block (np).
    let titled = e.title.is_some();
    let mut texts: Vec<String> = Vec::new();
    match (titled, header.is_empty()) {
        (_, true) => texts.extend(body),
        (false, false) => {
            let mut first = body.into_iter();
            texts.push(format!("{}\n{}", header.join("\n"), first.next().unwrap_or_default()).trim_end().to_owned());
            texts.extend(first);
        }
        (true, false) => {
            texts.push(header.join("\n"));
            texts.extend(body);
        }
    }
    match &e.thumbnail {
        Some(thumb) if !texts.is_empty() => {
            // A section holds at most three texts; the rest follow below it.
            let rest = texts.split_off(texts.len().min(3));
            parts.push(Component::Section(Section {
                accessory: Box::new(Component::Thumbnail(Thumbnail { description: None, id: None, media: media(&thumb.url), spoiler: None })),
                components: texts.into_iter().map(text).collect(),
                id: None,
            }));
            parts.extend(rest.into_iter().map(text));
        }
        _ if titled && e.image.is_none() && texts.len() > 1 => {
            let mut texts = texts.into_iter();
            parts.extend(texts.next().map(text));
            parts.push(separator());
            parts.extend(texts.map(text));
        }
        _ => parts.extend(texts.into_iter().map(text)),
    }
    let mut rows = rows.to_vec();
    if let Some(f) = footer {
        if titled && e.image.is_none() {
            parts.push(separator());
        }
        match rows.as_slice() {
            // fmbot puts a lone button (the chart's Edit) beside the footer.
            [Component::ActionRow(row)] if matches!(row.components.as_slice(), [Component::Button(_)]) => {
                parts.push(Component::Section(Section { accessory: Box::new(row.components[0].clone()), components: vec![text(f)], id: None }));
                rows.clear();
            }
            _ => parts.push(text(f)),
        }
    }
    parts.extend(rows);
    let accent_color = e.color.map(Some);
    vec![Component::Container(Container { accent_color, components: parts, id: None, spoiler: None })]
}

fn text(content: String) -> Component {
    Component::TextDisplay(TextDisplay { content, id: None })
}

fn separator() -> Component {
    Component::Separator(Separator { divider: Some(true), id: None, spacing: Some(SeparatorSpacingSize::Small) })
}

fn media(url: &str) -> UnfurledMediaItem {
    UnfurledMediaItem { content_type: None, height: None, proxy_url: None, url: url.to_owned(), width: None }
}

/// `s` cut to `budget` characters at a line end, in texts of at most `TEXT` characters, split between paragraphs
/// when possible so a block (a code block, a day of the overview) stays whole.
fn chunks(s: &str, budget: usize) -> Vec<String> {
    let mut s = s.trim_end();
    if s.chars().count() > budget {
        let cut = s.char_indices().nth(budget.saturating_sub(1)).map_or(s.len(), |(i, _)| i);
        s = s[..cut].rfind('\n').map_or(&s[..cut], |i| &s[..i]).trim_end();
    }
    let mut out = Vec::new();
    while !s.is_empty() {
        if s.chars().count() <= TEXT {
            out.push(s.to_owned());
            break;
        }
        let cut = s.char_indices().nth(TEXT).map_or(s.len(), |(i, _)| i);
        let at = s[..cut].rfind("\n\n").or_else(|| s[..cut].rfind('\n')).filter(|&i| i > 0).unwrap_or(cut);
        out.push(s[..at].trim_end().to_owned());
        s = s[at..].trim_start_matches('\n');
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use twilight_model::channel::message::component::{ActionRow, Button, ButtonStyle};
    use twilight_util::builder::embed::{EmbedAuthorBuilder, EmbedBuilder, EmbedFooterBuilder, ImageSource};

    const COVER: &str = "https://coverartarchive.org/release/76df3287-6cda-33eb-8e9a-044b5e15ffdd/829521842-250.jpg";

    fn button(label: &str, id: &str, disabled: bool) -> Component {
        Component::Button(Button {
            custom_id: Some(id.into()),
            disabled,
            emoji: None,
            id: None,
            label: Some(label.into()),
            sku_id: None,
            style: ButtonStyle::Secondary,
            url: None,
        })
    }

    fn row(buttons: Vec<Component>) -> Component {
        Component::ActionRow(ActionRow { components: buttons, id: None })
    }

    /// One card of each layout the bot sends, shaped like the real commands' embeds.
    pub(crate) fn samples() -> Vec<(&'static str, Embed, Vec<Component>)> {
        let base = || EmbedBuilder::new().color(crate::bot::COLOR);
        let np = base()
            .author(EmbedAuthorBuilder::new("Now playing - zoltrons").url("https://listenbrainz.org/user/zoltrons/"))
            .description("**[Paranoid Android](https://listenbrainz.org/track/x/)**\n**Radiohead** • *OK Computer*")
            .footer(EmbedFooterBuilder::new("1204 artist plays"))
            .thumbnail(ImageSource::url(COVER).unwrap())
            .build();
        let wk_lines = (1..=10)
            .map(|n| {
                let rank = if n == 1 { "👑".to_owned() } else { format!("{n}\\.") };
                let link = format!("[user_{n}](https://listenbrainz.org/user/u{n}/)");
                if n == 3 { format!("**{rank} {link}** - **{}** plays", 500 / n) } else { format!("{rank} {link} - **{}** plays", 500 / n) }
            })
            .collect::<Vec<_>>()
            .join("\n");
        let wk = base()
            .title("Radiohead in Brainz Lounge")
            .url("https://listenbrainz.org/artist/a74b1b7f/")
            .description(wk_lines)
            .footer(EmbedFooterBuilder::new("alternative rock, art rock, electronic\nArtist - 10 listeners - 1463 plays - 146 avg"))
            .thumbnail(ImageSource::url(COVER).unwrap())
            .build();
        let days = (0..4)
            .map(|d| {
                format!(
                    "**Saturday, November {}** · <t:1764288000:R>\n**{}** plays · **{}m** · 12 artists\n**Radiohead** · *OK Computer* · Paranoid Android\n`  42` Radiohead\n`  17` Björk\n`   9` Portishead",
                    28 - d,
                    80 - d * 7,
                    250 - d * 20
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        let overview = base()
            .title("Daily overview for zoltrons")
            .url("https://listenbrainz.org/user/zoltrons/")
            .description(days)
            .footer(EmbedFooterBuilder::new("Page 1/3 · 28 days\nDays are counted in Europe/Istanbul"))
            .build();
        let pages = row(vec![button("⏮", "a", true), button("◀", "b", true), button("▶", "c", false), button("⏭", "d", false)]);
        let chart = base()
            .title("3x3 weekly album chart for zoltrons")
            .url("https://listenbrainz.org/user/zoltrons/")
            .image(ImageSource::attachment("chart.png").unwrap())
            .description("Rainbow sorted")
            .footer(EmbedFooterBuilder::new("zoltrons has 48211 listens · 1 without a cover"))
            .build();
        let picture = base()
            .image(ImageSource::attachment("iceberg.png").unwrap())
            .footer(EmbedFooterBuilder::new("Your iceberg · all time\nDays are counted in Europe/Istanbul"))
            .build();
        let summary = base()
            .title("2026 in music")
            .description("**18,204** plays · **1,032** hours\n**612** artists · **1,840** albums · **6,112** tracks\n\n**Top artists**\n1\\. Radiohead · 1204\n2\\. Björk · 822\n3\\. Portishead · 610\n\n**Top albums**\n1\\. OK Computer · 402\n2\\. Homogenic · 300")
            .footer(EmbedFooterBuilder::new("+12% plays compared to 2025\nDays are counted in Europe/Istanbul"))
            .build();
        let receipt = base()
            .description("```\n        LISTENBRAINZ\n------------------------------\n  1  Paranoid Android\n  2  Hyperballad\n------------------------------\nTOTAL PLAYS                 82\n```")
            .build();
        let login = crate::bot::login_first();
        let reply = crate::bot::reply("Connect your ListenBrainz account with `/login` first.");
        vec![
            ("np", np, vec![]),
            ("whoknows", wk, vec![]),
            ("overview", overview, vec![pages]),
            ("chart", chart, vec![row(vec![button("Edit", "e", false)])]),
            ("picture", picture, vec![]),
            ("summary", summary, vec![]),
            ("receipt", receipt, vec![]),
            ("reply", reply, vec![]),
            ("login", login.embed, login.components),
        ]
    }

    fn all_text(c: &Component) -> usize {
        match c {
            Component::TextDisplay(t) => t.content.chars().count(),
            Component::Container(c) => c.components.iter().map(all_text).sum(),
            Component::Section(s) => s.components.iter().map(all_text).sum(),
            _ => 0,
        }
    }

    #[test]
    fn cards_pass_v2_validation_and_limits() {
        let long = EmbedBuilder::new().title("t").description("line of text here\n".repeat(400)).footer(EmbedFooterBuilder::new("f")).build();
        for (name, e, rows) in samples().into_iter().chain([("long", long, vec![])]) {
            let c = card(&e, &rows);
            twilight_validate::message::components(&c, true).unwrap_or_else(|err| panic!("{name}: {err}"));
            assert!(all_text(&c[0]) <= MESSAGE_TEXT, "{name}");
        }
    }

    #[test]
    fn layout() {
        let s = samples();
        let parts = |i: usize| match &card(&s[i].1, &s[i].2)[0] {
            Component::Container(c) => c.components.clone(),
            _ => panic!(),
        };
        // np: header and track beside the cover, the footer right under it.
        assert!(matches!(parts(0).as_slice(), [Component::Section(_), Component::TextDisplay(f)] if f.content == "-# 1204 artist plays"));
        // chart: picture first, the Edit button beside the footer, as in fmbot.
        assert!(matches!(parts(3).as_slice(), [Component::MediaGallery(_), Component::TextDisplay(_), Component::TextDisplay(_), Component::Section(_)]));
        // login: the text, then its button.
        assert!(matches!(parts(8).as_slice(), [Component::TextDisplay(_), Component::ActionRow(_)]));
        // overview: title, rule, days, rule, footer, buttons.
        assert!(matches!(parts(2).as_slice(), [Component::TextDisplay(_), Component::Separator(_), Component::TextDisplay(_), Component::Separator(_), Component::TextDisplay(_), Component::ActionRow(_)]));
        assert!(matches!(parts(7).as_slice(), [Component::TextDisplay(_)]));
    }

    #[test]
    fn linked_titles_are_not_escaped() {
        let e = EmbedBuilder::new().title("Vignette (Live) [Remix]").url("https://x.org/").description("d").build();
        let plain = EmbedBuilder::new().title("a_b (c)").description("d").build();
        let first = |e: &Embed| match &card(e, &[])[0] {
            Component::Container(c) => match &c.components[0] {
                Component::TextDisplay(t) => t.content.clone(),
                _ => panic!(),
            },
            _ => panic!(),
        };
        assert_eq!(first(&e), "**[Vignette (Live) (Remix)](https://x.org/)**");
        assert_eq!(first(&plain), "**a\\_b \\(c\\)**");
    }

    #[test]
    fn chunks_split_between_paragraphs() {
        let day = format!("{}\n", "x".repeat(99)).repeat(9);
        let text = [day.trim_end(); 4].join("\n\n");
        let c = chunks(&text, 4000);
        assert_eq!(c.len(), 2);
        assert!(c.iter().all(|t| t.chars().count() <= TEXT && !t.starts_with('\n')));
        assert_eq!(c.join("\n\n"), text);
        assert!(chunks(&"ab\n".repeat(100), 10).concat().chars().count() <= 10);
    }
}
