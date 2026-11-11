// The Forge GTV - Copyright (C) 2026 [TFG]Aciddrop
// SPDX-License-Identifier: GPL-3.0-or-later
// This program comes with ABSOLUTELY NO WARRANTY; see the LICENSE file.

//! Discord transport (serenity): slash commands, interactions, the `Chat` implementation.

use std::sync::Arc;

use async_trait::async_trait;
use serenity::all::*;
use serenity::async_trait as serenity_async_trait;

use crate::app::{App, DiscordStatus};
use crate::embed::{Chat, ChatError, Components, Embed};

pub struct DiscordHandle {
    pub(crate) shard_manager: Arc<ShardManager>,
    task: tokio::task::JoinHandle<()>,
}

impl DiscordHandle {
    pub async fn shutdown(self) {
        self.shard_manager.shutdown_all().await;
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), self.task).await;
    }
}

fn map_err(e: serenity::Error) -> ChatError {
    if let serenity::Error::Http(HttpError::UnsuccessfulRequest(r)) = &e {
        match r.status_code.as_u16() {
            404 => return ChatError::NotFound,
            403 => return ChatError::Forbidden,
            _ => {}
        }
    }
    ChatError::Other(e.to_string())
}

fn to_create_embed(e: &Embed) -> CreateEmbed {
    let mut c = CreateEmbed::new().color(e.color);
    if !e.title.is_empty() {
        c = c.title(&e.title);
    }
    if !e.description.is_empty() {
        c = c.description(&e.description);
    }
    for f in &e.fields {
        c = c.field(&f.name, &f.value, f.inline);
    }
    if !e.footer.is_empty() {
        let mut f = CreateEmbedFooter::new(&e.footer);
        if !e.footer_icon.is_empty() {
            f = f.icon_url(&e.footer_icon);
        }
        c = c.footer(f);
    }
    if !e.author.is_empty() {
        let mut a = CreateEmbedAuthor::new(&e.author);
        if !e.author_icon.is_empty() {
            a = a.icon_url(&e.author_icon);
        }
        c = c.author(a);
    }
    if !e.thumbnail.is_empty() {
        c = c.thumbnail(&e.thumbnail);
    }
    if !e.image.is_empty() {
        c = c.image(&e.image);
    }
    if e.timestamp {
        c = c.timestamp(Timestamp::now());
    }
    c
}

/// The id of the "Player card" drop-down: it must say which server's scoreboard it sits under.
pub fn player_menu_id(channel: u64, server: &str) -> String {
    if server.is_empty() { format!("gtv:player:{}", channel) } else { format!("gtv:player:{}:{}", channel, server) }
}

/// Read a drop-down id back: (channel, server if the id names one).
pub fn parse_player_menu_id(id: &str) -> Option<(u64, Option<String>)> {
    let rest = id.strip_prefix("gtv:player:")?;
    match rest.split_once(':') {
        Some((c, s)) => Some((c.parse().ok()?, Some(s.to_string()).filter(|s| !s.is_empty()))),
        None => Some((rest.parse().ok()?, None)),
    }
}

/// The text of `/gtv list`.
pub fn list_text(all: &[crate::app::BroadcastInfo]) -> String {
    if all.is_empty() {
        return "Nothing is broadcasting. Start one with `/gtv start`.".into();
    }
    let mut out = format!("**Broadcasting now ({})**", all.len());
    for b in all {
        let video = match (&b.watch_url, b.video) {
            (Some(u), true) => format!(" · 📹 [watch]({}){}", u, if b.web_port != 0 { format!(" (port {})", b.web_port) } else { String::new() }),
            (_, true) => " · 📹 video starting".to_string(),
            _ => String::new(),
        };
        let eyes = if b.video { format!(" · 👁 {}", b.viewers) } else { String::new() };
        out.push_str(&format!("\n• **{}** (`{}`) in <#{}> · delay {}s{}{}", b.name, b.server, b.channel_id, b.delay, video, eyes));
    }
    if out.chars().count() > 1900 {
        out = out.chars().take(1890).collect::<String>() + "\n…";
    }
    out
}

fn to_rows(channel: u64, c: &Components) -> Vec<CreateActionRow> {
    let mut rows = Vec::new();
    if !c.player_options.is_empty() {
        let options = c.player_options.iter().map(|n| CreateSelectMenuOption::new(n, n)).collect();
        rows.push(CreateActionRow::SelectMenu(
            CreateSelectMenu::new(player_menu_id(channel, &c.server), CreateSelectMenuKind::String { options }).placeholder("🎯 Player card…"),
        ));
    }
    if let Some(url) = &c.watch_url {
        rows.push(CreateActionRow::Buttons(vec![CreateButton::new_link(url).label("Watch live video").emoji('📺')]));
    }
    rows
}

pub struct DiscordChat {
    pub http: Arc<Http>,
}

#[async_trait]
impl Chat for DiscordChat {
    async fn send(&self, channel: u64, embed: Embed, comps: Option<Components>) -> Result<u64, ChatError> {
        let mut m = CreateMessage::new().embed(to_create_embed(&embed));
        if let Some(c) = comps {
            m = m.components(to_rows(channel, &c));
        }
        let msg = ChannelId::new(channel).send_message(&self.http, m).await.map_err(map_err)?;
        Ok(msg.id.get())
    }

    async fn edit(&self, channel: u64, message: u64, embed: Embed, comps: Option<Components>) -> Result<(), ChatError> {
        let mut m = EditMessage::new().embed(to_create_embed(&embed));
        if let Some(c) = comps {
            m = m.components(to_rows(channel, &c));
        }
        ChannelId::new(channel).edit_message(&self.http, MessageId::new(message), m).await.map_err(map_err)?;
        Ok(())
    }

    async fn delete(&self, channel: u64, message: u64) -> Result<(), ChatError> {
        ChannelId::new(channel).delete_message(&self.http, MessageId::new(message)).await.map_err(map_err)
    }

    async fn react(&self, channel: u64, message: u64, emoji: &str) -> Result<(), ChatError> {
        self.http
            .create_reaction(ChannelId::new(channel), MessageId::new(message), &ReactionType::Unicode(emoji.to_string()))
            .await
            .map_err(map_err)
    }
}

fn int_opt(name: &str, desc: &str, required: bool, min: u64, max: u64) -> CreateCommandOption {
    CreateCommandOption::new(CommandOptionType::Integer, name, desc).required(required).min_int_value(min).max_int_value(max)
}

