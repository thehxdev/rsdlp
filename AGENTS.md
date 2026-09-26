# AGENTS.md

Guidance for AI agents (and humans) working on this repo.

## What this project is

`rsdlp` is a small prototype HTTP API around [`yt-dlp`](https://github.com/yt-dlp/yt-dlp), written in Rust. It is intentionally minimal — the README's "I will regret using Rust for this" energy is accurate. Prefer the smallest change that works over abstractions, config systems, or new dependencies.

## General rules

- Never build in release mode for development. Always build in debug mode.
- Always cleanup test temporary files that you create after you're done.

## Stack and layout

- Rust, edition 2024 (toolchain in use: cargo/rustc 1.98).
- Web framework: `axum` 0.8 on `tokio`, streaming bodies via `tokio-util`.
- `libc` is used only for Unix process-group cancellation.

| Path | Role |
| --- | --- |
| `src/main.rs` | Router, `GET /`, `POST /download` handler, response headers. |
| `src/ytdlp.rs` | `Ytdlp` struct: spawns `yt-dlp`, exposes its stdout as an `AsyncRead`. |
| `index.html` | The single-page UI, embedded at compile time with `include_bytes!`. |
| `send-test-request.sh` | curl smoke test against a running server. |
| `README.md` | Current behavior, requirements, and known limitations. Keep in sync. |

There is no `tests/` directory, no CI, and no `rustfmt`/`clippy` config in the repo.

## How it works

1. `POST /download` with a form field `url` (`application/x-www-form-urlencoded`).
2. `Ytdlp::get_info()` runs `yt-dlp -J <url>` and parses JSON to pick a filename for `Content-Disposition`.
3. `Ytdlp::start_download()` spawns `yt-dlp -o - <url>` with a piped stdout, in its own process group.
4. That stdout is wrapped in `tokio_util::io::ReaderStream` and streamed straight to the HTTP response as `application/octet-stream`.

Shared yt-dlp flags live in `COMMON_FLAGS` in `src/ytdlp.rs` (`-f b/bv*+ba`, `-S res:720`, retries, etc.). The format/quality selection is hardcoded there; change it in one place, not per call site.

### Cancellation contract

If the client disconnects, the response body is dropped, which drops `Ytdlp`, whose `Drop` impl fires a `watch` channel. The spawned supervisor task then sends `SIGTERM` to the child's **process group** (`libc::getpgid` + `libc::kill`), so `ffmpeg`/downloader children die too. This is deliberately Unix-only — do not "fix" it with a cross-platform crate without reading the note in `start_download()`.

## Build, run, verify

```sh
cargo run                          # serves 0.0.0.0:3000
cargo build --release              # release profile: LTO, panic = "abort", stripped
./send-test-request.sh "<URL>"     # server must already be running; writes output.mp4
```

Runtime dependencies must be on `PATH`: `yt-dlp`, `ffmpeg`, and a Node.js runtime (per README).

There is no automated test suite. Before claiming something works:

1. `cargo check` (or `cargo build`) must pass.
2. Start the server and exercise `POST /download` with `send-test-request.sh` and a real media URL, plus `GET /` for UI changes.
3. To test cancellation, start a download and abort the curl mid-stream, then confirm no orphaned `yt-dlp`/`ffmpeg` processes remain (`pgrep -af 'yt-dlp|ffmpeg'`).

## Conventions and gotchas

- **Two-process design is intentional**: one short-lived `yt-dlp -J` for metadata, then one streaming `yt-dlp -o -`. Do not merge them into a single invocation without a good reason.
- **Errors**: handlers log with `eprintln!` and map to `StatusCode`. Metadata/parse failures that imply a bad URL become `400`; spawn/build failures become `500`.
- **Known gaps** (documented in README, not yet bugs to paper over silently): no URL validation (a bad URL makes `yt-dlp` fail), no per-request format/quality, no auth, no rate limiting. If you touch one of these, update the README's Limitations section.
- **Don't reintroduce removed complexity**: history includes commits that removed unnecessary code and dependencies. Keep dependency count flat unless the task requires it.
- `index.html` is embedded into the binary — there is no template engine and no separate asset pipeline.
- `.gitignore` covers `/target` and `.pi/`. Leave `.pi/` alone; it is agent tooling state, not project source.
- Bind address and port (`0.0.0.0:3000`) are hardcoded in `main()`; a config/env override is a reasonable feature request, but don't add a config crate for it.
- Keep `README.md` accurate when behavior changes; it is the project's source of truth for outsiders.

## Scope reminder

This is a prototype. Bias toward readable, short code that a reviewer can hold in their head. If a change needs a new crate, a module tree, or a build step, justify it explicitly in the PR/commit message.
