# rsdlp

A tiny prototype of an HTTP API around [yt-dlp](https://github.com/yt-dlp/yt-dlp), written in Rust (I will regret using Rust for this).

**This is a prototype.** I just wanted to play with the idea of wrapping yt-dlp in a small service.

## Current structure

1. POST a form with a `url` field to `/qualities` to list the resolutions a media offers. The server runs `yt-dlp -J`, reads its `formats` list and answers with JSON: `video` holds resolutions in px (the smallest dimension, i.e. exactly what `-S res` selects) and `audio` holds average bitrates in kbps (for `-S abr`) — video and audio describe their "resolution" in different units, so they arrive as two lists. Values are rounded up to well-known steps, so only standard options are offered: 144p/240p/360p/480p/720p/1080p/… for video, 128/192/320 kbps for audio. The Web UI's "Fetch qualities" button calls this endpoint.
2. POST a form with a `url` field and at least one quality format (`res` in px and/or `abr` in kbps) to `/download` to stream the download. These become `-S res:<res>,abr:<abr>`. The format selector `-f b/bv*+ba` is fixed and identical for every request.
3. The server spawns a `yt-dlp` child process with `-o -` (write to stdout).
4. yt-dlp's stdout is streamed straight into the HTTP response as `application/octet-stream` through a pipe. This eliminates temp filesa and media file buffering in memory.

If the client disconnects, the server kills the whole yt-dlp process group so nothing keeps downloading in the background. This uses process-group signaling via `libc`, so cancellation is Unix-only for now; Which is fine actually. I expect a user of this project to run it on a Linux/BSD server.

## Requirements

- Rust and the cargo toolchain
- `yt-dlp`, `ffmpeg` and NodeJS runtime executables installed and available on your `PATH`

## Run

```sh
cargo run
```
The server listens on `0.0.0.0:3000` (configurable via `RSDLP_BIND_ADDRESS`). Then open `http://127.0.0.1:3000` in your browser.

Environment variables can be stored in a `.env` file (copy from `.env.example`).

### Logging & Observability

`rsdlp` uses `tracing` and `tracing-subscriber`. Log levels (`trace`, `debug`, `info`, `warn`, `error`) can be configured via:
- Command-line flag: `cargo run -- --log-level debug` (or `--log debug`, `-l debug`)
- Environment variable: `export RSDLP_LOG=debug` (or `RUST_LOG=debug`)

**Priority**: The environment variable (`RSDLP_LOG` / `RUST_LOG`) takes precedence over the command-line flag if both are specified. Defaults to `info`.

## Web Admin Panel & Telegram Authentication

`rsdlp` includes a password-authenticated Web Admin Panel at `/admin`.

- **Authentication**: Default password is `admin`. Password can be changed from the panel and is stored as a bcrypt hash in SQLite (`rsdlp.db`).
- **Telegram Bot Integration**: Configure API ID, API Hash, and Bot Token directly through the browser. Bot token login operates via MTProto (`client.bot_sign_in`) without requiring phone numbers, SMS codes, or 2FA credentials.
- **Modular Backend**: If Telegram is unconfigured or stopped, the web server (`/`, `/qualities`, `/download`, `/admin`) operates in standalone mode without errors.
- **Storage & Disk Safety**: Media files staged for Telegram uploads reside in `${RSDLP_DATA_DIR}/downloads`. The panel displays partition disk metrics (used/free/total) and includes a manual trigger to prune stale temp files. Unhandled crashes are automatically cleaned up on next boot.

## Docker Deployment

`rsdlp` is fully unattended and automatic in Docker:

```sh
docker run -d \
  -p 3000:3000 \
  -v rsdlp_data:/data \
  rsdlp
```

- Both `rsdlp.db`, `rsdlp.session`, and temporary download staging reside concisely in `/data`.
- No environment variables are mandatory at startup; all configuration and Telegram bot setup are handled via `http://<host>:3000/admin`.

## Telegram Bot

`rsdlp` can optionally run as a Telegram MTProto bot (with MTProto allowing file uploads up to 2GB).

### Setup

1. Open `http://<host>:3000/admin` in your browser.
2. Log in with password `admin` (change it in the panel).
3. Under **Telegram Bot**, enter your `api_id` and `api_hash` (from [my.telegram.org](https://my.telegram.org)) and `bot_token` (from [@BotFather](https://t.me/BotFather)).
4. Click **Save & Start Bot**. The bot signs in and starts immediately.

*(Environment variables `RSDLP_TG_API_ID`, `RSDLP_TG_API_HASH`, and `RSDLP_TG_BOT_TOKEN` remain supported as optional fallbacks).*

### Usage

- Send any media URL to the bot in a private chat (or in **Saved Messages** if running on a user account).
- The bot fetches available formats and replies with interactive inline buttons (`1080p`, `720p`, `🎵 320k`, `⚡ Best`, `❌ Cancel`) as well as clickable fallback text commands.
- Tap any button to download and upload the media file directly in Telegram.
- Both download and upload progress messages display an interactive **❌ Cancel** button.
- Tap **❌ Cancel** (or send `/cancel`) at any stage to immediately abort running downloads, kill background processes (SIGTERM to the process group), abort uploads, and wipe all temporary files.
- All temporary files are wrapped with an RAII cleanup guard (`TempFileGuard`) guaranteeing files are immediately deleted from disk when upload finishes, when an error occurs, or upon cancellation.

### Streaming Architecture Note

Telegram MTProto uploads (`upload.saveBigFilePart`) require knowing the exact file size and part count in advance to validate parts and construct `InputFileBig`. Because `yt-dlp` dynamically remuxes audio and video streams via `ffmpeg` to stdout with variable bitrate and muxing overhead, stream byte sizes cannot be determined upfront. `rsdlp` stages the media locally in temp storage, uploads via MTProto, and relies on `TempFileGuard` to guarantee zero residual disk usage across all termination paths.


## Current Limitations
 
- No URL validation. A bad URL makes yt-dlp fail and panics the whole server.
- No rate limiting.

## License

[MIT](LICENSE)
