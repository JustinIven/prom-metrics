# prom-metrics

A single, tiny Rust binary that serves the Kubernetes **Resource Metrics API**
(`metrics.k8s.io`) from an existing Prometheus server.

```
Prometheus  ──periodic PromQL──▶  in-memory snapshot  ──▶  metrics.k8s.io
```

It replaces `metrics-server` on clusters that already run Prometheus. It is
**not** a Prometheus Adapter: there are no rules, no custom/external metrics API
and no metric discovery.

## What it implements

| Endpoint | Purpose |
| --- | --- |
| `GET /apis/metrics.k8s.io` | `APIGroup` discovery |
| `GET /apis/metrics.k8s.io/{v1,v1beta1}` | `APIResourceList` |
| `GET /apis/metrics.k8s.io/{version}/nodes[/{name}]` | `NodeMetricsList` / `NodeMetrics` |
| `GET /apis/metrics.k8s.io/{version}/pods` | `PodMetricsList` (all namespaces) |
| `GET /apis/metrics.k8s.io/{version}/namespaces/{ns}/pods[/{name}]` | namespaced list / get |
| `GET /healthz`, `/livez` | process is alive |
| `GET /readyz` | ready once the first snapshot was collected |
| `GET /metrics` | five gauges/counters about the adapter itself |

`v1` and `v1beta1` are the same code path over one internal representation; only
`apiVersion` differs. `?labelSelector=` is supported for equality and existence
requirements (`a=b`, `a!=b`, `a`, `!a`); set-based expressions return `400`.

## Prometheus requirements

The adapter issues exactly six broad instant queries per poll — never one per
pod, and never anything on the HTTP request path.

| Data | Query |
| --- | --- |
| pod/container CPU | `sum by (namespace, pod, container) (rate(container_cpu_usage_seconds_total{container!="", container!="POD"}[$window]))` |
| pod/container memory | `sum by (namespace, pod, container) (last_over_time(container_memory_working_set_bytes{container!="", container!="POD"}[$window]))` |
| node CPU | `sum by (node) (rate(container_cpu_usage_seconds_total{id="/"}[$window]))` |
| node memory | `sum by (node) (last_over_time(container_memory_working_set_bytes{id="/"}[$window]))` |
| pod labels (optional) | `last_over_time(kube_pod_labels[$window])` |
| node labels (optional) | `last_over_time(kube_node_labels[$window])` |

`$window` is `CPU_RATE_WINDOW` and is also reported as the `window` field of
every response, so it truthfully describes the interval the CPU rate was
computed over.

**Expected labels.** cAdvisor metrics must carry `namespace`, `pod` and
`container`; the root cgroup series (`id="/"`) must carry `node`. This is what
`kube-prometheus-stack` and the standard kubelet/cAdvisor scrape configs
produce. A series that does not carry the labels needed to identify a Kubernetes
object is dropped rather than guessed at.

Node metrics are derived from the cAdvisor root cgroup, so no second collection
mechanism (node-exporter) is required.

`kube_pod_labels` / `kube_node_labels` come from kube-state-metrics and are
**optional**. They are used only to evaluate `labelSelector`. If neither series
exists, selector filtering is disabled (fail-open) so an HPA still gets data
instead of an empty list. kube-state-metrics rewrites label names such as
`app.kubernetes.io/name` to `label_app_kubernetes_io_name`; selector keys are
sanitised the same way before matching. Labels are never echoed back in
`metadata.labels`, because the sanitised names would be wrong.

`CPU_RATE_WINDOW` must be at least **twice** the Prometheus scrape interval,
otherwise `rate()` has too few samples.

## Configuration

Environment variables only.

| Variable | Default | Meaning |
| --- | --- | --- |
| `PROMETHEUS_URL` | `http://prometheus:9090` | base URL of Prometheus |
| `POLL_INTERVAL` | `15s` | how often the snapshot is rebuilt (also the query timeout) |
| `MAX_METRICS_AGE` | `60s` | older snapshots are refused with `503` |
| `CPU_RATE_WINDOW` | `60s` | PromQL range window and reported `window` |
| `LISTEN_ADDR` | `0.0.0.0:8443` | bind address |
| `TLS_CERT_FILE` | `/var/run/serving-cert/tls.crt` | serving certificate |
| `TLS_KEY_FILE` | `/var/run/serving-cert/tls.key` | serving key |
| `RUST_LOG` | `info` | log level |

Durations accept `500ms`, `15s`, `2m`, `1h` or a bare number of seconds.

TLS is enabled when both files exist. If the defaults are absent and neither
variable was set, the server falls back to plain HTTP (useful locally) and logs
a warning. If either variable *is* set and the file is missing, startup fails.

Prometheus is reached over plain HTTP: the HTTP client is built without TLS on
purpose, which keeps one TLS stack out of the binary. Use an in-cluster
`http://` Prometheus address.

## Semantics

* **CPU** is reported as Kubernetes millicore quantities (`250m`, `1000m`),
  never as a bare float. Millicores is exactly the granularity the HPA
  controller reduces to.
* **Memory** uses the largest binary suffix that divides exactly
  (`2Gi`, `64Mi`, `4Ki`), falling back to plain bytes, so nothing is rounded
  away.
