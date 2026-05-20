# RawDB Helm chart

Installs the [rawdb](https://github.com/dnglab/rawdb) image as a hardened
multi-replica `Deployment` with a separate metrics listener, ServiceMonitor
support, PodDisruptionBudget, optional HPA, anti-affinity, and topology
spread.

## Install

```bash
# 1. Create the credentials Secret out-of-band (preferred):
kubectl -n rawdb create secret generic rawdb \
  --from-literal=RAWDB_S3_ACCESS_KEY=… \
  --from-literal=RAWDB_S3_SECRET_KEY=… \
  --from-literal=RAWDB_ADMIN_PASSWORD_HASH='$argon2id$…' \
  --from-literal=RAWDB_SESSION_KEY="$(openssl rand -hex 32)"

# 2. Point the chart at it and set bucket details:
helm upgrade --install rawdb chart/ \
  --namespace rawdb --create-namespace \
  --set secret.create=false \
  --set secret.existingSecret=rawdb \
  --set config.s3.bucket=rawdb \
  --set config.s3.endpoint=https://garage.example.internal
```

## Highlights

- **Pod security**: `runAsNonRoot`, dropped capabilities, `readOnlyRootFilesystem`,
  `RuntimeDefault` seccomp. uid/gid 65532 (nonroot).
- **Two-port container**: 8080 for the API/SPA, 9090 for `/metrics`, `/live`,
  `/ready`, `/healthz`. Probes hit the metrics port.
- **Anti-affinity + topology spread**: replicas prefer different nodes and
  spread across zones.
- **PodDisruptionBudget**: `minAvailable=1` by default (assumes ≥2 replicas).
- **HPA**: opt-in via `autoscaling.enabled=true`.
- **ServiceMonitor**: opt-in via `serviceMonitor.enabled=true`.
- **NetworkPolicy**: opt-in via `networkPolicy.enabled=true`; permissive
  egress by default, tighten via `networkPolicy.egress`.

See `values.yaml` for everything that's tunable.
