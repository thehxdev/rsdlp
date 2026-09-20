# rsdlp

A tiny prototype of an HTTP API around [yt-dlp](https://github.com/yt-dlp/yt-dlp), written in Rust (I will regret using Rust for this).

**This is a prototype.** I just wanted to play with the idea of wrapping yt-dlp in a small service.

## Current structure

1. You POST a form with a `url` field to `/download`.
2. The server spawns a `yt-dlp` child process with `-o -` (write to stdout) and `-S res:720` (prefer the best format at or below 720p).
3. yt-dlp's stdout is streamed straight into the HTTP response as `application/octet-stream` through a pipe. This eliminates temp filesa and media file buffering in memory.

If the client disconnects, the server kills the whole yt-dlp process group so nothing keeps downloading in the background. This uses process-group signaling via `libc`, so cancellation is Unix-only for now; Which is fine actually. I expect a user of this project to run it on a Linux/BSD server.

## Requirements

- Rust and the cargo toolchain
- `yt-dlp` executable installed and available on your `PATH`

## Run

```sh
cargo run
```
The server listens on `0.0.0.0:3000`.

## Send a request

```sh
curl -X POST -d 'url=https://example.com/some/video' \
  -o video.mp4 http://127.0.0.1:3000/download
```

## Current Limitations

- No URL validation. A bad URL makes yt-dlp fail and panics the whole server.
- No way to pick format/quality per request. `res:720` is hardcoded.
- No auth and no rate limiting.

## License

[MIT](LICENSE)
