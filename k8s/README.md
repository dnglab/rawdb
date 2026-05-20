# RawDB Kubernetes manifests

Apply in this order:

```bash
kubectl apply -f namespace.yaml
kubectl apply -f configmap.yaml

# Copy the template, fill in real credentials, apply.
cp secret.example.yaml secret.yaml
$EDITOR secret.yaml
kubectl apply -f secret.yaml

kubectl apply -f deployment.yaml
kubectl apply -f service.yaml

# Optional: HTTP ingress (edit host first).
kubectl apply -f ingress.example.yaml
```

## Notes

- **Replicas: 2** by default. Each pod scans S3 independently and serves
  from its own SQLite cache (`emptyDir`). Approvals from one pod are
  picked up by peers on their next rescan (`RAWDB_RESCAN_SECS`).
- **Readiness vs. liveness.** `/healthz?ready=1` returns 200 only after
  the first full S3 scan completes, so the Service won't route traffic
  to a pod with an empty cache. `/healthz` (liveness) returns 200
  immediately.
- **Bootstrap admin password** is mandatory (`RAWDB_ADMIN_PASSWORD` or
  `RAWDB_ADMIN_PASSWORD_HASH`). Use the argon2 hash variant in prod.
- **OIDC** is optional. To enable, set all four `RAWDB_OIDC_*` env vars
  in `secret.yaml`. Use `RAWDB_OIDC_INITIAL_ADMIN_SUB` for first-login
  provisioning, then comment it out once the admin has been created via
  the UI.
