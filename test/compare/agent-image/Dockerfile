FROM python:3.12-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends git build-essential curl ca-certificates iproute2 sqlite3 procps \
 && rm -rf /var/lib/apt/lists/*
