# Changelog

## 3.8.0 — 2026-11-10
- **Choose the people who may use `/gtv rcon`.** Besides the admin role, individual Discord people can now be selected (*Settings → Rcon
  commands from Discord → People*). Someone may use the commands when they are on that list **or** hold a chosen role; the list and the
  role are both optional, and one person alone is enough to switch the command on. Add a person by their **Discord user id** (the panel
  looks the name up and shows it, so a wrong number is noticed) or **search by name** (this needs the bot's *Server Members Intent*
  switched on in the Discord Developer Portal; without it the panel says so, and adding by id still works). The list can only be
  changed in the password-protected panel, never from Discord. New config field `discord.rcon_users` (user ids); the configuration check
  names an entry that is not a number, and a server with rcon switched on but nobody chosen (no role, no person).
- Tests: 198 pass on Linux (new: people-only access, people plus role, nobody else, non-numbers never match, the panel saves the list and
  refuses typos in words, the lookups explain a missing bot). The new section was driven in headless Chromium (typo refused, id added,
  name search explained without a bot, remove, save, survives a reload). **Not tried:** the name lookup and name search against real
  Discord (user lookup by id, member search, the Server Members Intent), and `/gtv rcon` itself on a real Discord server.

## 3.7.0 — 2026-11-09
- **Choose which game server an rcon command goes to.** With two or more servers that have rcon switched on (switch ticked **and**
  password entered), `/gtv rcon cyclemap | slap | kick | bigtext | ban` now starts with a **required `server` drop-down** that lists
  exactly those servers (name and display name); the command cannot be sent without choosing one. With a single server there is
  nothing to choose. Beyond Discord's 25-choice limit the field falls back to a typed one with suggestions. The player suggestions
  follow the server you picked. The drop-down is rebuilt in Discord by itself when you switch a server on or off, or rename it.
  Not tried on a real Discord server.
- Tests: 196 pass on Linux (new: the drop-down is first and required with 2+ servers, absent with one, typed beyond 25, lists only
  servers that can take rcon).

## 3.6.1 — 2026-11-08
- **The admin commands are now `/gtv rcon <action>`:** type `/gtv`, choose **rcon**, then `cyclemap`, `slap`, `kick`, `bigtext` or `ban`
  (for example `/gtv rcon slap player:Bob`). It is a group inside `/gtv`; the separate `/rcon` command is gone (the bot replaces its
  command list on start, so the old one disappears by itself). Nothing else about it changed: same five actions, same role-only access,
  same safeguards. **While it is on, `/gtv` is no longer hidden from people without Manage Server** (otherwise the admin role could not
  see it); every other `/gtv` sub-command still refuses them when used. You can hide `/gtv` again from anyone in *Server Settings →
  Integrations → this bot*. Not tried on a real Discord server.
- **Make a release from the source on GitHub, by anyone:** the *release* workflow can now be started from the Actions tab (*Run workflow*,
  with a tag such as `v3.6.1`) and one final job creates the Release from the Linux and Windows builds. New `BUILDING.md` explains it
  and compiling on your own computer. `build-windows.ps1` also builds before testing now.
- **The release workflow builds the installer before running the tests**, and a failing Windows test no longer stops the Windows `.exe` from
  being uploaded (Linux tests still must pass).
- **Windows build no longer needs NASM.** The SSH/SFTP library (russh) used the `aws-lc` crypto backend, which on Windows requires
  the NASM assembler to be installed. It now uses the `ring` backend that the rest of the bot already used, so a plain Visual Studio
  C++ toolchain is enough. Changes the crypto library behind SFTP/SSH log reading: **not re-tested against a real SFTP/SSH server.**
- **Windows build fix (found by the GitHub Windows check):** the video code's wait for the Linux-only Xvfb socket, and some Linux-only
  tests, were not fenced off with `cfg(unix)`, so the Windows build and its tests did not compile (`cannot find unix in os`). They are
  fenced now. The same Windows run showed 8 tests that assumed Linux (systemd, X lock files, `kill`, case-sensitive paths, several
  videos at once); they now only run on Linux or check the Windows behaviour. Still not run on a real Windows machine.