/// The servers `/gtv rcon` works on right now: switched on with the rcon password entered, and only while an admin role or at
/// least one person is chosen.
pub fn rcon_offered(cfg: &crate::config::Config) -> Vec<String> {
    if cfg.discord.rcon_role_ids().is_empty() && cfg.discord.rcon_user_ids().is_empty() {
        return vec![];
    }
    cfg.servers.iter().filter(|(_, s)| s.rcon_enabled()).map(|(k, _)| k.clone()).collect()
}

/// May this person run `/gtv rcon`? Needs a guild, and either to be one of the people chosen in the panel or to hold one of the
/// chosen admin roles. Manage Server is NOT enough on its own, and the guild's @everyone role (its id is the guild's id) never
/// counts, whatever is configured.
pub fn rcon_authorized(cfg: &crate::config::Config, guild_id: Option<u64>, user_id: u64, member_roles: &[u64]) -> Result<(), String> {
    let Some(guild) = guild_id else { return Err("Use this in a server channel, not in a direct message.".into()) };
    let roles: Vec<u64> = cfg.discord.rcon_role_ids().into_iter().filter(|r| *r != guild).collect();
    let users = cfg.discord.rcon_user_ids();
    if roles.is_empty() && users.is_empty() {
        return Err("No admin role or person is chosen for the rcon commands yet (admin panel, Settings tab).".into());
    }
    if users.contains(&user_id) || member_roles.iter().any(|r| roles.contains(r)) {
        Ok(())
    } else {
        Err("Only the people and the server-admin role chosen for the rcon commands can use them.".into())
    }
}

/// Which server a `/gtv rcon` command is for: the named one, or the only one that has rcon switched on.
pub fn rcon_pick_server<'a>(cfg: &'a crate::config::Config, given: Option<&str>) -> Result<(&'a str, &'a crate::config::ServerConfig), String> {
    let on: Vec<(&String, &crate::config::ServerConfig)> = cfg.servers.iter().filter(|(_, s)| s.rcon_enabled()).collect();
    match (given, on.len()) {
        (_, 0) => Err("Rcon commands are not switched on for any server.".into()),
        (Some(k), _) => on.iter().find(|(key, _)| key.as_str() == k).map(|(key, s)| (key.as_str(), *s)).ok_or_else(|| format!("`{}` does not have rcon commands switched on.", k.replace('`', "'"))),
        (None, 1) => Ok((on[0].0.as_str(), on[0].1)),
        (None, _) => Err(format!("Which server? Add `server:` - one of {}.", on.iter().map(|(k, _)| format!("`{}`", k)).collect::<Vec<_>>().join(", "))),
    }
}

/// `/gtv rcon <action> ...`: the action name and its options, when this interaction is an rcon one.
fn rcon_call<'a>(opts: Vec<ResolvedOption<'a>>) -> Option<(&'a str, Vec<ResolvedOption<'a>>)> {
    match opts.into_iter().next()? {
        ResolvedOption { name: "rcon", value: ResolvedValue::SubCommandGroup(inner), .. } => match inner.into_iter().next()? {
            ResolvedOption { name, value: ResolvedValue::SubCommand(o), .. } => Some((name, o)),
            _ => None,
        },
        _ => None,
    }
}

/// The servers `/gtv rcon` can be sent to, as (key, label for the drop-down).
pub fn rcon_choices(cfg: &crate::config::Config) -> Vec<(String, String)> {
    rcon_offered(cfg)
        .into_iter()
        .map(|k| {
            let label = cfg.servers.get(&k).map(|s| format!("{} - {}", k, s.display_name())).unwrap_or_else(|| k.clone());
            (k, label.chars().take(100).collect())
        })
        .collect()
}

/// The `rcon` group of `/gtv`: `/gtv rcon cyclemap | slap | kick | bigtext | ban`.
/// With two or more servers the person must pick the server first, from a drop-down of exactly the servers that have rcon
/// switched on (Discord allows 25 choices; beyond that it is typed with suggestions). With one server there is nothing to pick.
fn rcon_group(servers: &[(String, String)]) -> CreateCommandOption {
    let pick = servers.len() > 1 && servers.len() <= 25;
    let server = || -> Option<CreateCommandOption> {
        if pick {
            let mut o = CreateCommandOption::new(CommandOptionType::String, "server", "Which game server to send this to").required(true);
            for (k, label) in servers {
                o = o.add_string_choice(label.clone(), k.clone());
            }
            Some(o)
        } else if servers.len() > 25 {
            Some(CreateCommandOption::new(CommandOptionType::String, "server", "Which game server to send this to").required(false).set_autocomplete(true))
        } else {
            None
        }
    };
    let player = |d: &str| CreateCommandOption::new(CommandOptionType::String, "player", d.to_string()).required(true).set_autocomplete(true).max_length(60);
    let text = |name: &str, d: &str, min: u16, req: bool| CreateCommandOption::new(CommandOptionType::String, name, d.to_string()).required(req).min_length(min).max_length(80);
    // required options come first in Discord, so a required server goes before the rest, an optional one after it
    let sub = |name: &str, desc: &str, rest: Vec<CreateCommandOption>| {
        let mut o = CreateCommandOption::new(CommandOptionType::SubCommand, name, desc.to_string());
        if pick {
            o = o.add_sub_option(server().unwrap());
        }
        for r in rest {
            o = o.add_sub_option(r);
        }
        if !pick {
            if let Some(sv) = server() {
                o = o.add_sub_option(sv);
            }
        }
        o
    };
    CreateCommandOption::new(CommandOptionType::SubCommandGroup, "rcon", "Server-admin commands for a game server (server-admin role only)")
        .add_sub_option(sub("cyclemap", "Go to the next map in the rotation", vec![]))
        .add_sub_option(sub("slap", "Slap a player", vec![player("Player name or number")]))
        .add_sub_option(sub("kick", "Kick a player", vec![player("Player name or number"), text("reason", "Shown to the player (optional)", 1, false)]))
        .add_sub_option(sub("bigtext", "Show a large message on every player's screen", vec![text("message", "What to show", 1, true)]))
        .add_sub_option(sub("ban", "Ban a player's address and kick them (a reason is required)", vec![player("Player name or number"), text("reason", "Why (shown to the player, kept in the bot's log)", 3, true)]))
}

