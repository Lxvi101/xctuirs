# Contributing

Thanks for helping. xctuirs is small on purpose: one screen, one primary action, and a runner that doesn't fall over on big builds.

## Setup

You need macOS, Xcode and Rust 1.88+. Ruby (preinstalled on macOS) runs the upstream conformance checker.

```sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
cargo build && ruby spec/conformance/check.rb target/debug/xctuirs cli
```

The tests don't need a real Xcode project or device. `tests/runtime_workflow.rs` drives fake `xcodebuild`, `xcrun simctl` and `devicectl` scripts, and the TUI tests render into a test backend.

## Where things live

| File | What it does |
| --- | --- |
| `src/xcode.rs` | Project, scheme, destination and build-settings discovery |
| `src/runtime.rs` | Running xcodebuild, installing, launching, cancellation, the build lock |
| `src/progress.rs` | Progress bar and ETA from xcodebuild's output |
| `src/diagnose.rs` | Picks the error to show first and suggests a fix |
| `src/job.rs`, `src/store.rs` | Runs, logs, history and preferences on disk |
| `src/tui.rs` | The screen |
| `spec/` | Upstream xctui specification and fixtures (kept unchanged) |

## Pull requests

- Keep changes focused, and add a test for any bug you fix. A fake-tool scenario in `tests/runtime_workflow.rs` is usually the easiest way to reproduce a build or launch problem.
- CI runs fmt, clippy, tests and the conformance checker on macOS. Please run them locally first.
- New error hints go in the `HINTS` table in `src/diagnose.rs`, with a test using a real (anonymized) log line.
- When sharing logs in issues, remove signing identities, team IDs, device names and anything else you consider private.

## Reporting bugs

Open an issue with your macOS and Xcode versions, what you pressed, and the relevant part of the log. For security problems, see [SECURITY.md](SECURITY.md).
