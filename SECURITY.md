# Security policy

## Supported versions

Only the latest release of The Forge GTV receives fixes.

## Reporting a vulnerability

Please **do not** open a public issue for a security problem. Use GitHub's **private vulnerability reporting**
(*Security → Report a vulnerability* on this repository) or contact the owner, **[TFG]Aciddrop**, privately.
Include the version (`forge-gtv version`), what you did, and what happened. You will get an answer as soon as the maintainers can.

## Scope notes

* The admin panel is designed for `127.0.0.1` only. Exposing it to a network without TLS is a configuration choice, not a vulnerability.
* The Windows build is untested; please report Windows-specific problems as normal issues unless they expose secrets.
* Never post a Discord token, an RCON password or your `config.json` in an issue. If you did, reset the token in the Discord
  Developer Portal and change the password.

## Hardening checklist for people who run it

* Run the bot as an **ordinary user**, never as root or Administrator. The installer's per-user service does this.
* Keep the admin panel on `127.0.0.1` (the default). If you need it from elsewhere, use a VPN or an SSH tunnel, or put an HTTPS
  reverse proxy in front of it, and use a password of 12+ characters. Anyone who can sign in to the panel can choose which programs
  the video starts, so treat the panel password like the machine's password.
* Give the video player an access key (the panel makes one when you switch the player on) and open only the ports you use.
* Choose few people for `/gtv rcon`: everyone chosen can kick and ban.
* Keep the bot up to date: dependency advisories are checked on every push and weekly (`cargo audit`, exceptions with reasons
  in `.cargo/audit.toml`).
