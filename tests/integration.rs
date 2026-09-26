//! End-to-end tests against a mocked Prometheus. No real cluster or Prometheus
//! instance is required.

use std::{collections::HashMap, sync::Arc, time::Duration};

use axum::{Form, Json, Router, routing::post};
use serde_json::{Value, json};

use prom_metrics::{
    api::{AppState, router},
    metrics::{Store, collect},
    prometheus::PromClient,
};

const MI: f64 = 1024.0 * 1024.0;

fn vector(samples: &[Value]) -> Value {
    json!({"status": "success", "data": {"resultType": "vector", "result": samples}})
}

fn sample(labels: &Value, value: f64) -> Value {
    json!({"metric": labels, "value": [1_700_000_000.0, value.to_string()]})
}

async fn prometheus_query(Form(form): Form<HashMap<String, String>>) -> Json<Value> {
    let q = form.get("query").cloned().unwrap_or_default();
    let result = if q.contains("kube_pod_labels") {
        vector(&[
            sample(
                &json!({"namespace": "default", "pod": "web", "label_app": "web"}),
                1.0,
            ),
            sample(
                &json!({"namespace": "default", "pod": "api", "label_app": "api"}),
                1.0,
            ),
        ])
    } else if q.contains("kube_node_labels") {
        vector(&[sample(
            &json!({"node": "node-a", "label_kubernetes_io_os": "linux"}),
            1.0,
        )])
    } else if q.contains("id=\"/\"") && q.contains("rate(") {
        vector(&[
            sample(&json!({"node": "node-a"}), 1.5),
            sample(&json!({"node": "node-b"}), 0.25),
        ])
    } else if q.contains("id=\"/\"") {
        vector(&[
            sample(&json!({"node": "node-a"}), 2.0 * 1024.0 * MI),
            sample(&json!({"node": "node-b"}), 512.0 * MI),
        ])
    } else if q.contains("rate(") {
        vector(&[
            sample(
                &json!({"namespace": "default", "pod": "web", "container": "app"}),
                0.25,
            ),
            sample(
                &json!({"namespace": "default", "pod": "web", "container": "POD"}),
                9.0,
            ),
            sample(
                &json!({"namespace": "default", "pod": "api", "container": "app"}),
                0.0,
            ),
            sample(
                &json!({"namespace": "kube-system", "pod": "dns", "container": "coredns"}),
                0.01,
            ),
        ])
    } else {
        vector(&[
            sample(
                &json!({"namespace": "default", "pod": "web", "container": "app"}),
                64.0 * MI,
            ),
            sample(
                &json!({"namespace": "default", "pod": "web", "container": "sidecar"}),
                8.0 * MI,
            ),
            sample(
                &json!({"namespace": "default", "pod": "api", "container": "app"}),
                0.0,
            ),
            sample(
                &json!({"namespace": "kube-system", "pod": "dns", "container": "coredns"}),
                16.0 * MI,
            ),
        ])
    };
    Json(result)
}