pub fn build_commands(server_keys: &[String], rcon_servers: &[(String, String)]) -> Vec<CreateCommand> {
    let server = CreateCommandOption::new(CommandOptionType::String, "server", format!("Which configured server ({})", server_keys.join(", ")).chars().take(100).collect::<String>())
        .required(true)
        .set_autocomplete(true);
    let pick = |desc: &str| CreateCommandOption::new(CommandOptionType::String, "server", desc).required(false).set_autocomplete(true);
    let mut gtv = CreateCommand::new("gtv").description("Watch an Urban Terror server from Discord").dm_permission(false);
    if rcon_servers.is_empty() {
        // Discord hides /gtv from everyone without Manage Server. (While `/gtv rcon` is on this is left off, because the
        // people who hold the rcon admin role often do not have Manage Server and could not see the command at all; every
        // sub-command still checks its own permission when it is used, see `allowed` and `rcon_authorized`.)
        gtv = gtv.default_member_permissions(Permissions::MANAGE_GUILD);
    }
    let mut gtv = gtv
        .add_option(
            CreateCommandOption::new(CommandOptionType::SubCommand, "start", "Start broadcasting a server in this channel")
                .add_sub_option(server)
                .add_sub_option(int_opt("delay", "Anti-ghosting delay in seconds", false, 0, 180))
                .add_sub_option(CreateCommandOption::new(CommandOptionType::Boolean, "stream", "Also start the spectator client + video stream")),
        )
        .add_option(
            CreateCommandOption::new(CommandOptionType::SubCommand, "stop", "Stop a broadcast in this channel")
                .add_sub_option(pick("Which server (needed when this channel shows more than one)"))
                .add_sub_option(CreateCommandOption::new(CommandOptionType::Boolean, "all", "Stop every broadcast in this channel").required(false)),
        )
        .add_option(
            CreateCommandOption::new(CommandOptionType::SubCommand, "delay", "Change the anti-ghosting delay")
                .add_sub_option(int_opt("seconds", "Delay in seconds", true, 0, 180))
                .add_sub_option(pick("Which server (needed when this channel shows more than one)")),
        )
        .add_option(
            CreateCommandOption::new(CommandOptionType::SubCommand, "stream", "Turn the spectator video stream on or off")
                .add_sub_option(CreateCommandOption::new(CommandOptionType::Boolean, "enable", "On or off").required(true))
                .add_sub_option(pick("Which server (needed when this channel shows more than one)")),
        )
        .add_option(
            CreateCommandOption::new(CommandOptionType::SubCommand, "cam", "Point the spectator camera (empty = auto director)")
                .add_sub_option(CreateCommandOption::new(CommandOptionType::String, "player", "Player to follow").set_autocomplete(true))
                .add_sub_option(int_opt("seconds", "How long to stay on them", false, 5, 600))
                .add_sub_option(pick("Which server (needed when this channel shows more than one)")),
        )
        .add_option(CreateCommandOption::new(CommandOptionType::SubCommand, "list", "Everything that is broadcasting now, in every channel"))
        .add_option(CreateCommandOption::new(CommandOptionType::SubCommand, "about", "Version, owner and status of this bot"));
    if !rcon_servers.is_empty() {
        gtv = gtv.add_option(rcon_group(rcon_servers));
    }
    vec![gtv]
}

/// Replace the bot's slash commands (the `/gtv rcon` group is part of the set only while it is switched on).
pub async fn refresh_commands(http: &Http, cfg: &crate::config::Config) {
    let keys: Vec<String> = cfg.servers.keys().cloned().collect();
    match Command::set_global_commands(http, build_commands(&keys, &rcon_choices(cfg))).await {
        Ok(_) => tracing::info!("slash commands updated"),
        Err(e) => tracing::error!("updating the slash commands failed: {}", e),
    }
}

struct Handler {
    app: Arc<App>,
    /// When each person last ran a `/gtv rcon` command (a pause of a couple of seconds between them).
    rcon_last: std::sync::Mutex<std::collections::HashMap<u64, std::time::Instant>>,
}

impl Handler {
    fn allowed(&self, ci: &CommandInteraction) -> bool {
        let restrict = &self.app.cfg().discord.restrict_to_user_ids;
        if !restrict.is_empty() {
            return restrict.iter().any(|id| *id == ci.user.id.get().to_string());
        }
        ci.member.as_ref().and_then(|m| m.permissions).is_some_and(|p| p.manage_guild() || p.administrator())
    }

    async fn reply(&self, ctx: &Context, ci: &CommandInteraction, text: &str, ephemeral: bool) {
        let m = CreateInteractionResponseMessage::new().content(text).ephemeral(ephemeral);
        if let Err(e) = ci.create_response(&ctx.http, CreateInteractionResponse::Message(m)).await {
            tracing::warn!("could not answer a command: {}", e);
        }
    }

    /// For slow commands: acknowledge first, then post the result as a follow-up.
    async fn deferred(&self, ctx: &Context, ci: &CommandInteraction, result: Result<String, String>) {
        let text = result.unwrap_or_else(|e| e);
        if let Err(e) = ci.create_followup(&ctx.http, CreateInteractionResponseFollowup::new().content(text)).await {
            tracing::warn!("could not post a command result: {}", e);
        }
    }

    /// The admin role check for a command or an autocomplete, from what the interaction itself carries.
    fn rcon_check(&self, ci: &CommandInteraction) -> Result<Arc<crate::config::Config>, String> {
        let cfg = self.app.cfg();
        let roles: Vec<u64> = ci.member.as_ref().map(|m| m.roles.iter().map(|r| r.get()).collect()).unwrap_or_default();
        rcon_authorized(&cfg, ci.guild_id.map(|g| g.get()), ci.user.id.get(), &roles)?;
        Ok(cfg)
    }

