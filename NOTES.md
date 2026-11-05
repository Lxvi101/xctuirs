# Implementation and validation

Implemented from the upstream xctui behavior specification at libec/xctui commit `782f65b6483386bc91bd75b119caafb2dc0f4ba0` (2026-11-01 checkout). The retained `spec/` folder contains the original text, fixtures, and black-box checker. The application and its process/UI/storage layers are Rust; Python and Ruby are used only by tests to simulate Apple tools or run the upstream checker.

## What works

- Workspace/project discovery, shared and user schemes, local Swift package products.
- My Mac, paired physical devices, and available simulators; platform tabs and remembered UDID selection.
- Real build, test, clean, resolve, and build/install/launch workflows.
- Device signing uses existing project settings. Device install and console use `devicectl`; simulator runs use `simctl`; Mac runs execute the app directly.
- Merged stdout/stderr, bounded channels, 4 MiB line limit, console timestamps, mirrored OS logs, best effort debug log stream.
- Isolated process groups, correct tool stop signals, five-second escalation, drain-and-wait shutdown.
- Five-pane terminal UI, filters, issues, wrapped log following and scrolling, two phases per run, editor, rename, history deletion, help.
- Atomic preferences, project-keyed history, metadata-first lazy log loading, 50-run retention, corrupt-history recovery.

## Verification

The tests read upstream YAML fixtures rather than replacing them with a new set. Some Rust test functions run an entire fixture group; Rust's test-function count differs from the fixture count.

- Xcode parsing/discovery/settings: 56 upstream cases.
- Command construction and actual runtime tool calls: all 23 upstream cases (8 also checked independently by the Xcode test).
- Streaming: all 7 upstream cases, plus extra merged-pipe and cancellation checks.
- Log cleaning: all 9 upstream cases.
- Log processing and summaries: all 43 upstream cases.
- CLI: upstream black-box Ruby checker, 12/12 cases.
- TUI: all 56 non-screen upstream cases, covering loading, keys, rename, job results, filtering, in-memory history, wrapped log scrolling and half-page movement.
- Persistence: upstream preference/history storage expectations and TUI deletion behavior, plus roundtrip, metadata, lazy loading, missing logs, corruption, retention, and isolation across projects.
- Five extra isolated runtime workflows exercise physical-device argv/environment, cancellation during install, install failure, app exit status, Mac executable launch, and unified-log cleanup. Every recorded tool PID must be gone after each workflow.
- PTY smoke: five panes appear; build starts a fake tool; quitting terminates it, saves a stopped run, and restores the terminal. No tmux or Go reference binary was available for the optional live reference comparison.
- Live read-only check: Xcode 26.3 detected; the runner listed this Mac's paired iPhone/iPad and available simulators.

No real application was built, installed, or launched on a device: this initially empty workspace contained no Xcode app project. Workflow tests use isolated fake tools and temporary HOME directories. The live device discovery command was read-only.

## Screen compatibility

Screen fixture results are recorded after the final comparison below. The implementation uses ANSI terminal colors and reverse-video selection. A debugger, previews, Instruments, scheme editing, signing/account setup, and Xcode project generation are outside this runner's scope. Keep Xcode installed for Apple's SDKs and tools.

All 17 upstream screen cases are exercised. **15 match byte for byte**. Two upstream captures (`build-failed.txt` and `run-attached-build-row.txt`) truncate long detail text where SPEC 12.4 requires wrapping. This implementation preserves the full wrapped lines. Those two outputs are checked against reviewed Rust snapshots under `tests/snapshots/`; tests also verify unchanged left panes and that the only right-pane differences are the documented continuation lines. The original upstream snapshots are unchanged. This is not a claim of 17/17 byte-exact upstream parity or an unqualified 226/226 conformance score.

Final checks: `cargo test --locked -- --nocapture`, `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings`, and `ruby spec/conformance/check.rb target/debug/xctuirs cli` all succeeded. The black-box CLI checker reported `cli: 12/12`. The optimized release is installed using `cargo install --path . --locked`.

Mouse scrolling follow-up: wheel/trackpad events target the pane under the pointer, including expanded panes. Scrolling lists moves the cursor without committing a different scheme/destination. Log scrolling pauses follow until the bottom is reached (or `G` is pressed). Dialogs and active filter input ignore wheel events. Mouse capture is released for the editor and on exit. Three focused regression tests and a PTY smoke covering SGR wheel input and terminal/mouse restoration pass; the existing library fixture suite and Clippy remain clean.

Recovery hardening: implemented per-project/container flock shared by CLI/TUI; `xcodebuild -jobs 1`; 20 GiB start / 10 GiB ongoing free-space checks; parent-first graceful cancellation followed by isolated process-group cleanup even when the parent already exited; 15-second bounded discovery capture; and dirty-driven rendering to avoid continuously rewrapping idle logs. New tests cover two CLI instances contending for one project, lock release, descendant compilers retaining or closing output pipes, and discovery timing out while a descendant holds a pipe. Original upstream action argv expectations are supplemented explicitly with the intentional `-jobs 1` safety addition. Existing scheme configuration is preserved. Apple build-service-managed processes outside the owned group remain a limitation; no global process-name kill is used. No real Apple project builds were run during recovery validation.

## November 5 redesign

The five-pane upstream layout was replaced by one screen: header chips (scheme, device), a status area with a progress bar, and the log, with one primary action in the footer. Because of this, the upstream screen fixtures (`spec/screens/`) and the 56 TUI key-sequence cases no longer apply. They remain in `spec/` for reference; new TUI tests in `src/tui.rs` cover the redesigned flows. CLI behavior and output are unchanged (`cli: 12/12`).

Big-build failures found in the saved run history, and their fixes:
- Free-space gates of 20 GiB (start) and 10 GiB (during a build) blocked or killed builds on a machine with 13 GiB free. They are now 3 GiB to start, a warning below 10 GiB, and a stop below 1 GiB.
- `-jobs 1` serialized every compile. It is now memory-bounded (about one job per 3 GiB of RAM; `XCTUIRS_JOBS` overrides).
- The container was discovered once at launch. After the workspace was deleted, every action failed with "does not exist". The project is now revalidated every 3 seconds while idle, before each action, and after such failures.
- Every frame cloned and re-wrapped the whole log, and recounted issues with a fresh HashSet, which slowed the UI and backed up the output pipeline on large logs. Rendering now walks only the visible lines, and issues are indexed incrementally.
- Device builds lacked `-allowProvisioningUpdates`, which Xcode effectively uses. xcodebuild output was block-buffered, so it now runs with `NSUnbufferedIO=YES`.