/// Test-only helper: panics on failure are acceptable inside test infrastructure.
#[allow(clippy::unwrap_used)]
async fn serve(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

/// Boots a mock Prometheus, performs one real collection through the
/// Prometheus client, and serves the resulting snapshot over the adapter API.
#[allow(clippy::unwrap_used)]
async fn adapter(max_metrics_age: Duration) -> String {
    let prom = serve(Router::new().route("/api/v1/query", post(prometheus_query))).await;
    let client = PromClient::new(&prom, Duration::from_secs(5)).unwrap();
    let snapshot = collect(&client, Duration::from_secs(60)).await.unwrap();

    let store = Arc::new(Store::default());
    store.replace(snapshot);
    serve(router(AppState {
        store,
        max_metrics_age,
    }))
    .await
}

#[allow(clippy::unwrap_used)]
async fn get(base: &str, path: &str) -> (u16, Value) {
    let resp = reqwest::get(format!("{base}{path}")).await.unwrap();
    let status = resp.status().as_u16();
    let body = resp.json().await.unwrap_or(Value::Null);
    (status, body)
}

#[tokio::test]
async fn serves_discovery_for_both_versions() {
    let base = adapter(Duration::from_secs(60)).await;

    let (status, body) = get(&base, "/apis/metrics.k8s.io").await;
    assert_eq!(status, 200);
    assert_eq!(body["kind"], "APIGroup");
    assert_eq!(body["versions"][0]["version"], "v1");
    assert_eq!(body["versions"][1]["version"], "v1beta1");

    for version in ["v1", "v1beta1"] {
        let (status, body) = get(&base, &format!("/apis/metrics.k8s.io/{version}")).await;
        assert_eq!(status, 200);
        assert_eq!(body["kind"], "APIResourceList");
        assert_eq!(body["groupVersion"], format!("metrics.k8s.io/{version}"));
        assert_eq!(body["resources"][0]["kind"], "NodeMetrics");
        assert_eq!(body["resources"][0]["namespaced"], false);
        assert_eq!(body["resources"][1]["kind"], "PodMetrics");
        assert_eq!(body["resources"][1]["namespaced"], true);
    }

    let (status, _) = get(&base, "/apis/metrics.k8s.io/v2").await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn lists_pods_across_all_namespaces() {
    let base = adapter(Duration::from_secs(60)).await;
    let (status, body) = get(&base, "/apis/metrics.k8s.io/v1/pods").await;

    assert_eq!(status, 200);
    assert_eq!(body["kind"], "PodMetricsList");
    assert_eq!(body["apiVersion"], "metrics.k8s.io/v1");
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 3);

    let web = items
        .iter()
        .find(|i| i["metadata"]["name"] == "web")
        .unwrap();
    assert_eq!(web["metadata"]["namespace"], "default");
    assert_eq!(web["window"], "60s");
    assert!(web["timestamp"].as_str().unwrap().ends_with('Z'));
    // pause container is excluded, memory-only sidecar is kept with zero CPU
    let containers = web["containers"].as_array().unwrap();
    assert_eq!(containers.len(), 2);
    assert_eq!(containers[0]["name"], "app");
    assert_eq!(containers[0]["usage"]["cpu"], "250m");
    assert_eq!(containers[0]["usage"]["memory"], "64Mi");
    assert_eq!(containers[1]["name"], "sidecar");
    assert_eq!(containers[1]["usage"]["cpu"], "0m");
    assert_eq!(containers[1]["usage"]["memory"], "8Mi");

    // idle pod still reports zeroed usage rather than disappearing
    let api = items
        .iter()
        .find(|i| i["metadata"]["name"] == "api")
        .unwrap();
    assert_eq!(api["containers"][0]["usage"]["cpu"], "0m");
    assert_eq!(api["containers"][0]["usage"]["memory"], "0");
}

#[tokio::test]
async fn lists_pods_in_one_namespace() {
    let base = adapter(Duration::from_secs(60)).await;
    let (status, body) = get(&base, "/apis/metrics.k8s.io/v1/namespaces/default/pods").await;

    assert_eq!(status, 200);
    let names: Vec<&str> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["metadata"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["api", "web"]);

    let (status, body) = get(&base, "/apis/metrics.k8s.io/v1/namespaces/nope/pods").await;
    assert_eq!(status, 200);
    assert!(body["items"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn v1beta1_is_semantically_equivalent_to_v1() {
    let base = adapter(Duration::from_secs(60)).await;
    let (status, mut beta) = get(&base, "/apis/metrics.k8s.io/v1beta1/pods").await;
    assert_eq!(status, 200);
    assert_eq!(beta["apiVersion"], "metrics.k8s.io/v1beta1");

    let (_, v1) = get(&base, "/apis/metrics.k8s.io/v1/pods").await;
    beta["apiVersion"] = json!("metrics.k8s.io/v1");
    assert_eq!(beta, v1);
}

#[tokio::test]
async fn gets_single_pod_and_reports_missing_ones() {
    let base = adapter(Duration::from_secs(60)).await;

    let (status, body) = get(
        &base,
        "/apis/metrics.k8s.io/v1beta1/namespaces/default/pods/web",
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body["kind"], "PodMetrics");
    assert_eq!(body["apiVersion"], "metrics.k8s.io/v1beta1");
    assert_eq!(body["metadata"]["name"], "web");

    let (status, body) = get(
        &base,
        "/apis/metrics.k8s.io/v1/namespaces/default/pods/ghost",
    )
    .await;
    assert_eq!(status, 404);
    assert_eq!(body["kind"], "Status");
    assert_eq!(body["reason"], "NotFound");
    assert_eq!(body["code"], 404);

    // a pod that exists in another namespace must not leak
    let (status, _) = get(&base, "/apis/metrics.k8s.io/v1/namespaces/default/pods/dns").await;
    assert_eq!(status, 404);

    let (status, _) = get(
        &base,
        "/apis/metrics.k8s.io/v1/namespaces/default/pods/BAD..name",
    )
    .await;
    assert_eq!(status, 400);
}

#[tokio::test]
async fn lists_and_gets_nodes() {
    let base = adapter(Duration::from_secs(60)).await;

    let (status, body) = get(&base, "/apis/metrics.k8s.io/v1/nodes").await;
    assert_eq!(status, 200);
    assert_eq!(body["kind"], "NodeMetricsList");
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["metadata"]["name"], "node-a");
    assert_eq!(items[0]["usage"]["cpu"], "1500m");
    assert_eq!(items[0]["usage"]["memory"], "2Gi");
    assert!(items[0]["metadata"].get("namespace").is_none());

    let (status, body) = get(&base, "/apis/metrics.k8s.io/v1/nodes/node-b").await;
    assert_eq!(status, 200);
    assert_eq!(body["kind"], "NodeMetrics");
    assert_eq!(body["usage"]["cpu"], "250m");
    assert_eq!(body["usage"]["memory"], "512Mi");

    let (status, body) = get(&base, "/apis/metrics.k8s.io/v1/nodes/node-z").await;
    assert_eq!(status, 404);
    assert_eq!(body["reason"], "NotFound");
}

#[tokio::test]
async fn filters_by_label_selector() {
    let base = adapter(Duration::from_secs(60)).await;

    let (status, body) = get(
        &base,
        "/apis/metrics.k8s.io/v1/namespaces/default/pods?labelSelector=app%3Dweb",
    )
    .await;
    assert_eq!(status, 200);
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["metadata"]["name"], "web");

    let (_, body) = get(
        &base,
        "/apis/metrics.k8s.io/v1/pods?labelSelector=app%3Dnothing",
    )
    .await;
    assert!(body["items"].as_array().unwrap().is_empty());

    let (status, body) = get(
        &base,
        "/apis/metrics.k8s.io/v1/pods?labelSelector=env+in+(a%2Cb)",
    )
    .await;
    assert_eq!(status, 400);
    assert_eq!(body["reason"], "BadRequest");
}

#[tokio::test]
async fn refuses_to_serve_stale_metrics() {
    let base = adapter(Duration::from_millis(1)).await;
    tokio::time::sleep(Duration::from_millis(20)).await;

    for path in [
        "/apis/metrics.k8s.io/v1/pods",
        "/apis/metrics.k8s.io/v1/nodes",
        "/apis/metrics.k8s.io/v1beta1/namespaces/default/pods/web",
    ] {
        let (status, body) = get(&base, path).await;
        assert_eq!(status, 503, "{path}");
        assert_eq!(body["reason"], "ServiceUnavailable");
    }
}

#[tokio::test]
async fn health_endpoints_reflect_snapshot_availability() {
    let store = Arc::new(Store::default());
    let base = serve(router(AppState {
        store: store.clone(),
        max_metrics_age: Duration::from_secs(60),
    }))
    .await;

    let client = reqwest::Client::new();
    assert_eq!(
        client
            .get(format!("{base}/healthz"))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        client
            .get(format!("{base}/readyz"))
            .send()
            .await
            .unwrap()
            .status(),
        503
    );
    let (status, body) = get(&base, "/apis/metrics.k8s.io/v1/pods").await;
    assert_eq!(status, 503);
    assert_eq!(body["reason"], "ServiceUnavailable");

    store.replace(prom_metrics::metrics::build_snapshot(
        prom_metrics::metrics::Inputs::default(),
        Duration::from_secs(60),
    ));
    assert_eq!(
        client
            .get(format!("{base}/readyz"))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );

    let text = client
        .get(format!("{base}/metrics"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(text.contains("metrics_adapter_cached_pods 0"));
    assert!(text.contains("metrics_adapter_prometheus_query_errors_total 0"));
}