    /// `/gtv rcon cyclemap | slap | kick | bigtext | ban`. Every answer is private to the person who ran it.
    async fn rcon(&self, ctx: &Context, ci: CommandInteraction) {
        let cfg = match self.rcon_check(&ci) {
            Ok(c) => c,
            Err(e) => return self.reply(ctx, &ci, &e, true).await,
        };
        let Some((sub, opts)) = rcon_call(ci.data.options()) else { return };
        let get = |n: &str| opts.iter().find(|o| o.name == n).and_then(|o| if let ResolvedValue::String(s) = &o.value { Some(s.to_string()) } else { None });
        let (key, server) = match rcon_pick_server(&cfg, get("server").as_deref()) {
            Ok(x) => x,
            Err(e) => return self.reply(ctx, &ci, &e, true).await,
        };
        let action = match sub {
            "cyclemap" => crate::rcon::Action::Cyclemap,
            "slap" => crate::rcon::Action::Slap { player: get("player").unwrap_or_default() },
            "kick" => crate::rcon::Action::Kick { player: get("player").unwrap_or_default(), reason: get("reason").unwrap_or_default() },
            "bigtext" => crate::rcon::Action::Bigtext { text: get("message").unwrap_or_default() },
            "ban" => crate::rcon::Action::Ban { player: get("player").unwrap_or_default(), reason: get("reason").unwrap_or_default() },
            _ => return,
        };
        let wait = {
            let mut last = self.rcon_last.lock().unwrap();
            let user = ci.user.id.get();
            let since = last.get(&user).map(|t| t.elapsed().as_secs_f64()).filter(|e| *e < 2.0);
            if since.is_none() {
                last.insert(user, std::time::Instant::now());
            }
            since.map(|e| 2.0 - e)
        };
        if let Some(w) = wait {
            return self.reply(ctx, &ci, &format!("One moment - try again in {:.0} s.", w.ceil().max(1.0)), true).await;
        }
        let _ = ci.defer_ephemeral(&ctx.http).await;
        let who = format!("{} ({})", ci.user.name, ci.user.id.get());
        let result = crate::rcon::run(server, &action).await;
        match &result {
            Ok((_, audit)) => tracing::info!("rcon: {} ran `{}` on {}: {}", who, action.verb(), key, audit),
            Err(e) => tracing::warn!("rcon: {} tried `{}` on {} and it did not go through: {}", who, action.verb(), key, e),
        }
        let text = match result {
            Ok((summary, _)) => format!("✅ {}", summary),
            Err(e) => format!("⚠️ {}", e),
        };
        self.deferred(ctx, &ci, Ok(text)).await;
    }

    /// Server and player suggestions for `/gtv rcon` (only for people who may use it).
    async fn rcon_autocomplete(&self, ctx: &Context, ai: CommandInteraction) {
        let mut resp = CreateAutocompleteResponse::new();
        if let (Ok(cfg), Some(focus)) = (self.rcon_check(&ai), ai.data.autocomplete()) {
            let cur = focus.value.to_lowercase();
            let given = rcon_call(ai.data.options()).and_then(|(_, o)| o.iter().find(|o| o.name == "server").and_then(|o| if let ResolvedValue::String(s) = &o.value { Some(s.to_string()) } else { None }));
            if focus.name == "server" {
                for (k, s) in cfg.servers.iter().filter(|(k, s)| s.rcon_enabled() && k.to_lowercase().contains(&cur)).take(25) {
                    resp = resp.add_string_choice(format!("{} - {}", k, s.display_name()).chars().take(100).collect::<String>(), k.clone());
                }
            } else if focus.name == "player" {
                if let Ok((_, server)) = rcon_pick_server(&cfg, given.as_deref()) {
                    // the public status query (no password): names only, answered within Discord's 3-second limit
                    let snap = tokio::time::timeout(std::time::Duration::from_millis(2200), crate::urt::query_status(&server.host, server.port)).await;
                    for p in snap.map(|s| s.players).unwrap_or_default().into_iter().filter(|p| p.name.to_lowercase().contains(&cur)).take(25) {
                        let n: String = p.name.chars().take(60).collect();
                        resp = resp.add_string_choice(n.clone(), n);
                    }
                }
            }
        }
        let _ = ai.create_response(&ctx.http, CreateInteractionResponse::Autocomplete(resp)).await;
    }

    async fn command(&self, ctx: &Context, ci: CommandInteraction) {
        if ci.data.name != "gtv" {
            return;
        }
        if matches!(ci.data.options.first(), Some(o) if o.name == "rcon") {
            return self.rcon(ctx, ci).await;
        }
        let Some(ResolvedOption { name: sub, value: ResolvedValue::SubCommand(opts), .. }) = ci.data.options().into_iter().next() else { return };
        if sub != "about" && !self.allowed(&ci) {
            return self.reply(ctx, &ci, "You need **Manage Server** to control the broadcast.", true).await;
        }
        let channel = ci.channel_id.get();
        let get_str = |n: &str| opts.iter().find(|o| o.name == n).and_then(|o| if let ResolvedValue::String(s) = &o.value { Some(s.to_string()) } else { None });
        let get_int = |n: &str| opts.iter().find(|o| o.name == n).and_then(|o| if let ResolvedValue::Integer(i) = o.value { Some(i) } else { None });
        let server = get_str("server").filter(|_| sub != "start");
        let get_bool = |n: &str| opts.iter().find(|o| o.name == n).and_then(|o| if let ResolvedValue::Boolean(b) = o.value { Some(b) } else { None });
        match sub {
            "start" => {
                let server = get_str("server").unwrap_or_default();
                let (delay, stream) = (get_int("delay").unwrap_or(0).clamp(0, 180) as u32, get_bool("stream").unwrap_or(false));
                if !self.app.cfg().servers.contains_key(&server) {
                    return self.reply(ctx, &ci, "Unknown server.", true).await;
                }
                if self.app.watch(channel, &server).is_some() {
                    return self.reply(ctx, &ci, &format!("`{}` is already broadcasting in this channel. Use `/gtv stop server:{}` first, or start another server here.", server, server), true).await;
                }
                let _ = ci.defer(&ctx.http).await;
                let r = self.app.start_watch(channel, &server, delay, stream).await;
                self.deferred(ctx, &ci, r).await;
            }
            "stop" => {
                if get_bool("all").unwrap_or(false) {
                    let _ = ci.defer(&ctx.http).await;
                    let r = self.app.stop_channel(channel).await;
                    return self.deferred(ctx, &ci, r).await;
                }
                if let Err(e) = self.app.pick(channel, server.as_deref()) {
                    return self.reply(ctx, &ci, &e, true).await;
                }
                let _ = ci.defer(&ctx.http).await;
                let r = self.app.stop_watch(channel, server.as_deref()).await;
                self.deferred(ctx, &ci, r).await;
            }
            "delay" => {
                let r = self.app.cmd_delay(channel, server.as_deref(), get_int("seconds").unwrap_or(0).clamp(0, 180) as u32);
                self.reply(ctx, &ci, &r.unwrap_or_else(|e| e), true).await;
            }
            "stream" => {
                if let Err(e) = self.app.pick(channel, server.as_deref()) {
                    return self.reply(ctx, &ci, &e, true).await;
                }
                let _ = ci.defer(&ctx.http).await;
                let r = self.app.cmd_stream(channel, server.as_deref(), get_bool("enable").unwrap_or(false)).await;
                self.deferred(ctx, &ci, r).await;
            }
            "cam" => {
                let r = self.app.cmd_cam(channel, server.as_deref(), get_str("player"), get_int("seconds").unwrap_or(60).clamp(5, 600) as u32).await;
                self.reply(ctx, &ci, &r.unwrap_or_else(|e| e), true).await;
            }
            "list" => {
                let text = list_text(&self.app.broadcasts());
                self.reply(ctx, &ci, &text, true).await;
            }
            "about" => {
                let m = CreateInteractionResponseMessage::new().embed(to_create_embed(&self.app.about_embed())).ephemeral(true);
                let _ = ci.create_response(&ctx.http, CreateInteractionResponse::Message(m)).await;
            }
            _ => {}
        }
    }

