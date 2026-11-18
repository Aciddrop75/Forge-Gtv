# Contributing to The Forge GTV

Thank you for helping! The Forge GTV is free software under the **GNU General Public License v3.0 or later**
(`GPL-3.0-or-later`). By sending a contribution (pull request, patch, issue text that you want included) you agree that it is
licensed under the same terms, and that you have the right to give it to the project.

## Before you start

* **Bugs and ideas:** open an issue first for anything bigger than a small fix. Use the issue templates; for bugs include the
  output of `forge-gtv check-config`, `forge-gtv check-servers` and `forge-gtv logs -n 100` (remove your Discord token and
  passwords — the bot hides them in the panel, but check pasted text).
* **Security problems:** do **not** open a public issue; see [`SECURITY.md`](SECURITY.md).
* **Windows:** the Windows build has never been run by the original author. Windows test reports and fixes are especially welcome.

## Build and test

```bash
cargo test                       # unit + integration tests (needs no network, no Discord, no game)
cargo build --release            # target/release/forge-gtv
tools/release.sh                 # tests + portable Linux build + dist/ (needs cargo-zigbuild + zig for the glibc 2.17 build)
```

Windows: `build-windows.bat` (see `INSTALL-WINDOWS.md`, Part 12).

Some tests start real helper programs (Xvfb, ffmpeg, an `sshd`) and skip themselves when those are missing. Tests that leave
temporary folders must clean them up (`/tmp/gtv-*`).

## Pull requests

1. Keep a change focused; add or update a test for behaviour you change.
2. `cargo fmt` is not enforced, but match the surrounding style. No new warnings from `cargo build`.
3. New source files start with the licence header used in the other files:
   ```
   // The Forge GTV - Copyright (C) <year> <your name or handle>
   // SPDX-License-Identifier: GPL-3.0-or-later
   // This program comes with ABSOLUTELY NO WARRANTY; see the LICENSE file.
   ```
4. Update `CHANGELOG.md` (user-visible changes) and the install guides if the steps or screens change.
5. New dependencies must be under a licence compatible with GPL-3.0-or-later (MIT, Apache-2.0, BSD, ISC, MPL-2.0 …); list
   them in `THIRD-PARTY-NOTICES.md`.
6. Do not add Urban Terror game files, Discord logos or other material you do not have the right to redistribute.

## Behaviour that must not regress

* Game chat is never posted to Discord and is hidden on the spectator video (unless the operator changes the setting).
* The admin panel listens on `127.0.0.1` only, and every state-changing request needs the `X-Requested-With` header.
* The installer never deletes a folder that is not an install of this program, and never runs the service as root unless the
  user asked for that.
