use std::{
    collections::HashMap,
    sync::{atomic::Ordering, Arc},
    time::Duration,
};

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{
    error::{k8s_status, not_found},
    metrics::{
        format_cpu, format_memory, sanitize_label_key, NodeMetric, PodMetric, Snapshot, Store,
    },
};

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<Store>,
    pub max_metrics_age: Duration,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/livez", get(healthz))
        .route("/readyz", get(readyz))
        .route("/metrics", get(self_metrics))
        .route("/apis/metrics.k8s.io", get(api_group))
        .route("/apis/metrics.k8s.io/{version}", get(api_resources))
        .route("/apis/metrics.k8s.io/{version}/nodes", get(list_nodes))
        .route("/apis/metrics.k8s.io/{version}/nodes/{name}", get(get_node))
        .route("/apis/metrics.k8s.io/{version}/pods", get(list_pods))
        .route(
            "/apis/metrics.k8s.io/{version}/namespaces/{namespace}/pods",
            get(list_pods_in_ns),
        )
        .route(
            "/apis/metrics.k8s.io/{version}/namespaces/{namespace}/pods/{name}",
            get(get_pod),
        )
        .fallback(fallback)
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Health & self observability
// ---------------------------------------------------------------------------

async fn healthz() -> &'static str {
    "ok"
}

async fn readyz(State(state): State<AppState>) -> Response {
    match state.store.load() {
        Some(_) => (StatusCode::OK, "ok").into_response(),
        None => (StatusCode::SERVICE_UNAVAILABLE, "no metrics snapshot yet").into_response(),
    }
}

async fn self_metrics(State(state): State<AppState>) -> Response {
    let s = &state.store.stats;
    // No lossless conversion exists between `u64` microseconds and `f64` seconds.
    #[allow(clippy::cast_precision_loss, clippy::as_conversions)]
    let last_duration_secs = s.last_duration_micros.load(Ordering::Relaxed) as f64 / 1e6;
    let body = format!(
        "# HELP metrics_adapter_prometheus_query_duration_seconds Duration of the last Prometheus refresh.\n\
         # TYPE metrics_adapter_prometheus_query_duration_seconds gauge\n\
         metrics_adapter_prometheus_query_duration_seconds {:.6}\n\
         # HELP metrics_adapter_prometheus_query_errors_total Failed Prometheus refreshes.\n\
         # TYPE metrics_adapter_prometheus_query_errors_total counter\n\
         metrics_adapter_prometheus_query_errors_total {}\n\
         # HELP metrics_adapter_last_successful_refresh_timestamp_seconds Unix time of the last successful refresh.\n\
         # TYPE metrics_adapter_last_successful_refresh_timestamp_seconds gauge\n\
         metrics_adapter_last_successful_refresh_timestamp_seconds {}\n\
         # HELP metrics_adapter_cached_pods Pods in the current snapshot.\n\
         # TYPE metrics_adapter_cached_pods gauge\n\
         metrics_adapter_cached_pods {}\n\
         # HELP metrics_adapter_cached_nodes Nodes in the current snapshot.\n\
         # TYPE metrics_adapter_cached_nodes gauge\n\
         metrics_adapter_cached_nodes {}\n",
        last_duration_secs,
        s.query_errors.load(Ordering::Relaxed),
        s.last_success_unix.load(Ordering::Relaxed),
        s.cached_pods.load(Ordering::Relaxed),
        s.cached_nodes.load(Ordering::Relaxed),
    );
    ([("content-type", "text/plain; version=0.0.4")], body).into_response()
}

async fn fallback() -> Response {
    k8s_status(
        StatusCode::NOT_FOUND,
        "NotFound",
        "the server could not find the requested resource",
    )
}

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

const VERSIONS: [&str; 2] = ["v1", "v1beta1"];

fn group_version(version: &str) -> Option<String> {
    VERSIONS
        .contains(&version)
        .then(|| format!("metrics.k8s.io/{version}"))
}