    async fn autocomplete(&self, ctx: &Context, ai: CommandInteraction) {
        if matches!(ai.data.options.first(), Some(o) if o.name == "rcon") {
            return self.rcon_autocomplete(ctx, ai).await;
        }
        let Some(focus) = ai.data.autocomplete() else { return };
        let cur = focus.value.to_lowercase();
        let channel = ai.channel_id.get();
        let (sub, given) = match ai.data.options().into_iter().next() {
            Some(ResolvedOption { name, value: ResolvedValue::SubCommand(o), .. }) =>
                (name.to_string(), o.iter().find(|o| o.name == "server").and_then(|o| if let ResolvedValue::String(s) = &o.value { Some(s.to_string()) } else { None })),
            _ => (String::new(), None),
        };
        let mut resp = CreateAutocompleteResponse::new();
        if focus.name == "server" && sub == "start" {
            for (k, s) in self.app.cfg().servers.iter().filter(|(k, _)| k.to_lowercase().contains(&cur)).take(25) {
                let live = if self.app.watch(channel, k).is_some() { " (already here)" } else { "" };
                resp = resp.add_string_choice(format!("{} - {}{}", k, s.display_name(), live).chars().take(100).collect::<String>(), k.clone());
            }
        } else if focus.name == "server" {
            // the other commands act on a broadcast that is running in this channel
            for w in self.app.watches_in(channel).into_iter().filter(|w| w.cfg.key.to_lowercase().contains(&cur)).take(25) {
                resp = resp.add_string_choice(format!("{} - {}", w.cfg.key, w.cfg.display_name()).chars().take(100).collect::<String>(), w.cfg.key.clone());
            }
        } else if let Ok(w) = self.app.pick(channel, given.as_deref()) {
            for n in w.player_names().into_iter().filter(|n| n.to_lowercase().contains(&cur)).take(25) {
                resp = resp.add_string_choice(n.chars().take(100).collect::<String>(), n.chars().take(100).collect::<String>());
            }
        }
        let _ = ai.create_response(&ctx.http, CreateInteractionResponse::Autocomplete(resp)).await;
    }

    async fn component(&self, ctx: &Context, ci: ComponentInteraction) {
        let Some((chan, server)) = parse_player_menu_id(&ci.data.custom_id) else { return };
        let ComponentInteractionDataKind::StringSelect { values } = &ci.data.kind else { return };
        let msg = match (self.app.pick(chan, server.as_deref()).ok(), values.first()) {
            (Some(w), Some(name)) => CreateInteractionResponseMessage::new().embed(to_create_embed(&w.player_card(name))).ephemeral(true),
            _ => CreateInteractionResponseMessage::new().content("That broadcast has ended.").ephemeral(true),
        };
        let _ = ci.create_response(&ctx.http, CreateInteractionResponse::Message(msg)).await;
    }
}

#[serenity_async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, ready: Ready) {
        self.app.set_chat(Some(Arc::new(DiscordChat { http: ctx.http.clone() })));
        *self.app.discord.write().unwrap() = DiscordStatus::Ready(ready.user.name.clone());
        self.app.set_discord_http(Some(ctx.http.clone()));
        let cfg = self.app.cfg();
        let keys: Vec<String> = cfg.servers.keys().cloned().collect();
        match Command::set_global_commands(&ctx.http, build_commands(&keys, &rcon_choices(&cfg))).await {
            Ok(_) => tracing::info!("ready as {}; slash commands registered{}", ready.user.name, if rcon_offered(&cfg).is_empty() { "" } else { " (including /gtv rcon)" }),
            Err(e) => tracing::error!("ready as {}, but registering the slash commands failed: {}", ready.user.name, e),
        }
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        match interaction {
            Interaction::Command(ci) => self.command(&ctx, ci).await,
            Interaction::Autocomplete(ai) => self.autocomplete(&ctx, ai).await,
            Interaction::Component(ci) => self.component(&ctx, ci).await,
            _ => {}
        }
    }

    async fn message(&self, ctx: Context, msg: Message) {
        if msg.author.bot {
            return;
        }
        let watches = self.app.watches_in(msg.channel_id.get());
        if watches.is_empty() {
            return;
        }
        let name = msg.member.as_ref().and_then(|m| m.nick.clone()).or_else(|| msg.author.global_name.clone()).unwrap_or_else(|| msg.author.name.clone());
        // a channel can show several servers: the message goes to each one that relays chat
        let mut relayed = false;
        for w in watches {
            relayed |= w.relay(msg.author.id.get(), &name, &msg.content);
        }
        if relayed {
            let _ = msg.react(&ctx.http, ReactionType::Unicode("🎮".into())).await;
        }
    }
}

/// Discord refuses a connection that asks for a privileged intent the bot application has not been given.
pub fn intents_refused(msg: &str) -> bool {
    msg.to_lowercase().contains("disallowed gateway intents")
}

/// The notice shown in the panel when chat relay had to be switched off because of that.
pub const NO_CONTENT_INTENT_NOTE: &str = "Connected, but relaying Discord chat into the game is OFF: Discord refused the \"Message Content Intent\". \
To use the relay, open the Discord Developer Portal, your application, Bot, Privileged Gateway Intents, switch on \"Message Content Intent\", save, \
then press \"Reconnect to Discord\" here. Scoreboards, the live feed, summaries and the slash commands work without it.";

fn failure_hint(msg: String) -> String {
    if msg.to_lowercase().contains("invalid authentication") || msg.contains("4004") {
        "Discord rejected the bot token (check it in the admin panel)".to_string()
    } else if intents_refused(&msg) {
        "Discord refused the connection because of a gateway intent. Open the Discord Developer Portal, your application, Bot, and check the Privileged Gateway Intents switches, then press Reconnect.".to_string()
    } else {
        msg
    }
}

async fn build_client(token: &str, app: &Arc<App>, relay: bool) -> serenity::Result<Client> {
    let mut intents = GatewayIntents::GUILDS;
    if relay {
        // MESSAGE_CONTENT is privileged: the application must have it switched on in the Developer Portal.
        intents |= GatewayIntents::GUILD_MESSAGES | GatewayIntents::MESSAGE_CONTENT;
    }
    Client::builder(token, intents).event_handler(Handler { app: app.clone(), rcon_last: Default::default() }).await
}

