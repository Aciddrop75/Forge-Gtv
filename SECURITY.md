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
