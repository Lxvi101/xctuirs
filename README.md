# xctuirs

[![CI](https://github.com/Lxvi101/xctuirs/actions/workflows/ci.yml/badge.svg)](https://github.com/Lxvi101/xctuirs/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

A Rust terminal companion for Xcode projects, based on [libec/xctui](https://github.com/libec/xctui). Build, test, install, and run an Apple app without opening the Xcode IDE.

The application is implemented in Rust. It invokes Apple's `xcodebuild`, `simctl`, `devicectl`, and logging tools; **Xcode must still be installed** for SDKs, compilers, simulators, and signing. This is a development runner for existing Xcode projects, not a replacement compiler or project generator.

## Install

Requires macOS, Xcode, and Rust 1.88 or newer.

```sh
cargo install --git https://github.com/Lxvi101/xctuirs --locked
```

Or from a checkout: `cargo install --path . --locked`.

Make sure `~/.cargo/bin` is on `PATH`. If the selected developer directory points to Command Line Tools rather than Xcode, select your Xcode installation:

```sh
sudo xcode-select --switch /Applications/Xcode.app/Contents/Developer
```

Use an existing project with its signing settings configured. Physical devices must be paired, trusted, unlocked, and have Developer Mode enabled. xctuirs uses your project's existing signing configuration.

## Use

```sh
cd /path/to/your/app
xctuirs
```

It prefers a workspace to a project, searches the current directory and one directory down, and discovers schemes from disk before falling back to `xcodebuild -list`. Local Swift package products are shown separately.

One screen:

```text
 Puzzle   s  ◆ Puzzle    d  ▯ My iPhone

 ⠹ Compiling  BoardView.swift · Puzzle                      1:48 · ~1:05 left
 ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━──────────────────  62%
 18 of 27 targets · 412 tasks · 3 warnings
 ─ Build  Console ───────────────────────────────────────────────────────────
 …live log…
 ▶ r  Run on My iPhone    b build · t test · c clean · h history · ? keys
```

Press `r` to build, install, launch and attach the console. The header chips are the controls: `s` picks the scheme, `d` the device or simulator (type to filter; physical devices first). If something is missing when you press `r`, the right picker opens.

The progress bar follows xcodebuild's target graph and task stream. After one successful build of a scheme on a destination, it also uses that build's task count and duration, so later builds show an ETA. When a build fails, the first error appears above the log with its file and line, plus a likely fix for common problems (type-checker timeouts, signing, missing pods, locked devices, unshared schemes). The log switches to problems only, errors first; `i` shows the full log. Long builds post a macOS notification when they finish while the terminal is in the background.

| Key | Action |
| --- | --- |
| `r` | Run: build, install, launch, attach the console |
| `b`, `t`, `c`, `p` | Build, test, clean, resolve packages |
| `x` | Stop |
| `s`, `d` | Pick scheme, pick device or simulator (ctrl-r renames a simulator) |
| `h` | History; Enter opens a past run, `D` deletes all |
| Tab | Build log ⇄ app console |
| `i` | Problems only ⇄ full log |
| `/` | Filter the log (Enter keeps, Esc clears) |
| ↑ ↓, PgUp/PgDn, Ctrl-U/D, mouse wheel | Scroll (pauses following) |
| `g` / `G` | Top / follow the end |
| `e` | Open the visible log in `$VISUAL`, `$EDITOR`, or `vi` |
| Esc | Clear the filter, or return to the latest run |
| `?`, `q` | Keys, quit |

xctuirs follows the project: if the `.xcworkspace` is deleted or a new one appears, it switches containers and reloads schemes.

Selections, up to 50 runs per project, and build estimates are saved under `~/Library/Application Support/xctui/`. Quitting cancels the active process, drains its final output, and saves the stopped run.

## CLI

```sh
xctuirs project
xctuirs schemes
xctuirs destinations
xctuirs settings MyScheme 'iPhone 17 Pro'
xctuirs build MyScheme
xctuirs test MyScheme
xctuirs clean MyScheme
xctuirs resolve
xctuirs run MyScheme 'iPhone 17 Pro'
xctuirs run MyScheme DEVICE_UDID
xctuirs run MyMacScheme mac
```

An omitted destination uses the first booted iOS simulator, otherwise the first iPhone simulator, otherwise My Mac. An explicit destination matches its exact name or UDID. A lone scheme name opens the TUI with that scheme selected. `info` is a compatibility alias for project and scheme information. CLI commands do not read preferences or save history. Build failures preserve the tool exit code; interruption returns 130.

## Development

```sh
cargo test
cargo clippy --all-targets -- -D warnings
ruby spec/conformance/check.rb target/debug/xctuirs cli
cargo build --release
```

The upstream specification and fixtures are retained in `spec/`. Tests exercise the original parsing, command, log, processing and streaming fixtures, plus CLI conformance and process lifecycle tests using fake tools. See [NOTES.md](NOTES.md) for validation coverage and differences from upstream. No Go runtime, Node runtime, or Xcode plugin is required.

## Reliability and resources

- **Parallelism is bounded by memory**: `-jobs` is about one compiler job per 3 GiB of RAM, at least 2 and at most the CPU count. On a 16 GB machine that is 5. Set `XCTUIRS_JOBS=N` to override it, or `XCTUIRS_JOBS=0` to let xcodebuild decide.
- **Disk**: a build refuses to start below 3 GiB free and warns below 10 GiB. It stops only when free space falls below 1 GiB.
- **Signing**: device builds pass `-allowProvisioningUpdates`, so they create or refresh profiles like Xcode does, using the accounts already added to Xcode.
- **Streaming**: xcodebuild runs with `NSUnbufferedIO=YES` and `-hideShellScriptEnvironment`. Output arrives line by line, without the environment dump for every script phase. Install output is streamed too.
- **Large logs**: the screen only wraps the visible lines, and errors and warnings are indexed as they arrive. Logs of hundreds of thousands of lines stay responsive.
- A per-project lock prevents two xctuirs instances from building the same project at once. Cancellation sends the tool's graceful signal, then kills its process group after five seconds.

## Contributing

Issues and pull requests are welcome. See [CONTRIBUTING.md](CONTRIBUTING.md). Bug reports with the failing part of a build log (`e` opens it in your editor) are the most useful kind.

## Acknowledgements

xctuirs is a Rust reimplementation of [xctui](https://github.com/libec/xctui) by Libor Huspenina. The behavior specification, fixtures and conformance checker in `spec/` come from that project under the MIT license.

xctuirs is not affiliated with or endorsed by Apple. Xcode, iOS and macOS are trademarks of Apple Inc.

## License

[MIT](LICENSE)
