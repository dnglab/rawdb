# Wrapper image for the one-shot Garage initialization step.
#
# The official `dxflrs/garage` image is `FROM scratch` (single static
# binary, no shell), so we can't run shell scripts in it. This image
# adds `/bin/sh` on top so `garage-init.sh` can drive the CLI to assign
# the cluster layout, create the bucket, and import the dev key.
#
# `/garage` stays at the root so the script's `/garage <cmd>` calls
# work unchanged.

FROM dxflrs/garage:v2.3.0 AS garage

FROM debian:trixie-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      ca-certificates \
      wget \
      curl \
      netcat-openbsd \
 && rm -rf /var/lib/apt/lists/*
COPY --from=garage /garage /garage
RUN chmod +x /garage
