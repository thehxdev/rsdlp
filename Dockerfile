FROM rust:alpine AS builder
WORKDIR /app
RUN apk add --no-cache musl-dev
COPY . .
RUN cargo build --release

FROM alpine:latest
RUN apk add --no-cache \
    ca-certificates \
    curl \
    ffmpeg \
    deno \
    python3 \
    && curl -L https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp_musllinux -o /usr/local/bin/yt-dlp \
    && chmod a+rx /usr/local/bin/yt-dlp

WORKDIR /app
COPY --from=builder /app/target/release/rsdlp /usr/local/bin/rsdlp

RUN mkdir -p /data/downloads
ENV PATH="/usr/local/bin:$PATH"
ENV RSDLP_DATA_DIR="/data"
# VOLUME ["/data"]

EXPOSE 3000
# HEALTHCHECK --interval=30s --timeout=3s --retries=3 \
#     CMD curl -f http://localhost:3000/ || exit 1

ENTRYPOINT ["rsdlp"]
