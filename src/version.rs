// The Forge GTV - Copyright (C) 2026 [TFG]Aciddrop
// SPDX-License-Identifier: GPL-3.0-or-later
// This program comes with ABSOLUTELY NO WARRANTY; see the LICENSE file.

//! Single source of truth for version and ownership.

pub const APP_NAME: &str = "The Forge GTV";
pub const OWNER: &str = "[TFG]Aciddrop";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const RELEASE_DATE: &str = "2026-11-10";

/// The credit line carried by every post the bot makes (Discord footers, the web player). Clan branding changes how the
/// bot looks, never this line: it is part of the licence terms of the project.
pub fn credit() -> String {
    format!("{} v{} · owner {}", APP_NAME, VERSION, OWNER)
}

pub fn banner() -> String {
    format!("{} v{} (released {}, Rust) - owner: {}", APP_NAME, VERSION, RELEASE_DATE, OWNER)
}

pub const COPYRIGHT: &str = "Copyright (C) 2026 [TFG]Aciddrop";
pub const LICENSE_ID: &str = "GPL-3.0-or-later";
/// The full text of the GNU General Public License v3 (also in the LICENSE file of the source).
pub const LICENSE_TEXT: &str = include_str!("../LICENSE");

/// One line, for banners.
pub fn short_notice() -> String {
    format!("{} - free software ({}), NO WARRANTY (run `forge-gtv license`)", COPYRIGHT, LICENSE_ID)
}

/// The notice printed by `forge-gtv license`.
pub fn notice() -> String {
    format!(
"{name} - {copyright}

This program is free software: you can redistribute it and/or modify it under the terms of the
GNU General Public License as published by the Free Software Foundation, either version 3 of the
License, or (at your option) any later version.

This program is distributed in the hope that it will be useful, but WITHOUT ANY WARRANTY; without
even the implied warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the GNU General
Public License for more details. You should have received a copy of it along with this program (the
LICENSE file, or `forge-gtv license --full`); if not, see <https://www.gnu.org/licenses/>.

Not affiliated: {name} is an independent community project. It is not made, endorsed or supported by
Frozen Sand, the Urban Terror developers, or Discord Inc. \"Urban Terror\" and \"Discord\" are trademarks
of their owners and are used here only to say what this program works with. This program does not include
or distribute the Urban Terror game or its files; those have their own licence.

The source code and the third-party notices (hls.js, Rust libraries) are in the source archive.",
        name = APP_NAME, copyright = COPYRIGHT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_and_licence_notice() {
        assert_eq!(APP_NAME, "The Forge GTV");
        assert_eq!(LICENSE_ID, "GPL-3.0-or-later");
        assert!(banner().starts_with("The Forge GTV v"));
        let n = notice();
        assert!(n.contains("GNU General Public License") && n.contains("WITHOUT ANY WARRANTY") && n.contains("Not affiliated"));
        assert!(n.contains("Frozen Sand") && n.contains("Discord Inc."));
        assert!(short_notice().contains("NO WARRANTY") && short_notice().contains(LICENSE_ID));
        assert!(LICENSE_TEXT.contains("GNU GENERAL PUBLIC LICENSE") && LICENSE_TEXT.contains("Version 3, 29 June 2007"));
    }
}
