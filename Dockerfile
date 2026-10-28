FROM rust:1.99.1-slim-bookworm AS builder
WORKDIR /app

# Build dependencies in their own layer so source edits stay cheap.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src \
  && echo 'fn main() {}' > src/main.rs \
  && cargo build --release --locked --bin ixian \
  && rm -rf src

COPY src ./src
RUN touch src/main.rs && cargo build --release --locked --bin ixian

FROM node:24-bookworm-slim

# yt-dlp from PyPI (freshest at build time) + ffmpeg for muxing.
# yt-dlp-ejs ships the YouTube challenge solver locally; no runtime GitHub fetch.
# Force IPv4: some container networks resolve IPv6 first but cannot route it.
RUN echo 'Acquire::ForceIPv4 "true";' > /etc/apt/apt.conf.d/99force-ipv4 \
  && apt-get update \
  && apt-get install -y --no-install-recommends ffmpeg python3 python3-venv ca-certificates \
  && rm -rf /var/lib/apt/lists/* \
  && python3 -m venv /opt/yt-dlp \
  && /opt/yt-dlp/bin/pip install --no-cache-dir --upgrade yt-dlp yt-dlp-ejs

# Node is yt-dlp's JS runtime (YouTube EJS extraction)
RUN mkdir -p /home/node/.config/yt-dlp \
  && printf -- '--js-runtimes node\n' > /home/node/.config/yt-dlp/config \
  && chown -R node:node /home/node/.config

ENV YTDLP_PATH=/opt/yt-dlp/bin/yt-dlp \
    YTDLP_PLUGIN_DIR=/app/yt-dlp-plugins \
    HOST=0.0.0.0 \
    PORT=3000

WORKDIR /app
COPY --chown=node:node yt-dlp-plugins ./yt-dlp-plugins
RUN find /app/yt-dlp-plugins -name __pycache__ -type d -prune -exec rm -rf {} +
COPY --from=builder /app/target/release/ixian /usr/local/bin/ixian
USER node

EXPOSE 3000
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
  CMD node -e "fetch('http://127.0.0.1:'+(process.env.PORT||3000)+'/').then(r=>process.exit(r.ok?0:1)).catch(()=>process.exit(1))"
CMD ["ixian"]