/// Connect to Discord (replacing any previous connection). Returns at once; the status is in `app.discord`.
pub async fn connect(app: &Arc<App>) {
    let mut ctl = app.discord_ctl.lock().await;
    if let Some(old) = ctl.take() {
        old.shutdown().await;
        app.set_chat(None);
    }
    app.discord_note.write().unwrap().clear();
    let cfg = app.cfg();
    let token = cfg.discord_token();
    if token.is_empty() {
        *app.discord.write().unwrap() = DiscordStatus::NoToken;
        return;
    }
    let relay = cfg.servers.values().any(|s| s.relay_chat && !s.rcon_password.is_empty());
    *app.discord.write().unwrap() = DiscordStatus::Connecting;
    let built = build_client(&token, app, relay).await;
    let mut client = match built {
        Ok(c) => c,
        Err(e) => {
            *app.discord.write().unwrap() = DiscordStatus::Failed(e.to_string());
            tracing::error!("cannot set up the Discord client: {}", e);
            return;
        }
    };
    let shard_manager = client.shard_manager.clone();
    let app2 = app.clone();
    let task = tokio::spawn(async move {
        let Err(e) = client.start().await else { return };
        let msg = e.to_string();
        tracing::error!("Discord connection ended: {}", msg);
        if relay && intents_refused(&msg) {
            // Do not stay offline for the sake of one optional feature: connect again without the privileged intent.
            tracing::warn!("Discord refused the Message Content Intent - connecting again without the chat relay (switch the intent on in the Developer Portal to use it)");
            match build_client(&token, &app2, false).await {
                Ok(mut again) => {
                    {
                        let mut ctl = app2.discord_ctl.lock().await;
                        match ctl.as_mut() {
                            Some(h) => h.shard_manager = again.shard_manager.clone(),
                            None => return, // disconnected meanwhile
                        }
                    }
                    *app2.discord_note.write().unwrap() = NO_CONTENT_INTENT_NOTE.to_string();
                    if let Err(e2) = again.start().await {
                        let m2 = e2.to_string();
                        tracing::error!("Discord connection ended: {}", m2);
                        app2.discord_note.write().unwrap().clear();
                        *app2.discord.write().unwrap() = DiscordStatus::Failed(failure_hint(m2));
                        app2.set_chat(None);
                    }
                }
                Err(e2) => {
                    *app2.discord.write().unwrap() = DiscordStatus::Failed(failure_hint(e2.to_string()));
                    app2.set_chat(None);
                }
            }
            return;
        }
        *app2.discord.write().unwrap() = DiscordStatus::Failed(failure_hint(msg));
        app2.set_chat(None);
    });
    *ctl = Some(DiscordHandle { shard_manager, task });
}

pub async fn disconnect(app: &Arc<App>) {
    if let Some(h) = app.discord_ctl.lock().await.take() {
        h.shutdown().await;
    }
    app.set_chat(None);
    *app.discord.write().unwrap() = DiscordStatus::Stopped;
}

/// Ask Discord who this token belongs to (used by the admin panel to validate a token before saving it).
pub async fn check_token(token: &str) -> Result<(String, String), String> {
    let http = Http::new(token);
    let app = http.get_current_application_info().await.map_err(|e| {
        let m = e.to_string();
        if m.contains("401") || m.to_lowercase().contains("unauthorized") { "Discord rejected this token".to_string() } else { m }
    })?;
    Ok((app.name, app.id.get().to_string()))
}

