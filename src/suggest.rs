//! Slash-command autocomplete: the user's own artists, albums and tracks by plays, and round play counts.

use std::{collections::HashMap, sync::Arc, time::{Duration, Instant}};

use anyhow::Result;
use tokio::sync::OnceCell;
use twilight_model::{
    application::{
        command::{CommandOptionChoice, CommandOptionChoiceValue},
        interaction::{
            Interaction,
            application_command::{CommandData, CommandDataOption, CommandOptionValue},
        },
    },
    http::interaction::{InteractionResponse, InteractionResponseData, InteractionResponseType},
};

use crate::{bot::Bot, db::Kind, stats};

/// Names kept per kind; suggestions come from these, filtered by what is typed.
const KEEP: usize = 1000;
/// Long enough for one command's keystrokes, short enough to show a new favorite soon.
const TTL: Duration = Duration::from_secs(600);
/// Discord's limit.
const MAX: usize = 25;

pub(crate) struct Suggestions {
    /// (name, plays), most played first; artists, albums ("Artist - Album"), tracks.
    names: [Vec<(String, u32)>; 3],
    /// Listens counted like `pace` (with the LB offset), and stored ones, which `milestone` numbers.
    total: i64,
    stored: i64,
}

/// Keyed by user and LB name, so logging in with another account starts a new list.
pub(crate) type Cache = std::sync::Mutex<HashMap<(i32, String), (Instant, Arc<OnceCell<Arc<Suggestions>>>)>>;

fn slot(kind: Kind) -> usize {
    match kind {
        Kind::Artist => 0,
        Kind::Album => 1,
        Kind::Track => 2,
    }
}

/// The focused option and the others, from the command or its subcommand.
fn options(data: &CommandData) -> Vec<&CommandDataOption> {
    data.options
        .iter()
        .flat_map(|o| match &o.value {
            CommandOptionValue::SubCommand(sub) => sub.iter().collect(),
            _ => vec![o],
        })
        .collect()
}

/// What a `query` names: the `type` option, else the command (`wkalbum`, `gwktrack`…).
fn query_kind(command: &str, kind: Option<&str>) -> Kind {
    match kind {
        Some("album") => Kind::Album,
        Some("track") => Kind::Track,
        Some(_) => Kind::Artist,
        None if command.ends_with("album") => Kind::Album,
        None if command.ends_with("track") => Kind::Track,
        None => Kind::Artist,
    }
}

/// Names containing `typed`, case ignored, most played first.
fn names(list: &[(String, u32)], typed: &str) -> Vec<CommandOptionChoice> {
    let typed = typed.trim().to_lowercase();
    list.iter()
        .filter(|(n, _)| n.chars().count() <= 100 && n.to_lowercase().contains(&typed))
        .take(MAX)
        .map(|(n, plays)| {
            let label = format!(" · {plays} plays");
            let keep = 100 - label.chars().count();
            let shown: String = if n.chars().count() > keep { n.chars().take(keep - 1).chain(['…']).collect() } else { n.clone() };
            choice(format!("{shown}{label}"), CommandOptionChoiceValue::String(n.clone()))
        })
        .collect()
}

fn choice(name: String, value: CommandOptionChoiceValue) -> CommandOptionChoice {
    CommandOptionChoice { name, name_localizations: None, value }
}

/// What was typed if it is a number, then `numbers` that start with it.
fn numbers(numbers: impl IntoIterator<Item = i64>, typed: &str) -> Vec<CommandOptionChoice> {
    let typed = typed.trim().replace([',', '.'], "");
    let first: Option<i64> = typed.parse().ok().filter(|n| *n >= 1);
    let rest = numbers.into_iter().filter(|n| n.to_string().starts_with(&typed) && Some(*n) != first);
    let out: Vec<i64> = first.into_iter().chain(rest).collect();
    out.into_iter().take(MAX).map(|n| choice(format!("{n}"), CommandOptionChoiceValue::Integer(n))).collect()
}

/// The next round play counts after `n`.
fn goals(n: i64, small: bool) -> Vec<i64> {
    std::iter::successors(Some(stats::next_goal(n, small)), |&g| Some(stats::next_goal(g, small))).take(10).collect()
}

/// Round listen numbers up to `total`, highest first: 1, 10, 100, 1000, 5000, then every 10,000.
fn milestones(total: i64) -> Vec<i64> {
    let mut v: Vec<i64> = [1, 10, 100, 1000, 5000].into_iter().chain((1..).map(|k| k * 10_000)).take_while(|&m| m <= total).collect();
    v.reverse();
    v
}

impl Bot {
    pub(crate) async fn autocomplete(&self, i: &Interaction, data: &CommandData) -> Result<()> {
        let choices = self.choices(i, data).await.unwrap_or_else(|e| {
            tracing::warn!(error = ?e, "autocomplete failed");
            Vec::new()
        });
        let resp = InteractionResponse {
            kind: InteractionResponseType::ApplicationCommandAutocompleteResult,
            data: Some(InteractionResponseData { choices: Some(choices), ..Default::default() }),
        };
        // Expected when the first list took over Discord's 3 s; the cache is filled for the next keystroke.
        if let Err(e) = self.http.interaction(self.app_id).create_response(i.id, &i.token, &resp).await {
            tracing::debug!(error = ?e, "autocomplete answer not delivered");
        }
        Ok(())
    }

