FROM python:3.13-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# trafilatura turns a page into its main text; ddgs is a keyless DuckDuckGo client.
RUN pip install --no-cache-dir trafilatura ddgs

WORKDIR /app
COPY server.py /app/server.py

ENTRYPOINT ["python3", "-u", "/app/server.py"]
CMD ["--socket", "/run/hermit/net.sock"]
