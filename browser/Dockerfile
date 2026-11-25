FROM debian:bookworm-slim AS download
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl
RUN set -eu; \
    target_arch="$(dpkg --print-architecture)"; \
    case "$target_arch" in \
        amd64) arch=x86_64; sha=11b53d27cd743e00e196e216920e7f3f794cf378e3f1792b8d25d3beabaf9cad ;; \
        arm64) arch=aarch64; sha=4ee0aeeb38fa8bb2cb004ba104abe1c79f1bdfd02f51672c694e8234a5ae0e5c ;; \
        *) echo "Unsupported Obscura architecture: $target_arch" >&2; exit 1 ;; \
    esac; \
    curl -fsSL --retry 3 -o /tmp/obscura.tar.gz \
        "https://github.com/h4ckf0r0day/obscura/releases/download/v0.2.7/obscura-${arch}-linux.tar.gz"; \
    echo "$sha  /tmp/obscura.tar.gz" | sha256sum -c -; \
    tar -xzf /tmp/obscura.tar.gz -C /usr/local/bin obscura

FROM python:3.13-slim
RUN apt-get update && apt-get install -y --no-install-recommends fonts-dejavu-core \
    && rm -rf /var/lib/apt/lists/*
COPY --from=download /usr/local/bin/obscura /usr/local/bin/obscura
COPY server.py /app/server.py
ENTRYPOINT ["python3", "-u", "/app/server.py"]