async fn api_group() -> Json<serde_json::Value> {
    Json(json!({
        "kind": "APIGroup",
        "apiVersion": "v1",
        "name": "metrics.k8s.io",
        "versions": VERSIONS.map(|v| json!({"groupVersion": format!("metrics.k8s.io/{v}"), "version": v})),
        "preferredVersion": {"groupVersion": "metrics.k8s.io/v1beta1", "version": "v1beta1"},
    }))
}

async fn api_resources(Path(version): Path<String>) -> Response {
    let Some(gv) = group_version(&version) else {
        return fallback().await;
    };
    Json(json!({
        "kind": "APIResourceList",
        "apiVersion": "v1",
        "groupVersion": gv,
        "resources": [
            {"name": "nodes", "singularName": "node", "namespaced": false, "kind": "NodeMetrics", "verbs": ["get", "list"]},
            {"name": "pods", "singularName": "pod", "namespaced": true, "kind": "PodMetrics", "verbs": ["get", "list"]},
        ],
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// Wire format (shared by v1 and v1beta1)
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct Usage {
    cpu: String,
    memory: String,
}

#[derive(Serialize)]
struct ContainerOut<'a> {
    name: &'a str,
    usage: Usage,
}

#[derive(Serialize)]
struct Meta<'a> {
    name: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    namespace: Option<&'a str>,
    #[serde(rename = "creationTimestamp")]
    creation_timestamp: &'a str,
}

#[derive(Serialize)]
struct PodMetricsOut<'a> {
    #[serde(rename = "apiVersion", skip_serializing_if = "Option::is_none")]
    api_version: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    kind: Option<&'a str>,
    metadata: Meta<'a>,
    timestamp: &'a str,
    window: &'a str,
    containers: Vec<ContainerOut<'a>>,
}

#[derive(Serialize)]
struct NodeMetricsOut<'a> {
    #[serde(rename = "apiVersion", skip_serializing_if = "Option::is_none")]
    api_version: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    kind: Option<&'a str>,
    metadata: Meta<'a>,
    timestamp: &'a str,
    window: &'a str,
    usage: Usage,
}

#[derive(Serialize)]
struct ListMeta {}

#[derive(Serialize)]
struct MetricsList<'a, T> {
    kind: &'a str,
    #[serde(rename = "apiVersion")]
    api_version: &'a str,
    metadata: ListMeta,
    items: Vec<T>,
}

fn pod_out<'a>(snap: &'a Snapshot, pod: &'a PodMetric, gv: Option<&'a str>) -> PodMetricsOut<'a> {
    PodMetricsOut {
        api_version: gv,
        kind: gv.map(|_| "PodMetrics"),
        metadata: Meta {
            name: &pod.name,
            namespace: Some(&pod.namespace),
            creation_timestamp: &snap.timestamp,
        },
        timestamp: &snap.timestamp,
        window: &snap.window,
        containers: pod
            .containers
            .iter()
            .map(|c| ContainerOut {
                name: &c.name,
                usage: Usage {
                    cpu: format_cpu(c.cpu_cores),
                    memory: format_memory(c.memory_bytes),
                },
            })
            .collect(),
    }
}