* `timestamp` is the snapshot time in RFC3339; `window` is `CPU_RATE_WINDOW`.

### Pod edge cases

| Case | Behaviour |
| --- | --- |
| pause / `POD` infrastructure container | excluded by the query and again during aggregation |
| container with CPU but no memory sample (or vice versa) | reported with `0` for the missing side |
| init containers | reported while they still produce samples, then dropped |
| terminated containers | `last_over_time(...[$window])` bounds the lookback, so they disappear within one window instead of lingering for Prometheus' default 5 min staleness |
| zero CPU / zero memory | reported as `0m` / `0`, the pod is not hidden |
| multiple containers | one entry each, sorted by container name |
| series missing `namespace`/`pod`/`container`/`node` | dropped; no identity is invented |
| pod known to Prometheus but gone from the cluster | disappears at the next poll |

## Architecture

```
src/
├── main.rs        runtime, polling loop, TLS/plain serving
├── config.rs      environment parsing
├── prometheus.rs  /api/v1/query client + response parsing
├── metrics.rs     internal model, snapshot building, quantities, cache
├── api.rs         routes, Kubernetes wire format, label selectors
└── error.rs       errors and metav1.Status responses
```

A background task rebuilds a complete `Snapshot` every `POLL_INTERVAL` and
swaps it into an `RwLock<Option<Arc<Snapshot>>>` atomically; the live snapshot
is never mutated. Request handlers clone the `Arc`, do a `BTreeMap` lookup
(namespace listing is a range scan) and serialise borrowed data. There are no
network calls, no Kubernetes API calls and no locks held across `await` on the
request path.

If a poll fails the previous snapshot is kept and the failure is logged; once it
exceeds `MAX_METRICS_AGE` every metrics endpoint returns `503` with a
`metav1.Status` rather than presenting stale data as current.

## Deploying

```sh
# 1. serving certificate (any CA works; use cert-manager in production)
openssl req -x509 -newkey rsa:2048 -nodes -days 365 \
  -subj "/CN=prom-metrics.kube-system.svc" \
  -addext "subjectAltName=DNS:prom-metrics.kube-system.svc" \
  -keyout tls.key -out tls.crt
kubectl -n kube-system create secret tls prom-metrics-serving-cert \
  --cert=tls.crt --key=tls.key

# 2. adapter
kubectl apply -f deploy/deployment.yaml -f deploy/service.yaml

# 3. register with the aggregation layer
kubectl delete apiservice v1beta1.metrics.k8s.io --ignore-not-found  # drop metrics-server
kubectl apply -f deploy/apiservice.yaml
```

Verify:

```sh
kubectl get --raw /apis/metrics.k8s.io/v1 | jq
kubectl get --raw /apis/metrics.k8s.io/v1/nodes | jq
kubectl get --raw /apis/metrics.k8s.io/v1/pods | jq
kubectl top nodes
kubectl top pods -A
```

`deploy/apiservice.yaml` uses `insecureSkipTLSVerify: true`; replace it with
`caBundle` once a proper CA issues the serving certificate. Certificate issuance
and rotation are Kubernetes' job, not the adapter's.

The adapter needs **no RBAC and no ServiceAccount token**
(`automountServiceAccountToken: false`) because every metric comes from
Prometheus.

## Security notes

* Only the six queries above are ever executed; no client input reaches PromQL,
  so the adapter cannot be used as a Prometheus proxy.
* Path parameters are validated as Kubernetes names before lookup.
* Prometheus errors are logged without the URL, so credentials in the URL are
  never printed.
* Per the design constraints there is **no authentication or authorization**:
  anything that can reach port 8443 can read cluster metrics. Restrict it with a
  `NetworkPolicy` if that matters in your cluster.

## Building and testing

```sh
cargo test          # 32 unit + integration tests, Prometheus is mocked
cargo build --release
podman build -t prom-metrics:0.1.0 .   # or docker build
```

## Measured

The image is `rust:1-alpine` → `FROM scratch` and contains nothing but the
statically linked musl binary: no shell, no package manager, no CA bundle (none
is needed — Prometheus is plain HTTP and the serving certificate is mounted).

On a single-node kind cluster (v1.37.0) with the Prometheus scrape config above:

| | |
| --- | --- |
| stripped release binary (musl, static) | 3.2 MiB |
| container image (`scratch` + binary) | 3.27 MB, 1.5 MB gzipped for registry transfer |
| adapter RSS (`container_memory_working_set_bytes`) | 1.4 MiB |
| adapter CPU | 1m, idling between polls |
| full refresh of all six PromQL queries | 4.1 ms |
| Prometheus query rate | 6 queries per `POLL_INTERVAL`, i.e. 24/min at the default 15s |
| `GET /apis/metrics.k8s.io/v1/pods` served directly | ~1.0 ms average over 50 keep-alive requests |

## Deliberately not implemented

Custom/External Metrics APIs, configurable PromQL rules, metric discovery,
informers, controllers, leader election, CRDs, RBAC, persistence, sharding, HA
coordination, authentication/authorization, a web UI.
