# prom-metrics Helm chart

Installs [prom-metrics](../../README.md): the Kubernetes Resource Metrics API
(`metrics.k8s.io`) served from Prometheus, as a replacement for metrics-server.

```sh
helm upgrade --install prom-metrics oci://ghcr.io/justiniven/charts/prom-metrics \
  -n kube-system --set prometheus.url=http://prometheus-operated.monitoring.svc:9090
```

The chart is published with every `v*.*.*` release; its version and `appVersion`
equal the release version. Use `--version` to pin, or `./charts/prom-metrics` to
install from a checkout.

Uninstall metrics-server first; only one release can own `metrics.k8s.io`.

Defaults are production-ready: 2 replicas spread across nodes, a PDB,
`system-cluster-critical` priority, the `restricted` Pod Security profile, no
ServiceAccount token, and a Helm-generated CA so the APIService is verified
(`insecureSkipTLSVerify: false`).

## TLS

The aggregation layer requires HTTPS. The adapter does not generate or reload
certificates, so the chart provides them via `tls.type`:

| `tls.type` | Certificate source | `caBundle` | Rotation |
| --- | --- | --- | --- |
| `helm` (default) | CA + cert generated at install, reused on upgrade via `lookup` | set by the chart | pods roll automatically when the Secret changes |
| `cert-manager` | `Certificate` from a self-signed or existing issuer | injected by cert-manager's CA injector | restart after renewal, e.g. add `reloader.stakater.com/auto: "true"` to `deploymentAnnotations` |
| `existingSecret` | your Secret with `tls.crt`/`tls.key` | `apiService.caBundle`, or `ca.crt` from the Secret | restart after replacing the Secret |

GitOps tools that render without cluster access (Argo CD) cannot use `lookup`,
so `tls.type=helm` regenerates the certificate on every render. Use
`cert-manager` or `existingSecret` there.

## Values