- Tests: 194 pass on Linux.

## 3.6.0 — 2026-11-07
- **Admin commands from Discord (rcon): `/rcon cyclemap | slap | kick | bigtext | ban`.** Exactly these five, for members of **one
  Discord role** you choose, and **off by default**. It is only available when a server has its **rcon password entered** and the
  per-server switch ticked (*Servers → edit → More options*; greyed out until the password is there) **and** a role is chosen
  (*Settings → Rcon commands from Discord*, with a *Load roles from Discord* picker). The `/rcon` command is added to Discord when
  that is true and removed when it stops being true, without a restart. A ban requires a reason.
- **Safeguards.** Only the chosen role (Manage Server is not enough; the guild's `@everyone` role is refused even if chosen; not
  in direct messages). A player is typed as a name or number but the bot resolves it against the server's own `status` list and
  sends **only the client number**; names that fit several players are refused. Reasons and messages are cut to 80 characters and
  lose `; " \ % $ ^ ~` and non-printable text, so they cannot close the quotes or add a command. Answers are private to the person
  who ran the command; the bot's log records who ran what (never the player's address or the password); one command per person
  every 2 seconds.
- **A ban is by address** (that is how Urban Terror bans: `addip`), followed by a kick that shows the reason. It is refused for a
  bot, a player with no address, the server's own address, and an address shared by several players (kick instead). The answer
  tells you the `removeip <address>` line that undoes it.
- Reads the server's reply in **all** its packets (a full `status` is longer than one UDP packet), which the old single-packet read
  would have cut.
- Config: `servers.<key>.rcon_commands` (default off) and `discord.rcon_roles` (role ids). The configuration check names a switch
  without a password, a password without a role, and a role id that is not a number. The rcon password stays hidden in the panel.
- Tests: 193 pass (new: the `status` parser, player lookup, hostile text, every command's exact text, every ban refusal, the role
  check, when `/rcon` exists, the command's shape, a stand-in game server over real UDP, the panel's switch/role/password). The three
  key safeguards were broken on purpose and each failed a test. **Not tested:** the `/rcon` command on a real Discord server (roles,
  autocomplete, private answers), a real Urban Terror server executing `slap`, `kick`, `addip`, `bigtext` (the command words follow
  the source of a client written for UrT 4.3.4), anything on Windows.
## 3.5.1 — 2026-11-06
- **Fix: "Disallowed gateway intents were provided" (the bot stayed offline).** When any server has *Relay Discord chat into the
  game* on (with an rcon password), the bot asks Discord for the privileged *Message Content Intent*; if that switch is off in the
  Developer Portal, Discord refused the whole connection. Now the bot connects again **without** the relay by itself, so the
  scoreboards, feeds, summaries and slash commands work, and the panel (Dashboard and Setup tabs, and the Discord pill in the
  header) says in plain words what to switch on and that the relay is off. The reason a connection failed is now shown in the
  panel too (it was only in the log).
- **Browse for the map pictures.** *Servers → edit → More options → Map pictures folder* has a **Browse…** button: it lists the
  folders of the machine the bot runs on, starts in the server's `q3ut4` folder when the video is set up, and says whether the
  folder you are in holds `.pk3` archives, picture files or a `levelshots` folder.
- **Added after the first 3.5.1 build (no change to the program):** `GITHUB-GUIDE.md`, a walkthrough for publishing the project on
  GitHub (Linux and Windows commands, a fresh-history route, secret checks, the first release); and the release workflow now asks for
  write permission (`permissions: contents: write`), because a repository under a personal account gives workflows a read-only token
  and the step that attaches the installers to a Release would have failed with 403. The CI workflow is explicitly read-only.