fn node_out<'a>(
    snap: &'a Snapshot,
    node: &'a NodeMetric,
    gv: Option<&'a str>,
) -> NodeMetricsOut<'a> {
    NodeMetricsOut {
        api_version: gv,
        kind: gv.map(|_| "NodeMetrics"),
        metadata: Meta {
            name: &node.name,
            namespace: None,
            creation_timestamp: &snap.timestamp,
        },
        timestamp: &snap.timestamp,
        window: &snap.window,
        usage: Usage {
            cpu: format_cpu(node.cpu_cores),
            memory: format_memory(node.memory_bytes),
        },
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

#[derive(Deserialize, Default)]
struct ListParams {
    #[serde(rename = "labelSelector")]
    label_selector: Option<String>,
}

/// Validates the version, freshness and the request's label selector in one go.
struct Request {
    group_version: String,
    snapshot: Arc<Snapshot>,
    selector: Vec<Requirement>,
}

fn prepare(state: &AppState, version: &str, params: &ListParams) -> Result<Request, Box<Response>> {
    let Some(group_version) = group_version(version) else {
        return Err(Box::new(k8s_status(
            StatusCode::NOT_FOUND,
            "NotFound",
            format!("the server could not find the requested resource: metrics.k8s.io/{version}"),
        )));
    };
    let Some(snapshot) = state.store.load() else {
        return Err(Box::new(k8s_status(
            StatusCode::SERVICE_UNAVAILABLE,
            "ServiceUnavailable",
            "no metrics have been collected from Prometheus yet",
        )));
    };
    let age = snapshot.age();
    if age > state.max_metrics_age {
        return Err(Box::new(k8s_status(
            StatusCode::SERVICE_UNAVAILABLE,
            "ServiceUnavailable",
            format!("cached metrics are stale ({}s old)", age.as_secs()),
        )));
    }
    let selector = match &params.label_selector {
        Some(raw) if snapshot.labels_available => parse_selector(raw)
            .map_err(|e| Box::new(k8s_status(StatusCode::BAD_REQUEST, "BadRequest", e)))?,
        // Without a label source, filtering would silently hide every pod.
        _ => Vec::new(),
    };
    Ok(Request {
        group_version,
        snapshot,
        selector,
    })
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 253
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.')
}

async fn list_pods(
    State(state): State<AppState>,
    Path(version): Path<String>,
    Query(params): Query<ListParams>,
) -> Response {
    let req = match prepare(&state, &version, &params) {
        Ok(r) => r,
        Err(e) => return *e,
    };
    let items = req
        .snapshot
        .pods
        .values()
        .filter(|p| selector_matches(&req.selector, &p.labels))
        .map(|p| pod_out(&req.snapshot, p, None))
        .collect();
    Json(MetricsList {
        kind: "PodMetricsList",
        api_version: &req.group_version,
        metadata: ListMeta {},
        items,
    })
    .into_response()
}

async fn list_pods_in_ns(
    State(state): State<AppState>,
    Path((version, namespace)): Path<(String, String)>,
    Query(params): Query<ListParams>,
) -> Response {
    let req = match prepare(&state, &version, &params) {
        Ok(r) => r,
        Err(e) => return *e,
    };
    if !valid_name(&namespace) {
        return k8s_status(
            StatusCode::BAD_REQUEST,
            "BadRequest",
            "invalid namespace name",
        );
    }
    let items = req
        .snapshot
        .pods_in_namespace(&namespace)
        .filter(|p| selector_matches(&req.selector, &p.labels))
        .map(|p| pod_out(&req.snapshot, p, None))
        .collect();
    Json(MetricsList {
        kind: "PodMetricsList",
        api_version: &req.group_version,
        metadata: ListMeta {},
        items,
    })
    .into_response()
}

async fn get_pod(
    State(state): State<AppState>,
    Path((version, namespace, name)): Path<(String, String, String)>,
) -> Response {
    let req = match prepare(&state, &version, &ListParams::default()) {
        Ok(r) => r,
        Err(e) => return *e,
    };
    if !valid_name(&namespace) || !valid_name(&name) {
        return k8s_status(
            StatusCode::BAD_REQUEST,
            "BadRequest",
            "invalid resource name",
        );
    }
    match req.snapshot.pods.get(&(namespace, name.clone())) {
        Some(pod) => Json(pod_out(&req.snapshot, pod, Some(&req.group_version))).into_response(),
        None => not_found("pods", &name),
    }
}

async fn list_nodes(
    State(state): State<AppState>,
    Path(version): Path<String>,
    Query(params): Query<ListParams>,
) -> Response {
    let req = match prepare(&state, &version, &params) {
        Ok(r) => r,
        Err(e) => return *e,
    };
    let items = req
        .snapshot
        .nodes
        .values()
        .filter(|n| selector_matches(&req.selector, &n.labels))
        .map(|n| node_out(&req.snapshot, n, None))
        .collect();
    Json(MetricsList {
        kind: "NodeMetricsList",
        api_version: &req.group_version,
        metadata: ListMeta {},
        items,
    })
    .into_response()
}

async fn get_node(
    State(state): State<AppState>,
    Path((version, name)): Path<(String, String)>,
) -> Response {
    let req = match prepare(&state, &version, &ListParams::default()) {
        Ok(r) => r,
        Err(e) => return *e,
    };
    if !valid_name(&name) {
        return k8s_status(
            StatusCode::BAD_REQUEST,
            "BadRequest",
            "invalid resource name",
        );
    }
    match req.snapshot.nodes.get(&name) {
        Some(node) => Json(node_out(&req.snapshot, node, Some(&req.group_version))).into_response(),
        None => not_found("nodes", &name),
    }
}

// ---------------------------------------------------------------------------
// Label selectors (equality and existence only)
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq)]
enum Requirement {
    Eq(String, String),
    Ne(String, String),
    Exists(String),
    NotExists(String),
}

