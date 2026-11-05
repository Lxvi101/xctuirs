# xctuirs

A Rust terminal companion for Xcode projects, based on [libec/xctui](https://github.com/libec/xctui). Build, test, install, and run an Apple app without opening the Xcode IDE.

The application is implemented in Rust. It invokes Apple's `xcodebuild`, `simctl`, `devicectl`, and logging tools; **Xcode must still be installed** for SDKs, compilers, simulators, and signing. This is a development runner for existing Xcode projects, not a replacement compiler or project generator.

## Install

Requires macOS, Xcode, and a current Rust toolchain (edition 2024).

```sh
cargo install --path .
```

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

Select a scheme in pane 2 and a destination in pane 3 with Enter or Space. Destinations include My Mac, paired physical devices, and available simulators. Physical devices appear in cyan. Press `r` to build, install, launch, and attach the app console.

| Key | Action |
| --- | --- |
| `b`, `r`, `t`, `c`, `p` | Build, run, test, clean, resolve packages |
| `x` | Stop current operation or attached app |
| `0`–`4`, Tab / Shift-Tab | Focus detail / status / schemes / destinations / runs |
| `j` / `k`, arrows | Move selection or scroll log |
| Mouse wheel / trackpad scroll | Scroll the pane under the pointer |
| Enter / Space | Select scheme or destination |
| `[` / `]` | Switch scheme or platform tab |
| `/`, Enter, Esc | Filter, keep filter, clear filter |
| `g` / `G`, Page Up / Down | Top / tail, page log |
| Ctrl-U / Ctrl-D | Scroll half a page |
| `i`, `s` | Issues only, console timestamps |
| `+` / `_` | Expand / restore left panes |
| `e` | Open visible log in `$VISUAL`, `$EDITOR`, or `vi` |
| `R` | Rename selected simulator |
| `D` | Delete history with confirmation |
| `?`, `q` | Help, quit |

Scrolling up in a log pauses automatic following; scroll back to the bottom or press `G` to resume. Wheel scrolling moves a list cursor without selecting a different scheme or destination. Mouse capture is released while the editor is open and when the app exits.

A run has a build row and a separate console row. App stdout/stderr and mirrored OS logs appear in the console; a best effort unified log stream supplies app debug messages for simulator and Mac runs. Physical-device console uses `devicectl`.

Selections and up to 50 runs per project are saved under `~/Library/Application Support/xctui/`. Quit cancels the active process, drains its final output, and saves the stopped run. Tool processes are isolated from terminal Ctrl-C; cancellation escalates to SIGKILL after five seconds.

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

## Resource safeguards

Following the November 1 resource-exhaustion incident, builds use `xcodebuild -jobs 1`. A per-container advisory lock prevents two xctuirs instances from running the same project's workflow at once. Builds require 20 GiB free at startup and are stopped if free space drops below 10 GiB while xcodebuild runs. Discovery commands time out after 15 seconds instead of hanging shutdown. The TUI redraws on changes rather than repeatedly rewrapping idle logs.

Cancellation first sends the tool's graceful signal, then kills its isolated process group after five seconds if descendants remain. This handles ordinary child compilers even if their parent already exited. Apple's build service can create compiler jobs outside that process group; the runner cannot prove those jobs stopped, and does not kill unrelated system processes. These safeguards also cannot guarantee that a single compiler will fit in available RAM, or prevent other applications from exhausting resources. Build configuration is still controlled by your existing scheme/project settings; no Debug/Release override is added.