| Key | Default | Description |
| --- | --- | --- |
| `prometheus.url` | `http://prometheus-operated.monitoring.svc:9090` | Prometheus base URL; plain `http://` only |
| `pollInterval` | `15s` | Snapshot refresh period and Prometheus query timeout |
| `maxMetricsAge` | `60s` | Snapshots older than this are answered with `503`; keep ≥ 3 × `pollInterval` |
| `cpuRateWindow` | `60s` | PromQL `rate()` window; must be ≥ 2 × the Prometheus scrape interval |
| `logLevel` | `info` | `RUST_LOG` |
| `extraEnv` | `[]` | Extra container environment variables |
| `image.repository` | `ghcr.io/justiniven/prom-metrics` | Image repository |
| `image.tag` | `""` | Defaults to the chart `appVersion` |
| `image.digest` | `""` | Pins by digest; overrides `image.tag` |
| `image.pullPolicy` | `IfNotPresent` | Pull policy |
| `imagePullSecrets` | `[]` | Pull secrets |
| `nameOverride` / `fullnameOverride` / `namespaceOverride` | `""` | Naming overrides |
| `commonLabels` | `{}` | Labels added to every object |
| `apiService.create` | `true` | Register the APIServices |
| `apiService.v1.create` / `apiService.v1beta1.create` | `true` | Register each version |
| `apiService.annotations` | `{}` | APIService annotations |
| `apiService.caBundle` | `""` | PEM CA; only needed for `tls.type=existingSecret` |
| `apiService.insecureSkipTLSVerify` | `false` | Skip backend verification; avoid in production |
| `tls.type` | `helm` | `helm`, `cert-manager` or `existingSecret` |
| `tls.clusterDomain` | `cluster.local` | Used for the certificate SANs |
| `tls.helm.certDurationDays` | `3650` | Validity of the generated CA and certificate |
| `tls.helm.lookup` | `true` | Reuse the Secret from a previous release |
| `tls.certManager.addInjectorAnnotations` | `true` | Add `cert-manager.io/inject-ca-from` to the APIServices |
| `tls.certManager.existingIssuer.enabled` | `false` | Use an existing issuer instead of a self-signed one |
| `tls.certManager.existingIssuer.{group,kind,name}` | `cert-manager.io`, `Issuer`, `my-issuer` | Issuer reference |
| `tls.certManager.duration` / `renewBefore` | `""` | Certificate lifetime settings |
| `tls.certManager.annotations` / `labels` | `{}` | Certificate metadata |
| `tls.existingSecret.name` | `""` | Secret with `tls.crt` and `tls.key` |
| `tls.existingSecret.lookup` | `true` | Read `ca.crt` from the Secret when `apiService.caBundle` is empty |
| `replicas` | `2` | Replicas; each polls Prometheus independently (6 queries per poll) |
| `revisionHistoryLimit` | `5` | ReplicaSets kept |
| `updateStrategy` | `maxUnavailable: 0`, `maxSurge: 1` | Deployment strategy |
| `podDisruptionBudget.enabled` | `true` | Create a PDB |
| `podDisruptionBudget.minAvailable` | `1` | Ignored when `maxUnavailable` is set |
| `podDisruptionBudget.maxUnavailable` | `null` | PDB max unavailable |
| `podDisruptionBudget.unhealthyPodEvictionPolicy` | `null` | `IfHealthyBudget` or `AlwaysAllow` |
| `priorityClassName` | `system-cluster-critical` | Evicting this pod stops every HPA |
| `deploymentAnnotations` / `podAnnotations` / `podLabels` | `{}` | Metadata |
| `podSecurityContext` | non-root 65532, `RuntimeDefault` seccomp | Pod security context |
| `securityContext` | read-only root, no privilege escalation, drop `ALL` | Container security context |
| `containerPort` | `8443` | Listen port |
| `livenessProbe` / `readinessProbe` | `/livez`, `/readyz` over HTTPS | Probes |
| `resources` | requests `10m`/`32Mi`, limit `64Mi` memory | No CPU limit on purpose |
| `service.type` / `service.port` | `ClusterIP` / `443` | Service |
| `service.annotations` / `service.labels` | `{}` | Service metadata |
| `nodeSelector` / `tolerations` / `affinity` | empty | Scheduling |
| `topologySpreadConstraints` | soft spread by hostname | `labelSelector` is filled in when omitted |
| `extraVolumes` / `extraVolumeMounts` | `[]` | Extra volumes |
| `serviceMonitor.enabled` | `false` | Create a `ServiceMonitor` for `/metrics` |
| `serviceMonitor.additionalLabels` | `{}` | Labels for Prometheus Operator selection |
| `serviceMonitor.interval` / `scrapeTimeout` | `1m` / `10s` | Scrape settings |
| `serviceMonitor.relabelings` / `metricRelabelings` | `[]` | Relabeling |
| `serviceMonitor.tlsConfig` | `insecureSkipVerify: true` | Scrape TLS config |
| `prometheusRule.enabled` | `false` | Create alerts for staleness, query errors, empty snapshots, slow refreshes and APIService availability |
| `prometheusRule.additionalLabels` | `{}` | Labels for Prometheus Operator selection |
| `prometheusRule.staleAfterSeconds` | `120` | Staleness threshold |
| `prometheusRule.slowRefreshSeconds` | `5` | Slow-refresh threshold |
| `prometheusRule.extraRules` | `[]` | Extra rules in the same group |
| `networkPolicy.enabled` | `false` | Create a NetworkPolicy (DNS + Prometheus egress, port 8443 ingress) |
| `networkPolicy.ingressFrom` | `[]` | Allowed sources; empty allows any on `containerPort` |
| `networkPolicy.prometheusTo` | `[]` | Prometheus peers; empty allows any on `prometheusPort` |
| `networkPolicy.prometheusPort` | `9090` | Prometheus port |
