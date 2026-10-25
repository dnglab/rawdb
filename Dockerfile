# syntax=docker/dockerfile:1.7

# ExifTool for the reviewer "exiftool output" panel. Latest release:
# https://exiftool.org/ver.txt; its SHA2-256 is listed in
# https://exiftool.org/checksums-<version>.txt. Bump both together.
ARG EXIFTOOL_VERSION=13.59
ARG EXIFTOOL_SHA256=668ea3acececb7235fbd0f4900e72d5f12c9b07e5c778fd36cb1e9b5828fd65a

# --- Stage 1: frontend ----------------------------------------------------------
FROM node:26.1-slim AS frontend
WORKDIR /fe

RUN npm install -g corepack@latest && \
    corepack enable && \
    corepack prepare yarn@4.1.0 --activate

# Copy the lockfile + yarn config first so the global-cache mount on the
# install step does most of the work; then bring the rest of the source
# in and build in the same layer so yarn's `.yarn/install-state.gz` is
# always present for `yarn build`.
COPY frontend/package.json frontend/yarn.lock frontend/.yarnrc.yml ./
COPY frontend/ ./
RUN --mount=type=cache,mode=0777,target=/usr/local/share/.cache/yarn \
    yarn install --immutable && yarn build

# --- Stage 2: backend -----------------------------------------------------------
FROM rust:1.95.0-alpine3.23 AS backend
WORKDIR /be

# `utoipa-swagger-ui`'s build.rs fetches the Swagger UI dist via curl at
# compile time; the slim rust:alpine image doesn't ship it.
RUN apk add --no-cache curl ca-certificates

# Cache deps separately from sources.
COPY backend/Cargo.toml ./
COPY backend/Cargo.lock* ./
COPY backend/static ./static
RUN mkdir -p src && echo "fn main() {}" > src/main.rs \
    && cargo build --release \
    && rm -rf src

COPY backend/src ./src
RUN touch src/main.rs && cargo build --release

# --- Stage 3: exiftool ----------------------------------------------------------
# The official release, checksum-verified; only the script and its lib/
# are kept.
FROM alpine:3.23 AS exiftool
ARG EXIFTOOL_VERSION
ARG EXIFTOOL_SHA256
RUN wget -qO /tmp/exiftool.tar.gz \
        "https://downloads.sourceforge.net/project/exiftool/Image-ExifTool-${EXIFTOOL_VERSION}.tar.gz" \
    && echo "${EXIFTOOL_SHA256}  /tmp/exiftool.tar.gz" | sha256sum -c - \
    && tar -xzf /tmp/exiftool.tar.gz -C /tmp \
    && mkdir /opt/exiftool \
    && mv "/tmp/Image-ExifTool-${EXIFTOOL_VERSION}/exiftool" \
          "/tmp/Image-ExifTool-${EXIFTOOL_VERSION}/lib" /opt/exiftool/

# --- Stage 4: runtime -----------------------------------------------------------
FROM alpine:3.23

# ExifTool runs on Perl. The symlink puts it on PATH; the script finds its
# lib/ through the link. `-ver` fails the build if a Perl module is missing.
RUN apk add --no-cache perl
COPY --from=exiftool /opt/exiftool /opt/exiftool
RUN ln -s /opt/exiftool/exiftool /usr/local/bin/exiftool && exiftool -ver

WORKDIR /app
COPY --from=backend /be/target/release/rawdb /app/rawdb
COPY --from=frontend /fe/dist /app/static

ENV RAWDB_STATIC_DIR=/app/static \
    RAWDB_CACHE_DIR=/tmp/rawdb-cache \
    RAWDB_BIND=0.0.0.0:8080 \
    RAWDB_METRICS_BIND=0.0.0.0:9090

EXPOSE 8080 9090
ENTRYPOINT ["/app/rawdb"]
