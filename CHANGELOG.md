# Changelog

## 0.1.0

First public release.

- Rust reimplementation of xctui: build, test, clean, resolve packages, and run on a simulator, a physical device or the Mac.
- One-screen TUI with a live progress bar, an ETA after the first build, and a failure card showing the first error with a likely fix.
- Reliable large builds: memory-bounded `-jobs`, relaxed disk thresholds, unbuffered streaming, an incremental issue index, and rendering only the visible lines.
- Confirmed launches, so a refused launch is reported as a failure instead of an app that exited.
- Workspace-scoped schemes, the runnable target taken from the scheme's Run action, and parallel destination discovery with timeouts.