- Tests: 180 pass (new: recognising and explaining the refused intent). **Not tested:** a real Discord connection with the intent
  switched off (the log line and Discord's behaviour come from your log; the fallback itself needs a live token), the new
  Browse… button on a real machine with Urban Terror installed, anything on Windows.

## 3.5.0 — 2026-11-05
- **The scoreboard shows the level picture of the map being played.** It changes with every map, sits under the team tables
  and is cut to **one fixed square — 320 × 320 pixels by default, whatever size the original has** — so every broadcast takes the
  same room and a channel with several broadcasts stays tidy. `crop` (default) fills the square; `pad` keeps the whole picture
  with dark bars. A map with no picture shows none; the logo is unchanged.
- **Where the pictures come from:** a folder on the machine the bot runs on — loose `<map>.jpg` files (also `.jpeg`, `.png`,
  `.tga`) or the maps' `.pk3` archives. By default the game folder of the server's video client; per server,
  `servers.<key>.levelshot_dir` (*Servers → edit → More options → Map pictures folder*) overrides it. Pictures are decoded
  with hard limits (4096 px, 8 MB), re-encoded as JPEG and cached in `data/levelshots/`; the first sight of a map prepares it in
  the background, so the picture appears a few seconds after the map starts.
- **Settings:** new `levelshots` section (`enabled`, `size` 128–640, `fit` crop|pad), on the *Branding* tab as *Map pictures*,
  with a **Find the picture** test (server + map name → the picture at its final size, the folders searched, the address Discord
  will use). Applies without a restart.
- **Needs `web.public_url`:** Discord downloads the picture from `<public_url>/levelshot/<file>`. Only the files the bot made are
  served, never the game folder. The configuration check names a missing public address (only when pictures could be found), a
  size outside 128–640 and an unknown `fit`.
- New libraries: `image` (JPEG/PNG/TGA decoding, resizing) and `zip` (reading `.pk3`), both permissive licences; the third-party
  notices list them and their dependencies.
- Tests: 179 pass (new: names, exact square output for many sizes and both fits, hostile files refused, loose files and
  archives in any letter case, cache and retry, the scoreboard picture at one fixed size, public serving, the panel test box,
  the config checks). The layout of four broadcasts in one channel was drawn from the bot's real output into a Discord-style
  page with **placeholder pictures**. **Not tested:** live Discord (how the picture really looks and whether Discord scales it
  down), real Urban Terror `.pk3` files (only synthetic archives), anything on Windows.

## 3.4.0 — 2026-11-02
- **Broadcast several game servers at the same time.** More than one server can be broadcast at once without stopping any
  other. A single channel can now carry several servers (it used to refuse a second `/gtv start` until the first was
  stopped): each gets its own scoreboard, feed, summary and Watch button, and the feed/summary titles name the server. A
  server can still be shown in several channels (its video is shared).
- **A web port for each server.** New `servers.<key>.spectator.web_port` (panel: *Player port for this server*): the server's
  video is served on a port of its own, opened when its video starts and closed when it stops, and showing only that server's
  stream (the page, status and files of other servers are refused there). The Watch link points at it by itself;
  `spectator.public_url` overrides the address (reverse proxy). `0` (default) = the shared player, so existing configs are
  unchanged. A taken port stops the video before anything is started and the message names it.
- **Discord:** `/gtv stop`, `delay`, `stream` and `cam` take an optional `server` (autocompletes to what runs in the channel;
  not needed while only one is running; asked for when it would be a guess). `/gtv stop all:True` stops every broadcast in the
  channel. New `/gtv list` shows every broadcast in every channel with its video link and viewers. The player-card menu knows
  which server it belongs to (menus posted by older versions keep working while only one server runs in the channel). Chat relay
  goes to every server in the channel that relays.
- **Admin panel:** the Dashboard lists every running broadcast with its player port and a Stop button per server; the Servers
  table has a *Player port* column; the server editor has the two new fields.
- **Configuration check** names: a player port used twice or equal to the web/admin port, a port without a web section, two servers
  publishing to the same `publish_to`/`rtmp_url`, two videos with sound on the same `pulse_source` — and, on Windows, that several
  videos cannot run at the same time (one desktop). Windows: several simultaneous videos are **not supported**; Linux only.
- Tests: 164 pass, including real TCP sockets for the per-server ports and the full start/stop path with two videos using
  stand-in client/ffmpeg programs; the panel fields were saved through headless Chromium. **Not tested:** live Discord, two real
  UrT clients and real ffmpeg streaming together (CPU load, the clients' local game ports), real PulseAudio sinks, anything on Windows.

## 3.3.1 — 2026-11-02
- **The installer offers to install the spectator-video tools.** On Linux: Xvfb (the virtual screen that makes the game client
  run headless, with no monitor), xdotool, ffmpeg and the client's libraries (software OpenGL, SDL 1.2, OpenAL) through
  apt-get, dnf, yum, pacman or zypper; `sudo` asks for the password itself, `--yes` installs only when running as root or with
  password-less sudo, and a failure never fails the install (the commands are printed instead). On Windows nothing but ffmpeg is
  needed (the client runs on the desktop, captured by gdigrab; no Xvfb/xdotool): offered through `winget` (Gyan.FFmpeg) in the
  "when I log on" mode. New: `forge-gtv install-video-deps` (any time), `install --no-video-deps`. Fedora/RHEL and openSUSE
  cannot get an H.264-capable ffmpeg from their stock repositories, so the installer says to enable RPM Fusion / Packman.
- **Installer no longer prints the browser's own warnings** (snap Firefox's `IBUS-WARNING`, AppArmor and dconf lines appeared in
  the terminal after the panel was opened); both install guides now explain that such lines are harmless.
- Tests: 148 pass. Verified for real on Ubuntu 24.04 (apt): the packages resolve and install, a second run changes nothing.
  Fedora/Arch/openSUSE package names and the Windows `winget` step were **not** run.

## 3.3.0 — 2026-11-02
- **A new Discord post for every map.** When the map changes, the old scoreboard is closed (title "🏁 FINISHED", its last
  standings, buttons removed) and the old live feed is kept under a "finished" title; a new scoreboard, feed and **Watch**
  button are posted at the bottom of the channel, so the live post is always the newest. The "Match finished" summary stays.
  Options: `discord.new_post_each_map` (default **on**; off = one post per broadcast as before) and `discord.delete_old_posts`
  (delete the previous map's posts instead; needs Manage Messages). If the new post cannot be made, the old ones keep working.
  A map change right after the bot starts reuses the first posts.
- **Viewer count.** The web player reports in with a random per-tab id every 2.5 s (`/status/<key>?v=…&p=…`); the bot keeps
  them in memory for 12 s (no addresses, nothing on disk). Shown as "👁 N watching the stream" on the scoreboard and the
  video page, on the panel's Dashboard (playing / waiting), and as "Peak viewers" in the match summary and archive JSON.
- **Clan branding.** New `branding` settings and panel tab: clan name, logo, icon, accent colour, with a live preview.
  Name + icon appear above each post's title, the logo in its corner, the colour on the feed/summaries/player cards (the live
  scoreboard keeps red/blue for the leading team); the video page gets the name, logo and tab icon. Pictures (PNG, JPEG,
  GIF, WebP, 2 MB, type checked by content, SVG refused) are uploaded in the panel, stored in `data/brand/` under a content
  hash and served publicly at `/brand/<name>` so Discord can fetch them (needs a reachable `web.public_url`), or you paste an
  https address. Changes apply live, no restart. **The credit footer "The Forge GTV v… · owner [TFG]Aciddrop" is added to every
  post and the video page by code and no setting changes or removes it** (covered by tests).
- **Admin panel: this computer only, or remote.** Default is unchanged (127.0.0.1). *Settings → Who can open this panel* (and
  `forge-gtv admin-access show|local|remote [--allow ADDR]… [--password P]`) switches it to listen on all addresses. Safeguards:
  the password is asked again and a **new 12+ character password** is required; optional `admin.allowed_ips` (addresses and
  CIDR ranges, this computer always allowed, a list that would lock you out is refused); every request is checked against the
  current settings, so switching back to local cuts other computers off at once; while remote, passwords must be 12+
  characters; the generic settings save / Python import cannot change `admin.host` or `admin.allowed_ips`; the session cookie
  is `Secure` behind an HTTPS proxy (`X-Forwarded-Proto`); a warning is logged at start-up. Plain HTTP: use a VPN, SSH tunnel
  or HTTPS proxy.
- Docs: README (new sections), both install guides and the ports diagram updated (no more "never open 8787" — now "never
  forward 8787 on your router"). New tests: 27 (111 → 138 pass).
- Upgrading: nothing to do; old configs load unchanged. Note the default is now one post per map; set
  `discord.new_post_each_map` to `false` to keep the old behaviour.

## 3.2.0 — 2026-11-02
- **Renamed: the project is now "The Forge GTV"** (it was "UrT GTV for Discord"). The program, service, folder and files are
  `forge-gtv` (installer `forge-gtv-setup-<version>-<os>`, Windows task "The Forge GTV", panel, installer, web player, Discord
  footers and all documentation report the new name). The `/gtv` Discord command is unchanged. The config file format did not
  change: to move from an old install, uninstall the old one (`urt-gtv uninstall`, keep settings), install this, and copy
  `shared/config.json` across. `URT_GTV_CONFIG` / `URT_GTV_SERVICE` are still read; new installs write `FORGE_GTV_*`.
  Earlier entries below keep the old name on purpose.
- **Open source:** released under the **GNU General Public License v3.0 or later** (`LICENSE`, SPDX headers in every source file,
  `THIRD-PARTY-NOTICES.md`, `Cargo.toml` license). New `forge-gtv license [--full]` prints the notice / full text; the installer, the
  admin panel (login page and a Settings → *About* card), the web player pages and `forge-gtv version` show the licence and
  no-warranty notice. Disclaimers added: no warranty, not affiliated with Frozen Sand / Urban Terror / Discord Inc., Urban Terror
  not included, players' information, Windows untested.
- **GitHub-ready:** `CONTRIBUTING.md`, `SECURITY.md`, issue and pull-request templates, a CI workflow (Linux; Windows as an
  informational job), `.gitattributes`, `.gitignore` (`dist/` is no longer tracked), release workflow uploads the licence files.

## 3.1.9 — 2026-11-02
- **Game chat is no longer broadcast.** (1) Player chat lines in `games.log` (`say`, `sayteam`, `tell`) no longer become
  entries in the Discord live feed. (2) The hidden spectator client now hides chat on screen (new setting
  `spectator.hide_chat`, default on; panel checkbox *Hide the in-game chat on the video*): its boot config sets
  `cg_chatHeight 0`, `cg_teamChatHeight 0`, `cg_chatTime 0`, `cg_teamChatTime 0` and `con_notifytime 0`, so neither public
  nor team chat nor console notices appear in the video. The opt-in Discord→game relay (`relay_chat`, off by default) is
  unchanged, but what it says in the game is not shown in the video either.

## 3.1.8 — 2026-11-02
- **Documentation:** `INSTALL-LINUX.md` and `INSTALL-WINDOWS.md` rewritten as picture walkthroughs (`docs/img/`): a 7-step
  roadmap, an account chooser (user vs `sudo` / administrator), real installer and panel screenshots, a port map and a video
  pipeline diagram. `tools/guides_html.py` builds single-file HTML copies with the images embedded. The two Windows
  Command Prompt pictures are labelled illustrations (the Windows build is still untested).
- **Escape is now sent 60 seconds after joining and after each map load** (new setting `spectator.escape_delay`, default
  60; panel field *Send Escape after (seconds)*). The other spectate keys (`4`) still go right after the game has settled.
  Escape is pressed once per join / map load, only while the client is still on the server, and a newer map load restarts
  the minute. `escape_delay: 0` keeps the 3.1.7 behaviour (Escape first, with the other keys).

## 3.1.7 — 2026-11-02
- **The bot now presses Escape to clear the join-team menu**, then `4` (spectate): `spectator.spectate_key` is a list of
  keys pressed in order, one second apart, once the client has joined and after every map load; the default is
  `Escape 4`. Retries (when the game log still shows a team) press only the non-Escape keys, because Escape toggles the
  in-game menu. For a server that shows no team menu set the keys to just `4`. Windows sends `{ESC}`. The panel field
  *Spectate keys* explains this; settings saved by 3.1.6 keep their `4` until you change them.

## 3.1.6 — 2026-11-02
- **The bot presses `4` in the game once it has joined the server and after every map load**, so the client picks
  "spectate" in the team menu. Map loads are detected from the game log (`InitGame`) and from a change of map in the status;
  the two reports of one load are merged. If the game log shows the client still on a team the key is pressed again every
  5 s (max 5 times per moment). New setting `spectator.spectate_key` (default `4`, any key name such as `F5` or
  `KP_4`, empty = off); panel field *Spectate key*. It works together with `team s` from 3.1.5.

## 3.1.5 — 2026-11-02
- **The spectator client now stays a spectator and never picks a team.** Once it is on the server the bot sends `team s`
  to it; if the game log shows it on red/blue/free (e.g. the server auto-joins players with `g_teamAutoJoin 1`) it is
  moved back every few seconds until the server agrees. Without a `games.log` the command is sent once per client
  start. Setting `spectator.force_spectator` (panel: *Always stay a spectator*), on by default.

## 3.1.4 — 2026-11-01
- **Fix: the web player page opened but never showed the video.** The access cookie was set without `Path=/`, so
  browsers sent it only to `/watch/...` and never to the video files (`/hls/...`), which answered 403. The cookie now
  covers the whole site. (Found by driving a real browser against the real Xvfb -> ffmpeg -> web pipeline.)
- The player now says what is happening instead of a black box: "video source is starting", "building the Ns
  anti-ghosting delay - about N seconds left", "source stopped updating", "access key missing - open the Watch link
  again" (new `/status/<key>`, key required). It keeps retrying while the delay builds up and shows the player's own
  error detail under the video for support.
- The player library (hls.js 1.5.17, Apache-2.0, `assets/hls.js-LICENSE.txt`) is now served by the bot itself instead of
  cdn.jsdelivr.net, so viewers on networks that block public CDNs get a working player.

## 3.1.3 — 2026-11-01
- **Browse… buttons** for the games.log path (this machine or over SFTP), the UrT client program and the UrT folder:
  click through folders instead of typing paths. Programs-only / `.log`-only filters, a name filter, drive list on
  Windows, and a "did you mean …?" button when a typed folder is misspelled (`GamesServer` → `GameServer`). Choosing the
  client program fills in the UrT folder. The picker lists names only (new `POST /api/browse`, login required).
- Mistyped paths now explain themselves: the log tail warning, `check-servers` and the video check add
  "Did you mean …?" when a close existing path is found.
- Fix: a mistyped public address such as `https//example.com/:8080` produced a broken Discord Watch button
  (Discord refused every message edit). The address is now repaired when it is used and when it is saved
  (`https//host/:8080` → `https://host:8080`), and `check-config` / the video check say what was wrong.
- A missing Xvfb / client / ffmpeg program is now named in the log with what to do about it, instead of
  "No such file or directory".

## 3.1.2 — 2026-11-01
- Fix: "the X display is not reachable" / client keeps restarting. A leftover `/tmp/.X<n>-lock` or socket owned by
  another user (typically from an earlier run with `sudo`) could not be removed by the bot's account, so Xvfb refused
  to start while the stale socket file made the bot think the display was up. The bot now skips display numbers that
  are busy or blocked, uses the next free one, and only treats the display as ready when it accepts connections.
- When the client or ffmpeg cannot reach the display, the log now also shows Xvfb's own last lines (or says Xvfb
  wrote no log / is not installed).

## 3.1.1 — 2026-11-01
- New step-by-step guides: `INSTALL-LINUX.md` and `INSTALL-WINDOWS.md` (user vs `sudo` / administrator, services, video,
  firewall, upgrade/rollback/uninstall, troubleshooting).
- Saving the configuration keeps the file's owner, so a `sudo urt-gtv reset-admin-password` can no longer leave a
  root-owned `config.json` the service user cannot read.
- Windows: `shared\` is restricted with `icacls` (SYSTEM, administrators, installing user); the logon task is created
  with `/IT`; the installer defaults to "when I log on" for non-administrators and explains a start-up task needs an
  administrator. (Windows paths remain untested.)

## 3.1.0 — 2026-11-01
- **Video on the CPU or the GPU.** New per-server `encoder` setting: `cpu` (default), `auto` (best GPU that passes
  a test, else CPU), or `nvenc` / `vaapi` / `qsv` / `amf`. Pick it under *Servers → Spectator video → Video
  processing*.
- **GPU detection that proves it works.** *Detect GPU & test encoders* (panel), `urt-gtv check-gpu` (command line)
  and the Video check look for hardware, then run a short real test encode with the CPU and every GPU encoder
  in your ffmpeg, report speed, explain failures in words (driver missing, no permission on /dev/dri, ...) and
  recommend one that actually produced video. `auto` runs it once at stream start and remembers the result.
- Only the encoding moves to the GPU; the game client still renders in software inside Xvfb (the report says so).

## 3.0.4 — 2026-11-01
- Video check log view: blank lines are dropped, repeated lines are folded ("(x2)"), and the harmless Xvfb
  `libEGL warning ... /dev/dri/card0: Permission denied` is labelled as harmless.

## 3.0.3 — 2026-11-01
- Web player: opening the bare address (e.g. `http://host:8080/`) used to give a bare 404. It now goes straight to the
  running stream (or lists several), says "No live broadcast" with what to do when none is running, and asks for
  the Discord link when the player is private. Other unknown pages and `/watch/<key>` with no stream show a readable
  page instead of an error line. (Video is only served while a broadcast was started with `stream:True`.)

## 3.0.2 — 2026-11-01
- Fix a false alarm in the Video check: Xvfb's harmless `libEGL warning: failed to open /dev/dri/card0:
  Permission denied` (no GPU on a headless box; it falls back to software rendering) was reported as
  "stream/ not writable". GPU/Mesa probing noise is now ignored; real permission errors are still reported,
  with the offending line quoted.

## 3.0.1 — 2026-11-01
- **Clean uninstall:** `urt-gtv uninstall` now asks "keep my settings" or "clean uninstall"; `--clean` removes the
  service, program, settings, data, logs and the install folder (typed confirmation; `--yes` for scripts;
  `--backups` also deletes `.bak-*` folders). Shows exactly what it will delete first, stops a bot that was
  started by hand, and refuses to touch any folder that is not an UrT GTV install.
- Add Server: the log file can be any path (local or over SSH). Pick a saved SSH login or create one right in
  the form, then **Test SSH login + log file**: it separates "cannot log in" from "logged in but the file is
  missing", and offers the host-key approval step inline.
- Fix: a system service installed with sudo could not write its own `shared/` files (config, logs, stream/),
  causing "permission denied" / "not writable". The installer now hands `shared/` to the service user.
- Video check: new **write access** test that really tries to write in the stream folder and names the folder,
  its owner and the exact `chown` command; the hint now quotes the log line that triggered it.

## 3.0.0 — 2026-11-01 (Rust edition)
- Complete rewrite in Rust: one self-contained program, no Python or pip needed.
- New admin panel (localhost, password-protected): Discord setup wizard, any number of game servers,
  SFTP connections with host-key approval, per-server tests, video pipeline check, logs, import from the
  Python config, restart.
- Self-installer: `urt-gtv install` (Linux systemd system/user service; Windows Task Scheduler),
  versioned `releases/`, upgrade keeps config and data, clean reinstall with backup, rollback, uninstall.
- Video check warns when `web.public_url` is a private/LAN address.
- Windows: installer, service registration and `gdigrab` video are written but untested.
- Python edition (v2.2.x) is unchanged and still supported.
