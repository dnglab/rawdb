{{/*
Expand the name of the chart.
*/}}
{{- define "rawdb.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{/*
Fully qualified app name. Truncated to 63 chars per DNS label limit.
*/}}
{{- define "rawdb.fullname" -}}
{{- if .Values.fullnameOverride -}}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- $name := default .Chart.Name .Values.nameOverride -}}
{{- if contains $name .Release.Name -}}
{{- .Release.Name | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}
{{- end -}}

{{/*
Chart label value: `<name>-<version>` sanitized.
*/}}
{{- define "rawdb.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{/*
Common labels.
*/}}
{{- define "rawdb.labels" -}}
helm.sh/chart: {{ include "rawdb.chart" . }}
{{ include "rawdb.selectorLabels" . }}
{{- if .Chart.AppVersion }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
{{- end }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end -}}

{{/*
Selector labels (stable across upgrades).
*/}}
{{- define "rawdb.selectorLabels" -}}
app.kubernetes.io/name: {{ include "rawdb.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{/*
ServiceAccount name.
*/}}
{{- define "rawdb.serviceAccountName" -}}
{{- if .Values.serviceAccount.create -}}
{{- default (include "rawdb.fullname" .) .Values.serviceAccount.name -}}
{{- else -}}
{{- default "default" .Values.serviceAccount.name -}}
{{- end -}}
{{- end -}}

{{/*
Name of the Secret the Deployment envFroms. Either the externally-managed
one (`secret.existingSecret`) or the chart-rendered one.
*/}}
{{- define "rawdb.secretName" -}}
{{- if .Values.secret.existingSecret -}}
{{- .Values.secret.existingSecret -}}
{{- else -}}
{{- include "rawdb.fullname" . -}}
{{- end -}}
{{- end -}}

{{/*
Names of the enabled Traefik middlewares, in attachment order. Empty list
when middlewares are disabled. Each entry is the bare resource name; callers
add namespace/CRD suffix as needed.
*/}}
{{- define "rawdb.traefikMiddlewareNames" -}}
{{- $names := list -}}
{{- if .Values.traefik.middlewares.enabled -}}
{{- $full := include "rawdb.fullname" . -}}
{{- if .Values.traefik.middlewares.rateLimit.enabled -}}
{{- $names = append $names (printf "%s-ratelimit" $full) -}}
{{- end -}}
{{- if .Values.traefik.middlewares.secureHeaders.enabled -}}
{{- $names = append $names (printf "%s-secureheaders" $full) -}}
{{- end -}}
{{- if .Values.traefik.middlewares.compression.enabled -}}
{{- $names = append $names (printf "%s-compression" $full) -}}
{{- end -}}
{{- end -}}
{{- $names | toJson -}}
{{- end -}}

{{/*
Annotation value for `traefik.ingress.kubernetes.io/router.middlewares`:
`<ns>-<name>@kubernetescrd,...` in attachment order.
*/}}
{{- define "rawdb.traefikIngressMiddlewareAnnotation" -}}
{{- $ns := .Release.Namespace -}}
{{- $names := include "rawdb.traefikMiddlewareNames" . | fromJsonArray -}}
{{- $refs := list -}}
{{- range $n := $names -}}
{{- $refs = append $refs (printf "%s-%s@kubernetescrd" $ns $n) -}}
{{- end -}}
{{- join "," $refs -}}
{{- end -}}

{{/*
Fully qualified image reference, honoring digest pinning when set.
*/}}
{{- define "rawdb.image" -}}
{{- $tag := default .Chart.AppVersion .Values.image.tag -}}
{{- if .Values.image.digest -}}
{{- printf "%s@%s" .Values.image.repository .Values.image.digest -}}
{{- else -}}
{{- printf "%s:%s" .Values.image.repository $tag -}}
{{- end -}}
{{- end -}}
