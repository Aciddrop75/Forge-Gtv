// The Forge GTV - Copyright (C) 2026 [TFG]Aciddrop
// SPDX-License-Identifier: GPL-3.0-or-later
// This program comes with ABSOLUTELY NO WARRANTY; see the LICENSE file.

//! Transport-neutral message pieces; `discord.rs` turns them into serenity builders.

use async_trait::async_trait;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Field {
    pub name: String,
    pub value: String,
    pub inline: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Embed {
    pub title: String,
    pub description: String,
    pub color: u32,
    pub fields: Vec<Field>,
    pub footer: String,
    /// Small picture next to the footer text (a URL Discord can fetch).
    pub footer_icon: String,
    /// Line above the title: the clan / community name, with an optional small picture.
    pub author: String,
    pub author_icon: String,
    /// Picture in the top-right corner of the card (the clan logo).
    pub thumbnail: String,
    /// Large picture under the text (the map's level picture).
    pub image: String,
    pub timestamp: bool,
}

impl Embed {
    pub fn new(title: impl Into<String>, color: u32) -> Self {
        Embed { title: title.into(), color, ..Default::default() }
    }
    pub fn desc(mut self, d: impl Into<String>) -> Self {
        self.description = d.into();
        self
    }
    pub fn footer(mut self, f: impl Into<String>) -> Self {
        self.footer = f.into();
        self
    }
    pub fn field(mut self, name: impl Into<String>, value: impl Into<String>, inline: bool) -> Self {
        self.fields.push(Field { name: name.into(), value: value.into(), inline });
        self
    }
}

/// The interactive parts under the live board.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Components {
    /// Player names for the "Player card" drop-down (empty = no drop-down).
    pub player_options: Vec<String>,
    /// "Watch live video" link button.
    pub watch_url: Option<String>,
    /// Which server the drop-down belongs to (a channel can show several).
    pub server: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ChatError {
    /// The message (or channel) is gone.
    NotFound,
    /// Missing permissions in the channel.
    Forbidden,
    Other(String),
}

impl std::fmt::Display for ChatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChatError::NotFound => write!(f, "message or channel not found"),
            ChatError::Forbidden => write!(f, "missing permissions"),
            ChatError::Other(s) => write!(f, "{}", s),
        }
    }
}

impl std::error::Error for ChatError {}

/// What the broadcast engine needs from a chat service (Discord in production, a recorder in tests).
#[async_trait]
pub trait Chat: Send + Sync {
    async fn send(&self, channel: u64, embed: Embed, comps: Option<Components>) -> Result<u64, ChatError>;
    /// `comps: Some(empty)` removes the interactive parts; `None` leaves them as they are.
    async fn edit(&self, channel: u64, message: u64, embed: Embed, comps: Option<Components>) -> Result<(), ChatError>;
    /// Remove a message. `NotFound` means it is already gone, which callers treat as success.
    async fn delete(&self, channel: u64, message: u64) -> Result<(), ChatError>;
    #[allow(dead_code)]
    async fn react(&self, channel: u64, message: u64, emoji: &str) -> Result<(), ChatError>;
}
