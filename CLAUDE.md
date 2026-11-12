# CLAUDE.md

handys-fisch (repo `handys-fisch`): a Windows-only app that reads Roblox process memory and sends input.

## SDK

The shared library comes from the `handys-sdk` repo (`handy-sdk` in `Cargo.toml`, a git dependency on `main`; the repo is private, git's credential manager authenticates). When this folder sits in the shared development folder, the root `.cargo/config.toml` patches it to `../sdk` and builds into the shared `../target`. A change that needs a new SDK feature lands in `handys-sdk` first. Nothing game-specific goes in the SDK.

## Build and test

```
cargo build
cargo clippy --all-targets -- -D warnings
cargo test
cargo fmt --check
```

Tests named `live_*` need a running Roblox and the network: mark them `#[ignore]` and never run them unasked.

`build.cmd [test]` release-builds the app, copies `handys-fisch.exe` into this folder (from `target\release\`, else `..\target\release\`) and runs `cargo test` when `test` is given. It closes the app if it is running (never force-killed: still open after 200 s stops the build and leaves it running) and starts it again through Explorer afterwards, even when the build fails. So it restarts the user's app mid-game: if the app is open, ask before building, or use plain `cargo build` / `cargo test`, which leave release exes alone.

## Safety (never skip)

- Never launch `handys-fisch.exe` while it could act in the user's game, unless the user says so.
- Never send input to Roblox in a public match.
- Never change input timing the app relies on (hold lengths, gaps, sending only while Roblox is foreground) and never leave keys held after a crash, close or toggle-off.
- Low-level hook callbacks stay O(1) and non-blocking (atomics only).

## Conventions

Each module is `src/<folder>/<file>.rs`, with its tests in a `#[cfg(test)] mod tests` at the bottom of the same file. Comments explain why, not what.

- **Names:** snake_case functions, `UPPER_CASE` consts.
- **Addresses** are `usize`; 0 means none. Read helpers return 0 or `None` on failure, so chains need no checks in between.
- **Memory:** functions take `m: &dyn Memory, o: &Offsets`. Typed reads are `m.get::<T>(addr)` (T: `Pod`) and `m.read_slice`. Structs read from Roblox are `#[repr(C)]` + `unsafe impl Pod`. Speed is the number of `Memory::read` calls (each is one ReadProcessMemory round trip); the `read_cost` tests pin them, so never let one rise unnoticed.
- **Offsets:** `o["Class.Field"]` and `o.at(base, "Class.Field")` panic on a missing key (a bug in an app's required list). Every key an app reads must be in its `required_keys()`. Code that must survive missing keys checks `o.has_all(&KEYS)` first; never `catch_unwind`.
- **Errors:** fallible functions return `Result<T, String>`. Bugs are panics; `panic = "abort"` is set, so a panic ends the process after the crash guard has released held keys and written `crash.log`.
- **Strings:** `&str` / `String` everywhere, UTF-16 only at the Win32 call (`util::text::{wide, wide_path, from_wide}`). Paths are `&Path` / `PathBuf`.
- **Win32:** the `windows` crate only (add features to `Cargo.toml` as needed). Wrap handles so they close on drop. Keep `unsafe` blocks small.
- **JSON:** `serde_json`.
- **Threads:** a stop flag is `stop: &AtomicBool`; long-lived threads are `util::thread::StopThread`. State shared across threads lives behind `Arc`; callbacks are `Box<dyn Fn(..) + Send + Sync>`.
- **Time:** `std::time::{Duration, Instant}`.
- **Tests:** `crate::testing::FakeMemory` is the byte-map `Memory`; inject `HttpGet` so tests never hit the network; each test uses its own temp dir (tests run in parallel).
- No new dependencies without a reason, no abstractions without two callers, no dead code.

## Releases and updates

`release.cmd <version>` (MAJOR.MINOR.PATCH) publishes a release: it refuses unless this tree is clean (and `..\sdk`, when present, is clean and equal to `origin/main`) and the tag is free, sets the version in `Cargo.toml`, builds with `HANDY_RELEASE=1` and tests, copies `handys-fisch.exe` plus its `.sha256` into `release\`, commits `Release v<version>`, tags and pushes, then runs `gh release create`. Only builds made by it self-update at startup (`update_on_startup` in `main.rs`, skipped by `--status` and while another copy runs); dev builds and `build.cmd` never do. Push the SDK first: the release is built against the local SDK. It publishes a public release: ask the user before running it. It never closes or starts the app.
