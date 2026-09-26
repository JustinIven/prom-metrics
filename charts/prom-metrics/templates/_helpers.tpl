{{- define "prom-metrics.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}

{{- define "prom-metrics.fullname" -}}
{{- if .Values.fullnameOverride }}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- $name := default .Chart.Name .Values.nameOverride }}
{{- if contains $name .Release.Name }}
{{- .Release.Name | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" }}
{{- end }}
{{- end }}
{{- end }}

{{- define "prom-metrics.namespace" -}}
{{- default .Release.Namespace .Values.namespaceOverride }}
{{- end }}

{{- define "prom-metrics.selectorLabels" -}}
app.kubernetes.io/name: {{ include "prom-metrics.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{- define "prom-metrics.labels" -}}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{ include "prom-metrics.selectorLabels" . }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- with .Values.commonLabels }}
{{ toYaml . }}
{{- end }}
{{- end }}

{{- define "prom-metrics.image" -}}
{{- if .Values.image.digest }}
{{- printf "%s@%s" .Values.image.repository .Values.image.digest }}
{{- else }}
{{- printf "%s:%s" .Values.image.repository (default .Chart.AppVersion .Values.image.tag) }}
{{- end }}
{{- end }}

{{- define "prom-metrics.tlsSecretName" -}}
{{- if eq .Values.tls.type "existingSecret" }}
{{- required "tls.existingSecret.name is required when tls.type=existingSecret" .Values.tls.existingSecret.name }}
{{- else }}
{{- printf "%s-tls" (include "prom-metrics.fullname" .) }}
{{- end }}
{{- end }}

{{- define "prom-metrics.dnsNames" -}}
{{- $svc := include "prom-metrics.fullname" . }}
{{- $ns := include "prom-metrics.namespace" . }}
{{- list (printf "%s.%s.svc" $svc $ns) (printf "%s.%s.svc.%s" $svc $ns .Values.tls.clusterDomain) | toJson }}
{{- end }}

{{/*
Base64 TLS material for tls.type=helm as JSON {crt,key,ca}.
Cached in .Values so the Secret, APIService and pod checksum see the same certificate.
*/}}
{{- define "prom-metrics.helmTLS" -}}
{{- if not (hasKey .Values "__tls") }}
{{- $existing := dict }}
{{- if .Values.tls.helm.lookup }}
{{- $existing = lookup "v1" "Secret" (include "prom-metrics.namespace" .) (include "prom-metrics.tlsSecretName" .) }}
{{- end }}
{{- if and $existing $existing.data (hasKey $existing.data "ca.crt") }}
{{- $_ := set .Values "__tls" (dict "crt" (index $existing.data "tls.crt") "key" (index $existing.data "tls.key") "ca" (index $existing.data "ca.crt")) }}
{{- else }}
{{- $days := int .Values.tls.helm.certDurationDays }}
{{- $dns := include "prom-metrics.dnsNames" . | fromJsonArray }}
{{- $ca := genCA (printf "%s-ca" (include "prom-metrics.fullname" .)) $days }}
{{- $cert := genSignedCert (first $dns) nil $dns $days $ca }}
{{- $_ := set .Values "__tls" (dict "crt" ($cert.Cert | b64enc) "key" ($cert.Key | b64enc) "ca" ($ca.Cert | b64enc)) }}
{{- end }}
{{- end }}
{{- toJson .Values.__tls }}
{{- end }}

{{/* Base64 caBundle for the APIService, or empty when cert-manager injects it or verification is skipped. */}}
{{- define "prom-metrics.caBundle" -}}
{{- if .Values.apiService.insecureSkipTLSVerify }}
{{- else if eq .Values.tls.type "helm" }}
{{- (include "prom-metrics.helmTLS" . | fromJson).ca }}
{{- else if eq .Values.tls.type "existingSecret" }}
{{- if .Values.apiService.caBundle }}
{{- .Values.apiService.caBundle | b64enc }}
{{- else }}
{{- $secret := dict }}
{{- if .Values.tls.existingSecret.lookup }}
{{- $secret = lookup "v1" "Secret" (include "prom-metrics.namespace" .) (include "prom-metrics.tlsSecretName" .) }}
{{- end }}
{{- if and $secret $secret.data (hasKey $secret.data "ca.crt") }}
{{- index $secret.data "ca.crt" }}
{{- else }}
{{- fail "tls.type=existingSecret: set apiService.caBundle (or store ca.crt in the Secret), or set apiService.insecureSkipTLSVerify=true" }}
{{- end }}
{{- end }}
{{- else if and (eq .Values.tls.type "cert-manager") (not .Values.tls.certManager.addInjectorAnnotations) }}
{{- required "apiService.caBundle is required when tls.certManager.addInjectorAnnotations=false" .Values.apiService.caBundle | b64enc }}
{{- end }}
{{- end }}