    async fn choices(&self, i: &Interaction, data: &CommandData) -> Result<Vec<CommandOptionChoice>> {
        let opts = options(data);
        let Some((field, typed)) = opts.iter().find_map(|o| match &o.value {
            CommandOptionValue::Focused(typed, _) => Some((o.name.as_str(), typed.as_str())),
            _ => None,
        }) else {
            return Ok(Vec::new());
        };
        let string = |name: &str| {
            opts.iter().find(|o| o.name == name).and_then(|o| match &o.value {
                CommandOptionValue::String(s) => Some(s.as_str()),
                _ => None,
            })
        };
        if data.name == "timezone" {
            // Matches of what was typed, else the zones of the user's language.
            let locales: Vec<&str> = [i.locale.as_deref(), i.guild_locale.as_deref()].into_iter().flatten().collect();
            return Ok(crate::tz::suggest(typed, &locales)
                .into_iter()
                .map(|zone| choice(crate::tz::label(&zone), CommandOptionChoiceValue::String(zone)))
                .collect());
        }
        let Some(discord) = i.author().map(|u| u.id.get() as i64) else { return Ok(Vec::new()) };
        let Some(s) = self.suggestions(discord).await? else { return Ok(Vec::new()) };
        let command = data.name.as_str();
        Ok(match field {
            "query" => names(&s.names[slot(query_kind(command, string("type")))], typed),
            "artist" => names(&s.names[0], typed),
            "goal" if command == "artistpace" => {
                let artist = string("artist").map(str::to_lowercase);
                let plays = s.names[0].iter().find(|(n, _)| Some(n.to_lowercase()) == artist).map_or(0, |(_, p)| i64::from(*p));
                numbers(goals(plays, true), typed)
            }
            "goal" => numbers(goals(s.total, false), typed),
            "number" => numbers(milestones(s.stored), typed),
            _ => Vec::new(),
        })
    }

    /// Drops expired lists; also run by `presence_loop`, so they leave memory without new autocompletes.
    pub(crate) fn prune_suggestions(&self) {
        self.suggest_cache.lock().unwrap().retain(|_, (t, _)| t.elapsed() < TTL);
    }

    /// Drops a user's lists, for `/remove` and a new `/login`.
    pub(crate) fn forget_suggestions(&self, user_id: i32) {
        self.suggest_cache.lock().unwrap().retain(|(id, _), _| *id != user_id);
    }

    /// The registered user's suggestions, built at most once per `TTL` even while keystrokes overlap.
    async fn suggestions(&self, discord: i64) -> Result<Option<Arc<Suggestions>>> {
        let Some(user) = self.db().user_by_discord(discord).await? else { return Ok(None) };
        self.prune_suggestions();
        let cell = {
            let mut cache = self.suggest_cache.lock().unwrap();
            cache.entry((user.id, user.lb_name.to_lowercase())).or_insert_with(|| (Instant::now(), Arc::default())).1.clone()
        };
        let s = cell
            .get_or_try_init(|| async {
                let h = self.load(&user).await?;
                let mut names: [Vec<(String, u32)>; 3] = Default::default();
                for kind in [Kind::Artist, Kind::Album, Kind::Track] {
                    let top: Vec<(i32, u32)> = stats::top(&h.entries, h.key(kind)).into_iter().take(KEEP).collect();
                    let ids: Vec<i32> = top.iter().map(|r| r.0).collect();
                    let mut found = self.db().names(kind, &ids).await?;
                    names[slot(kind)] = top.iter().filter_map(|(id, n)| Some((found.remove(id)?, *n))).collect();
                }
                let total = h.entries.len() as i64 + i64::from(user.count_offset);
                anyhow::Ok(Arc::new(Suggestions { names, total, stored: h.entries.len() as i64 }))
            })
            .await?;
        Ok(Some(s.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggestion_lists() {
        assert_eq!(milestones(25_000), [20_000, 10_000, 5000, 1000, 100, 10, 1]);
        assert_eq!(goals(1234, false)[..2], [2000, 3000]);
        let picked: Vec<_> = numbers([100, 1000, 2000], "1").into_iter().map(|c| c.value).collect();
        assert_eq!(picked, [1, 100, 1000].map(CommandOptionChoiceValue::Integer));
        let list = vec![("Radiohead".to_owned(), 50), ("Röyksopp".to_owned(), 9), ("x".repeat(120), 1)];
        let found = names(&list, "RÖY");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "Röyksopp · 9 plays");
        assert_eq!(names(&list, "").len(), 2, "names over 100 characters can't be values");
        assert_eq!(query_kind("lastlistened", Some("album")), Kind::Album);
        assert_eq!(query_kind("discoverydate", None), Kind::Artist);
        assert_eq!(query_kind("gwktrack", None), Kind::Track);
        assert_eq!(query_kind("wkalbum", None), Kind::Album);
    }
}
