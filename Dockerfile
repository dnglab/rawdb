# syntax=docker/dockerfile:1.7

# --- Stage 1: frontend ----------------------------------------------------------
FROM node:26.1-slim AS frontend
WORKDIR /fe

RUN npm install -g corepack@latest && \
    corepack enable && \
    corepack prepare yarn@4.1.0 --activate

COPY frontend/package.json frontend/yarn.lock ./

RUN --mount=type=cache,mode=0777,uid=1001,gid=0,target=/usr/local/share/.cache/yarn \
    yarn install --immutable

COPY frontend/ ./
RUN --mount=type=cache,mode=0777,uid=1001,gid=0,target=/usr/local/share/.cache/yarn \
    --mount=type=cache,mode=0777,uid=1001,gid=0,target=public \
    --mount=type=cache,mode=0777,uid=1001,gid=0,target=.cache \
    yarn build

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

# --- Stage 3: runtime -----------------------------------------------------------
FROM alpine:3.23

WORKDIR /app
COPY --from=backend /be/target/release/rawdb /app/rawdb
COPY --from=frontend /fe/dist /app/static

ENV RAWDB_STATIC_DIR=/app/static \
    RAWDB_CACHE_DIR=/tmp/rawdb-cache \
    RAWDB_BIND=0.0.0.0:8080 \
    RAWDB_METRICS_BIND=0.0.0.0:9090

EXPOSE 8080 9090
ENTRYPOINT ["/app/rawdb"]
