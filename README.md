# RawDB

A community site for sharing camera RAW sample files, built as
backing infrastructure for [dnglab](https://github.com/dnglab/dnglab)
and [rawler](https://crates.io/crates/rawler) decoder testing.

- **Backend:** Rust + axum
- **Frontend:** Vue 3 + Vite + PrimeVue
- **Storage:** any S3-compatible bucket (no database server)
- **Single container image**, listens on **port 8080**
- **Multi-pod ready** — each pod scans S3 independently into a per-pod
  SQLite cache at `/tmp/rawdb-cache`

## Layout in S3

```
s3://<bucket>/
├── samples/<maker>/<model>/                # approved, publicly browsable
│   ├── rawdb-meta.toml
│   ├── raw_modes/…
│   ├── crm/…                                # optional Canon raw movies
│   └── heif/…                               # optional HEIF
├── pending/<maker>/<model>/<upload_id>/    # awaiting review
│   ├── rawdb-meta.toml
│   └── …
└── _system/users.toml                       # OIDC user roster
```

`upload_id` looks like `20260514T180000Z-a1b2c3d4`.

`rawdb-meta.toml` is hand-authorable. The schema lives in
[backend/src/meta.rs](backend/src/meta.rs); minimal example:

```toml
[set]
maker = "Canon"
model = "EOS R5"
license = "CC0-1.0"

[[files]]
path = "raw_modes/IMG_0001.cr3"
compression = "lossless"
bit_depth = 14
aspect_ratio = "3:2"
```

No `set.id`, no per-file `size` — both are derived (the set's identity
is the S3 path, file sizes come from the listing).

## Local development

Requires podman (or docker — Dockerfile is portable) and a local
[Garage](https://garagehq.deuxfleurs.fr) S3 server, both wired in
[docker-compose.yml](docker-compose.yml).

```bash
podman compose up --build
# open http://localhost:8080
```

The `seed` service uploads two example sets so the UI has something to
show on first run.

Backend-only iteration (against a separately-running Garage):

```bash
cd backend
RAWDB_S3_BUCKET=rawdb \
RAWDB_S3_ENDPOINT=http://localhost:3900 \
RAWDB_S3_ACCESS_KEY=… RAWDB_S3_SECRET_KEY=… \
RAWDB_S3_PATH_STYLE=true \
RAWDB_ADMIN_PASSWORD=dev \
RAWDB_SESSION_KEY=$(openssl rand -hex 32) \
cargo run
```

Frontend dev server (with `/api` proxied to the backend on 8080):

```bash
cd frontend
npm install
npm run dev
```

## Production deployment

See [k8s/README.md](k8s/README.md). The short version:

```bash
kubectl apply -f k8s/namespace.yaml -f k8s/configmap.yaml
cp k8s/secret.example.yaml k8s/secret.yaml && $EDITOR k8s/secret.yaml
kubectl apply -f k8s/secret.yaml -f k8s/deployment.yaml -f k8s/service.yaml
```

## Configuration

All env vars use the `RAWDB_` prefix. Full list in
[backend/src/config.rs](backend/src/config.rs). The mandatory ones:

| Var | Notes |
|---|---|
| `RAWDB_S3_BUCKET`, `RAWDB_S3_ACCESS_KEY`, `RAWDB_S3_SECRET_KEY` | S3 connection |
| `RAWDB_S3_ENDPOINT` | optional — set for Garage / R2 / B2 / SeaweedFS |
| `RAWDB_ADMIN_PASSWORD` *or* `RAWDB_ADMIN_PASSWORD_HASH` | bootstrap admin |
| `RAWDB_SESSION_KEY` | ≥ 32 bytes (hex or otherwise) for JWT signing |

OIDC is opt-in: set all four of `RAWDB_OIDC_ISSUER_URL`, `_CLIENT_ID`,
`_CLIENT_SECRET`, `_REDIRECT_URL` to enable it.

## Architecture details

The approved implementation plan lives at
`~/.claude/plans/i-m-the-developer-of-effervescent-sphinx.md`; the
in-repo source is the canonical reference for current behavior. Key
invariants:

- **S3 is the only durable store.** SQLite at `/tmp/rawdb-cache` is
  ephemeral and rebuilt at startup from S3.
- The cache **scanner** runs a three-pass reconciliation
  (`samples/`, `pending/`, `_system/users.toml`) on a configurable
  interval (`RAWDB_RESCAN_SECS`, default 5 min). Each pass uses an
  ETag fast-path; unchanged sets are skipped without re-parsing the
  meta TOML — the >10k-set scalability path.
- **Approval flow**: copy `pending/.../upload_id/*` → `samples/.../*`,
  delete the pending originals, refresh the originating pod's cache.
  Peers pick up the change on their next tick.
- **User management** is just a TOML file at `s3://<bucket>/_system/users.toml`.
  Writes use S3 `If-Match` so concurrent edits from peer pods are
  detected and retried.

## License

MIT OR Apache-2.0.
