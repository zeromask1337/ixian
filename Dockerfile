FROM oven/bun:1-debian

# yt-dlp from PyPI (freshest at build time) + ffmpeg for muxing.
# yt-dlp-ejs ships the YouTube challenge solver locally; no runtime GitHub fetch.
# Force IPv4: some container networks resolve IPv6 first but cannot route it.
RUN echo 'Acquire::ForceIPv4 "true";' > /etc/apt/apt.conf.d/99force-ipv4 \
  && apt-get update \
  && apt-get install -y --no-install-recommends ffmpeg python3 python3-venv ca-certificates \
  && rm -rf /var/lib/apt/lists/* \
  && python3 -m venv /opt/yt-dlp \
  && /opt/yt-dlp/bin/pip install --no-cache-dir --upgrade yt-dlp yt-dlp-ejs

# Use the bundled Bun as yt-dlp's JS runtime (YouTube EJS extraction)
RUN mkdir -p /home/bun/.config/yt-dlp \
  && printf -- '--js-runtimes bun\n' > /home/bun/.config/yt-dlp/config \
  && chown -R bun:bun /home/bun/.config

ENV YTDLP_PATH=/opt/yt-dlp/bin/yt-dlp \
    YTDLP_PLUGIN_DIR=/app/yt-dlp-plugins \
    HOST=0.0.0.0 \
    PORT=3000

WORKDIR /app
COPY --chown=bun:bun package.json ./
COPY --chown=bun:bun src ./src
COPY --chown=bun:bun yt-dlp-plugins ./yt-dlp-plugins
RUN find /app/yt-dlp-plugins -name __pycache__ -type d -prune -exec rm -rf {} +
USER bun

EXPOSE 3000
CMD ["bun", "src/index.ts"]