fn parse_selector(raw: &str) -> Result<Vec<Requirement>, String> {
    let mut out = Vec::new();
    for part in raw.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let req = if let Some((k, v)) = part.split_once("!=") {
            Requirement::Ne(sanitize_label_key(k.trim()), v.trim().to_string())
        } else if let Some((k, v)) = part.split_once("==") {
            Requirement::Eq(sanitize_label_key(k.trim()), v.trim().to_string())
        } else if let Some((k, v)) = part.split_once('=') {
            Requirement::Eq(sanitize_label_key(k.trim()), v.trim().to_string())
        } else if let Some(k) = part.strip_prefix('!') {
            Requirement::NotExists(sanitize_label_key(k.trim()))
        } else if part
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-._/".contains(c))
        {
            Requirement::Exists(sanitize_label_key(part))
        } else {
            return Err(format!("unsupported label selector expression: {part:?}"));
        };
        out.push(req);
    }
    Ok(out)
}

fn selector_matches(reqs: &[Requirement], labels: &HashMap<String, String>) -> bool {
    reqs.iter().all(|r| match r {
        Requirement::Eq(k, v) => labels.get(k).is_some_and(|got| got == v),
        Requirement::Ne(k, v) => labels.get(k).is_none_or(|got| got != v),
        Requirement::Exists(k) => labels.contains_key(k),
        Requirement::NotExists(k) => !labels.contains_key(k),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn parses_equality_and_existence_selectors() {
        let reqs = parse_selector("app=web,tier!=db,!canary,ready").unwrap();
        assert_eq!(
            reqs,
            vec![
                Requirement::Eq("app".into(), "web".into()),
                Requirement::Ne("tier".into(), "db".into()),
                Requirement::NotExists("canary".into()),
                Requirement::Exists("ready".into()),
            ]
        );
        assert_eq!(
            parse_selector("app.kubernetes.io/name==web").unwrap(),
            vec![Requirement::Eq(
                "app_kubernetes_io_name".into(),
                "web".into()
            )]
        );
    }

    #[test]
    fn rejects_set_based_selectors() {
        assert!(parse_selector("env in (prod, dev)").is_err());
    }

    #[test]
    fn matches_labels() {
        let l = labels(&[("app", "web"), ("tier", "frontend")]);
        assert!(selector_matches(&parse_selector("app=web").unwrap(), &l));
        assert!(!selector_matches(&parse_selector("app=api").unwrap(), &l));
        assert!(selector_matches(&parse_selector("app!=api").unwrap(), &l));
        assert!(selector_matches(&parse_selector("missing!=x").unwrap(), &l));
        assert!(selector_matches(&parse_selector("app,tier").unwrap(), &l));
        assert!(!selector_matches(
            &parse_selector("app,missing").unwrap(),
            &l
        ));
        assert!(selector_matches(&[], &l));
    }

    #[test]
    fn validates_resource_names() {
        assert!(valid_name("kube-system"));
        assert!(valid_name("web-0.sts"));
        assert!(!valid_name(""));
        assert!(!valid_name("../etc/passwd"));
        assert!(!valid_name("Upper"));
        assert!(!valid_name(&"a".repeat(254)));
    }

    #[test]
    fn only_known_versions_resolve() {
        assert_eq!(group_version("v1").as_deref(), Some("metrics.k8s.io/v1"));
        assert_eq!(
            group_version("v1beta1").as_deref(),
            Some("metrics.k8s.io/v1beta1")
        );
        assert!(group_version("v2").is_none());
    }
}