pub fn invite_url(app_id: &str) -> String {
    // View Channels, Send Messages, Embed Links, Add Reactions, Read Message History + the slash-command scope
    format!("https://discord.com/oauth2/authorize?client_id={}&scope=bot%20applications.commands&permissions=84032", app_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::BroadcastInfo;

    #[test]
    fn a_refused_privileged_intent_is_recognised_and_explained() {
        // the exact text serenity produced in a real log
        assert!(intents_refused("Disallowed gateway intents were provided"));
        assert!(intents_refused("Gateway error: Disallowed Gateway Intents were provided"));
        assert!(!intents_refused("Invalid authentication token"));
        assert!(failure_hint("Disallowed gateway intents were provided".into()).contains("Privileged Gateway Intents"));
        assert!(failure_hint("Invalid authentication token".into()).contains("rejected the bot token"));
        assert_eq!(failure_hint("some other error".into()), "some other error");
        assert!(NO_CONTENT_INTENT_NOTE.contains("Message Content Intent") && NO_CONTENT_INTENT_NOTE.contains("Reconnect"));
    }

    #[test]
    fn the_player_menu_id_says_which_server_it_belongs_to() {
        assert_eq!(player_menu_id(7, "pro"), "gtv:player:7:pro");
        assert_eq!(parse_player_menu_id("gtv:player:7:pro"), Some((7, Some("pro".into()))));
        // menus posted by an older version carry only the channel
        assert_eq!(parse_player_menu_id("gtv:player:7"), Some((7, None)));
        assert_eq!(parse_player_menu_id("gtv:other:7"), None);
        assert_eq!(parse_player_menu_id("gtv:player:abc:pro"), None);
    }

    #[test]
    fn the_list_shows_every_broadcast_with_its_channel_and_link() {
        assert!(list_text(&[]).contains("Nothing is broadcasting"));
        let b = |server: &str, ch: &str, url: Option<&str>, port: u16| BroadcastInfo {
            channel_id: ch.into(), server: server.into(), name: format!("Name {}", server), delay: 20, video: url.is_some(),
            watch_url: url.map(String::from), viewers: 3, viewers_open: 3, web_port: port,
        };
        let t = list_text(&[b("a", "11", Some("http://h:8081/watch/a"), 8081), b("b", "11", None, 0)]);
        assert!(t.contains("(2)") && t.contains("<#11>"), "{}", t);
        assert!(t.contains("http://h:8081/watch/a") && t.contains("port 8081") && t.contains("👁 3"), "{}", t);
        assert_eq!(t.matches("<#11>").count(), 2, "both servers of the same channel are listed");
        let many: Vec<_> = (0..200).map(|i| b(&format!("s{}", i), "1", None, 0)).collect();
        assert!(list_text(&many).chars().count() <= 2000, "a Discord message may hold 2000 characters");
    }

    fn rcon_cfg(roles: &[&str], servers: &[(&str, &str, bool)]) -> crate::config::Config {
        let mut c = crate::config::Config::default();
        c.discord.rcon_roles = roles.iter().map(|r| r.to_string()).collect();
        for (k, pw, on) in servers {
            c.servers.insert(k.to_string(), crate::config::ServerConfig { host: "h".into(), rcon_password: pw.to_string(), rcon_commands: *on, ..Default::default() });
        }
        c
    }

    #[test]
    fn only_members_with_the_chosen_role_may_use_rcon() {
        let c = rcon_cfg(&["555", "666"], &[("a", "pw", true)]);
        let g = Some(1u64);
        let me = 42u64; // not one of the chosen people in this test
        assert!(rcon_authorized(&c, g, me, &[10, 666]).is_ok());
        assert!(rcon_authorized(&c, g, me, &[555]).is_ok());
        assert!(rcon_authorized(&c, g, me, &[10, 11]).unwrap_err().contains("server-admin role"));
        assert!(rcon_authorized(&c, g, me, &[]).is_err());
        assert!(rcon_authorized(&c, None, me, &[555]).unwrap_err().contains("direct message"));
        // no role chosen = nobody, even someone holding "a role"
        assert!(rcon_authorized(&rcon_cfg(&[], &[("a", "pw", true)]), g, me, &[555]).unwrap_err().contains("No admin role"));
        // text that is not a role id is ignored rather than matching anything
        assert!(rcon_authorized(&rcon_cfg(&["abc", "0", ""], &[]), g, 0, &[0]).unwrap_err().contains("No admin role"));
        // the guild's @everyone role has the guild's own id: choosing it must not open the commands to everybody
        let all = rcon_cfg(&["1"], &[("a", "pw", true)]);
        assert!(rcon_authorized(&all, Some(1), me, &[1]).unwrap_err().contains("No admin role"));
        assert!(rcon_authorized(&all, Some(1), me, &[]).is_err());
    }

    #[test]
    fn chosen_people_may_use_rcon_with_or_without_the_role() {
        let g = Some(1u64);
        // people only, no role at all
        let mut c = rcon_cfg(&[], &[("a", "pw", true)]);
        c.discord.rcon_users = vec!["77".into(), " 88 ".into()];
        assert!(rcon_authorized(&c, g, 77, &[]).is_ok());
        assert!(rcon_authorized(&c, g, 88, &[999]).is_ok());
        assert!(rcon_authorized(&c, g, 78, &[]).unwrap_err().contains("Only the people"), "someone not chosen");
        assert!(rcon_authorized(&c, g, 0, &[]).is_err());
        assert!(rcon_authorized(&c, None, 77, &[]).unwrap_err().contains("direct message"), "never in direct messages");
        assert_eq!(rcon_offered(&c), vec!["a"], "a person alone is enough to offer the command");
        // a person AND a role: either one opens it, and nobody else
        c.discord.rcon_roles = vec!["555".into()];
        assert!(rcon_authorized(&c, g, 77, &[]).is_ok());
        assert!(rcon_authorized(&c, g, 5, &[555]).is_ok());
        assert!(rcon_authorized(&c, g, 5, &[556]).is_err());
        // text that is not a user id, and 0, never match anybody
        let mut bad = rcon_cfg(&[], &[("a", "pw", true)]);
        bad.discord.rcon_users = vec!["abc".into(), "0".into(), "".into()];
        assert!(rcon_authorized(&bad, g, 0, &[]).unwrap_err().contains("No admin role or person"));
        assert!(rcon_offered(&bad).is_empty());
        // being the owner of the guild or holding Manage Server is not a reason; only the lists count
        assert!(rcon_authorized(&rcon_cfg(&[], &[("a", "pw", true)]), g, 77, &[]).is_err());
    }

    #[test]
    fn rcon_exists_only_when_switched_on_with_a_password_and_a_role() {
        let on = rcon_cfg(&["555"], &[("a", "pw", true), ("b", "pw", false), ("c", "", true)]);
        assert_eq!(rcon_offered(&on), vec!["a"]); // b is off, c has no password
        assert!(rcon_offered(&rcon_cfg(&[], &[("a", "pw", true)])).is_empty()); // no role or person chosen
        assert!(rcon_offered(&rcon_cfg(&["555"], &[("a", "", true)])).is_empty()); // switched on but no password
        let names = |c: &crate::config::Config| -> Vec<String> {
            let v = serde_json::to_value(build_commands(&["a".into()], &rcon_choices(c))).unwrap();
            v.as_array().unwrap().iter().map(|x| x["name"].as_str().unwrap().to_string()).collect()
        };
        // there is never a command of its own: rcon is a group inside /gtv, and only while it is switched on
        let groups = |c: &crate::config::Config| -> Vec<String> {
            let v = serde_json::to_value(build_commands(&["a".into()], &rcon_choices(c))).unwrap();
            v[0]["options"].as_array().unwrap().iter().filter(|o| o["type"] == 2).map(|o| o["name"].as_str().unwrap().to_string()).collect()
        };
        assert_eq!(names(&on), vec!["gtv"]);
        assert_eq!(groups(&on), vec!["rcon"]);
        assert!(groups(&rcon_cfg(&["555"], &[("a", "", true)])).is_empty());
        assert!(groups(&rcon_cfg(&[], &[("a", "pw", true)])).is_empty());
        // the Manage Server default hides /gtv from other people; it stays while rcon is off and is left off while it is on
        let perm = |c: &crate::config::Config| serde_json::to_value(build_commands(&["a".into()], &rcon_choices(c))).unwrap()[0]["default_member_permissions"].clone();
        assert!(perm(&rcon_cfg(&[], &[("a", "pw", true)])).is_string());
        assert!(perm(&on).is_null());
    }

    #[test]
    fn a_gtv_rcon_call_is_told_apart_from_the_other_gtv_commands() {
        // the shape Discord sends for `/gtv rcon slap player:Bob` and for `/gtv start`
        let data = |opts: serde_json::Value| -> CommandData {
            serde_json::from_value(serde_json::json!({"id": "1", "name": "gtv", "type": 1, "options": opts})).unwrap()
        };
        let slap = data(serde_json::json!([{"name": "rcon", "type": 2, "options": [{"name": "slap", "type": 1, "options": [{"name": "player", "type": 3, "value": "Bob"}]}]}]));
        let (sub, opts) = rcon_call(slap.options()).unwrap();
        assert_eq!((sub, opts.len(), opts[0].name), ("slap", 1, "player"));
        assert!(matches!(&opts[0].value, ResolvedValue::String("Bob")));
        let start = data(serde_json::json!([{"name": "start", "type": 1, "options": []}]));
        assert!(rcon_call(start.options()).is_none(), "a plain /gtv start is not an rcon call");
        assert!(rcon_call(data(serde_json::json!([])).options()).is_none());
    }

    fn rcon_subs(servers: &[(&str, &str)]) -> Vec<serde_json::Value> {
        let sv: Vec<(String, String)> = servers.iter().map(|(k, l)| (k.to_string(), l.to_string())).collect();
        let v = serde_json::to_value(build_commands(&["a".into()], &sv)).unwrap();
        assert!(v.as_array().unwrap().iter().all(|c| c["name"] != "rcon"), "no top-level /rcon any more");
        assert_eq!(v[0]["dm_permission"], false);
        let rcon = v[0]["options"].as_array().unwrap().iter().find(|o| o["name"] == "rcon").unwrap();
        assert_eq!(rcon["type"], 2, "a sub-command group: /gtv rcon <action>");
        rcon["options"].as_array().unwrap().clone()
    }

    fn sub_opts(subs: &[serde_json::Value], n: &str) -> Vec<serde_json::Value> {
        subs.iter().find(|o| o["name"] == n).unwrap()["options"].as_array().cloned().unwrap_or_default()
    }

    #[test]
    fn the_rcon_command_has_exactly_the_five_actions_and_a_ban_needs_a_reason() {
        let subs = rcon_subs(&[("a", "a - One")]);
        let names: Vec<&str> = subs.iter().map(|o| o["name"].as_str().unwrap()).collect();
        assert_eq!(names, vec!["cyclemap", "slap", "kick", "bigtext", "ban"]);
        let ban = sub_opts(&subs, "ban");
        let reason = ban.iter().find(|o| o["name"] == "reason").unwrap();
        assert_eq!((reason["required"] == true, reason["min_length"].as_u64(), reason["max_length"].as_u64()), (true, Some(3), Some(80)));
        assert!(sub_opts(&subs, "kick").iter().find(|o| o["name"] == "reason").unwrap()["required"] != true);
        for n in ["cyclemap", "slap", "kick", "bigtext", "ban"] {
            // Discord wants the required options first
            let o = sub_opts(&subs, n);
            let first_optional = o.iter().position(|x| x["required"] != true).unwrap_or(o.len());
            assert!(o[first_optional..].iter().all(|x| x["required"] != true), "{} {:?}", n, o);
        }
    }

    #[test]
    fn the_server_is_chosen_from_a_list_when_more_than_one_has_rcon_on() {
        // one server: nothing to choose
        let one = rcon_subs(&[("a", "a - One")]);
        for n in ["cyclemap", "slap", "kick", "bigtext", "ban"] {
            assert!(sub_opts(&one, n).iter().all(|o| o["name"] != "server"), "{}", n);
        }
        // two or more: a required drop-down, FIRST, offering exactly those servers (value = key)
        let two = rcon_subs(&[("a", "a - One"), ("b", "b - Two")]);
        for n in ["cyclemap", "slap", "kick", "bigtext", "ban"] {
            let o = sub_opts(&two, n);
            assert_eq!(o[0]["name"], "server", "{}", n);
            assert_eq!(o[0]["required"], true, "{}", n);
            let ch: Vec<(&str, &str)> = o[0]["choices"].as_array().unwrap().iter().map(|c| (c["name"].as_str().unwrap(), c["value"].as_str().unwrap())).collect();
            assert_eq!(ch, vec![("a - One", "a"), ("b - Two", "b")], "{}", n);
            assert!(o[0]["autocomplete"] != true);
        }
        // more than Discord's 25 choices: typed with suggestions, optional, last
        let many: Vec<(String, String)> = (0..26).map(|i| (format!("s{}", i), format!("s{}", i))).collect();
        let refs: Vec<(&str, &str)> = many.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
        let o = sub_opts(&rcon_subs(&refs), "slap");
        let srv = o.last().unwrap();
        assert!(srv["name"] == "server" && srv["required"] != true && srv["autocomplete"] == true);
    }

    #[test]
    fn the_drop_down_lists_only_servers_that_can_take_rcon_commands() {
        let c = rcon_cfg(&["555"], &[("a", "pw", true), ("b", "pw", false), ("c", "", true), ("d", "pw", true)]);
        let keys: Vec<String> = rcon_choices(&c).into_iter().map(|(k, _)| k).collect();
        assert_eq!(keys, vec!["a", "d"]); // b is off, c has no password
        assert!(rcon_choices(&c).iter().all(|(k, l)| l.starts_with(k.as_str()) && l.chars().count() <= 100));
    }

    #[test]
    fn the_server_for_an_rcon_command_is_named_or_the_only_one() {
        let one = rcon_cfg(&["5"], &[("a", "pw", true), ("b", "pw", false)]);
        assert_eq!(rcon_pick_server(&one, None).unwrap().0, "a");
        assert_eq!(rcon_pick_server(&one, Some("a")).unwrap().0, "a");
        assert!(rcon_pick_server(&one, Some("b")).unwrap_err().contains("not have rcon commands switched on"));
        assert!(rcon_pick_server(&one, Some("zzz")).is_err());
        let two = rcon_cfg(&["5"], &[("a", "pw", true), ("b", "pw", true)]);
        assert!(rcon_pick_server(&two, None).unwrap_err().contains("Which server"));
        assert_eq!(rcon_pick_server(&two, Some("b")).unwrap().0, "b");
        assert!(rcon_pick_server(&rcon_cfg(&["5"], &[("a", "", true)]), None).unwrap_err().contains("not switched on for any"));
    }

    #[test]
    fn the_commands_can_name_a_server_and_there_is_a_list() {
        let v = serde_json::to_value(build_commands(&["a".into(), "b".into()], &[])).unwrap();
        let subs = v[0]["options"].as_array().unwrap();
        let names: Vec<&str> = subs.iter().map(|o| o["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"list") && names.contains(&"start"), "{:?}", names);
        for sub in ["stop", "delay", "stream", "cam"] {
            let o = subs.iter().find(|o| o["name"] == sub).unwrap();
            let server = o["options"].as_array().unwrap().iter().find(|x| x["name"] == "server").unwrap_or_else(|| panic!("/gtv {} has no server option", sub));
            assert!(server["required"] != true && server["autocomplete"] == true, "{} {:?}", sub, server);
        }
        let stop = subs.iter().find(|o| o["name"] == "stop").unwrap();
        assert!(stop["options"].as_array().unwrap().iter().any(|x| x["name"] == "all"));
        // Discord wants required options before optional ones
        let delay = subs.iter().find(|o| o["name"] == "delay").unwrap()["options"].as_array().unwrap().clone();
        assert_eq!(delay[0]["name"], "seconds");
    }
}
