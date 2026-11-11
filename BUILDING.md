# Building The Forge GTV from the source

You do not need this to *use* the bot: download the installer from the Releases page. This is for compiling it yourself, or for
making your own release from a fork.

## Option 1: let GitHub compile it (no tools needed on your computer)

1. Fork the repository (or use your own copy) and open its **Actions** tab. If GitHub shows *I understand my workflows, go ahead and
   enable them*, press it.
2. Choose the workflow **release** → **Run workflow**.
3. Type a tag such as `v3.8.0` to build **and publish a Release**, or leave the box empty to only build.
4. Wait for the two builds (Linux and Windows, roughly 10–20 minutes the first time). Download the files from the **Releases** page,
   or, for an empty-tag run, from **Artifacts** at the bottom of the finished run.

Pushing a tag that starts with `v` (`git tag v3.8.0 && git push origin v3.8.0`) does the same thing. The Linux file is built on
Ubuntu 22.04 (needs glibc 2.35 or newer). **The Windows installer has not been tested on a real Windows machine.**

## Option 2: compile on your own computer

Both systems need **Rust** (stable) from <https://rustup.rs> and **git**.

* **Windows:** also install the **Visual Studio Build Tools** with the *Desktop development with C++* workload (this gives Rust its
  linker and C compiler). You do **not** need NASM or CMake.
* **Linux:** a C compiler and linker (`sudo apt install build-essential` on Debian/Ubuntu).

```
git clone https://github.com/<you>/forge-gtv.git
cd forge-gtv
cargo build --release        # the program: target/release/forge-gtv  (forge-gtv.exe on Windows)
cargo test --release         # optional: runs the tests
```

On Windows you can instead run `build-windows.bat` (or `build-windows.ps1`) in the folder: it builds, runs the tests (a failing test is
only a warning) and leaves the installer in `dist\`.

The first build downloads and compiles the dependencies and takes several minutes. The file you get is the bot itself; run
`forge-gtv --help` for its commands. `tools/release.sh` (Linux only) makes the wider-compatible installer that is attached to the
project's own releases.

## Making a new version

Change `version` in `Cargo.toml`, add the entry to `CHANGELOG.md`, commit and push, then run the **release** workflow (Option 1).

## If a Windows build fails

Copy the first red `error` line and the few lines under it and send them to whoever maintains the project. Two known causes:
a missing *Desktop development with C++* workload (error mentions `link.exe` or `cl.exe`), and an old Rust (`rustup update`).
