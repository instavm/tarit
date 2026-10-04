FROM debian:12.11-slim@sha256:b1a741487078b369e78119849663d7f1a5341ef2768798f7b7406c4240f86aef

ENV DEBIAN_FRONTEND=noninteractive \
    LANG=C.UTF-8 \
    LC_ALL=C.UTF-8

# The immutable Debian snapshot fixes GCC, binutils, libc headers, and every
# other package involved in the kernel build.
RUN printf '%s\n' \
      'deb [check-valid-until=no] http://snapshot.debian.org/archive/debian/20250701T000000Z bookworm main' \
      > /etc/apt/sources.list \
    && rm -f /etc/apt/sources.list.d/debian.sources \
    && apt-get update \
    && apt-get install -y --no-install-recommends \
      bc \
      bison \
      build-essential \
      ca-certificates \
      curl \
      flex \
      libelf-dev \
      libssl-dev \
      xz-utils \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /workspace
