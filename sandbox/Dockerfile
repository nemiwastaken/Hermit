FROM python:3.13-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
        bash git ripgrep jq curl less procps ca-certificates \
        docx2txt poppler-utils tesseract-ocr tesseract-ocr-eng tesseract-ocr-rus tesseract-ocr-ukr \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY server.py /app/server.py

ENTRYPOINT ["python3", "-u", "/app/server.py"]
CMD ["--socket", "/run/hermit/sandbox.sock"]
