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
The server listens on `0.0.0.0:3000`. Then open `http://127.0.0.1:3000` in your browser.

## Telegram User Bot

`rsdlp` can optionally run as a Telegram user bot (logging in as a regular Telegram user account via MTProto, allowing file uploads up to 2GB).

### Setup

1. Get `api_id` and `api_hash` from [my.telegram.org](https://my.telegram.org).
2. Set environment variables:
   ```sh
   export TG_API_ID="1234567"
   export TG_API_HASH="0123456789abcdef0123456789abcdef"
   export TG_SESSION_FILE="rsdlp.session" # optional, defaults to rsdlp.session
   ```
3. Run one-time interactive login to authenticate your session:
   ```sh
   cargo run -- --tg-login
   ```
   Enter your phone number, the verification code received on Telegram, and 2FA password (if enabled). The session is saved to `rsdlp.session`.
4. Start `rsdlp`:
   ```sh
   cargo run
   ```
   Both the Axum HTTP server and the Telegram user bot run concurrently. If Telegram environment variables are not set, `rsdlp` runs in web-only mode.

### Usage

- Send any media URL to the user bot account in a private chat or in **Saved Messages**.
- The bot fetches available formats and replies with clickable quality options (`/dl_1080`, `/dl_720`, `/dl_audio_320`, `/dl_best`).
- Tap an option to download and stream the media file directly in Telegram.
- Send `/cancel` to abort a pending selection.


## Current Limitations

- No URL validation. A bad URL makes yt-dlp fail and panics the whole server.
- No auth and no rate limiting.

## License

[MIT](LICENSE)
