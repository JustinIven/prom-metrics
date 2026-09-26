use std::{
    collections::{BTreeMap, HashMap},
    sync::{
        RwLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use crate::{
    error::Error,
    prometheus::{PromClient, Sample},
};

// ---------------------------------------------------------------------------
// Internal representation (version independent)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct ContainerMetric {
    pub name: String,
    pub cpu_cores: f64,
    pub memory_bytes: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PodMetric {
    pub namespace: String,
    pub name: String,
    pub containers: Vec<ContainerMetric>,
    /// Sanitised kube-state-metrics labels, used only for selector matching.
    pub labels: HashMap<String, String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NodeMetric {
    pub name: String,
    pub cpu_cores: f64,
    pub memory_bytes: f64,
    pub labels: HashMap<String, String>,
}

pub struct Snapshot {
    pub taken_at: Instant,
    pub timestamp: String,
    pub window: String,
    /// Keyed by (namespace, pod) so namespace listing is a range scan.
    pub pods: BTreeMap<(String, String), PodMetric>,
    pub nodes: BTreeMap<String, NodeMetric>,
    /// False when no label source (kube-state-metrics) is available; selector
    /// filtering then fails open instead of returning an empty list.
    pub labels_available: bool,
}

impl Snapshot {
    #[must_use]
    pub fn age(&self) -> Duration {
        self.taken_at.elapsed()
    }

    pub fn pods_in_namespace<'a>(&'a self, ns: &'a str) -> impl Iterator<Item = &'a PodMetric> {
        self.pods
            .range((ns.to_string(), String::new())..)
            .take_while(move |((n, _), _)| n == ns)
            .map(|(_, p)| p)
    }
}

// ---------------------------------------------------------------------------
// Kubernetes quantity formatting
// ---------------------------------------------------------------------------

/// Cores -> Kubernetes CPU quantity in millicores (`250m`).
#[must_use]
pub fn format_cpu(cores: f64) -> String {
    // No lossless conversion exists between `f64` and `u64`; the value is
    // clamped to a finite, non-negative range immediately above.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::as_conversions
    )]
    let milli = if cores.is_finite() && cores > 0.0 {
        (cores * 1000.0).round() as u64
    } else {
        0
    };
    format!("{milli}m")
}

/// Bytes -> Kubernetes memory quantity, using the largest binary suffix that
/// divides exactly so no precision is lost (`64Mi`, `1536Ki`, `1234567`).
#[must_use]
pub fn format_memory(bytes: f64) -> String {
    // No lossless conversion exists between `f64` and `u64`; the value is
    // clamped to a finite, non-negative range immediately above.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::as_conversions
    )]
    let b = if bytes.is_finite() && bytes > 0.0 {
        bytes.round() as u64
    } else {
        0
    };
    // `factor` is one of three non-zero compile-time constants; division and
    // remainder can never panic here.
    #[allow(clippy::arithmetic_side_effects)]
    for (factor, suffix) in [(1u64 << 30, "Gi"), (1 << 20, "Mi"), (1 << 10, "Ki")] {
        if b >= factor && b % factor == 0 {
            return format!("{}{}", b / factor, suffix);
        }
    }
    b.to_string()
}

/// `metav1.Duration` wire format; `time.ParseDuration` accepts plain seconds.
#[must_use]
pub fn format_window(d: Duration) -> String {
    format!("{}s", d.as_secs().max(1))
}

#[must_use]
pub fn rfc3339(t: SystemTime) -> String {
    let secs = t
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    let rem = secs.rem_euclid(86_400);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Howard Hinnant's days-from-civil inverse.
// Bounded integer date arithmetic; cannot realistically overflow or divide by zero.
#[allow(clippy::arithmetic_side_effects)]
const fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

// ---------------------------------------------------------------------------
// Prometheus -> snapshot
// ---------------------------------------------------------------------------

const LABEL_PREFIX: &str = "label_";

/// Builds the six broad queries. Everything the adapter ever asks Prometheus
/// is defined here; nothing is derived from client input.
fn queries(window: Duration) -> [String; 6] {
    let w = format!("{}s", window.as_secs().max(1));
    [
        format!(
            "sum by (namespace, pod, container) (rate(container_cpu_usage_seconds_total{{container!=\"\", container!=\"POD\"}}[{w}]))"
        ),
        format!(
            "sum by (namespace, pod, container) (last_over_time(container_memory_working_set_bytes{{container!=\"\", container!=\"POD\"}}[{w}]))"
        ),
        format!("sum by (node) (rate(container_cpu_usage_seconds_total{{id=\"/\"}}[{w}]))"),
        format!(
            "sum by (node) (last_over_time(container_memory_working_set_bytes{{id=\"/\"}}[{w}]))"
        ),
        format!("last_over_time(kube_pod_labels[{w}])"),
        format!("last_over_time(kube_node_labels[{w}])"),
    ]
}

/// # Errors
/// Returns an error if any of the required Prometheus queries fail.
pub async fn collect(client: &PromClient, window: Duration) -> Result<Snapshot, Error> {
    let [
        q_pod_cpu,
        q_pod_mem,
        q_node_cpu,
        q_node_mem,
        q_pod_labels,
        q_node_labels,
    ] = queries(window);

    let (pod_cpu, pod_mem, node_cpu, node_mem) = tokio::try_join!(
        client.query(&q_pod_cpu),
        client.query(&q_pod_mem),
        client.query(&q_node_cpu),
        client.query(&q_node_mem),
    )?;

    // Label metadata is optional: kube-state-metrics may not be installed.
    let (pod_labels, node_labels) =
        tokio::join!(client.query(&q_pod_labels), client.query(&q_node_labels));

    Ok(build_snapshot(
        Inputs {
            pod_cpu,
            pod_mem,
            node_cpu,
            node_mem,
            pod_labels: pod_labels.unwrap_or_default(),
            node_labels: node_labels.unwrap_or_default(),
        },
        window,
    ))
}

#[derive(Default)]
pub struct Inputs {
    pub pod_cpu: Vec<Sample>,
    pub pod_mem: Vec<Sample>,
    pub node_cpu: Vec<Sample>,
    pub node_mem: Vec<Sample>,
    pub pod_labels: Vec<Sample>,
    pub node_labels: Vec<Sample>,
}

#[must_use]
pub fn build_snapshot(input: Inputs, window: Duration) -> Snapshot {
    let Inputs {
        pod_cpu,
        pod_mem,
        node_cpu,
        node_mem,
        pod_labels,
        node_labels,
    } = input;
    let labels_available = !pod_labels.is_empty() || !node_labels.is_empty();

    let mut containers: BTreeMap<(String, String), BTreeMap<String, (f64, f64)>> = BTreeMap::new();
    for (samples, is_cpu) in [(&pod_cpu, true), (&pod_mem, false)] {
        for s in samples {
            let (Some(ns), Some(pod), Some(container)) =
                (s.label("namespace"), s.label("pod"), s.label("container"))
            else {
                continue; // cannot be tied to a Kubernetes object
            };
            if container == "POD" {
                continue;
            }
            let entry = containers
                .entry((ns.to_string(), pod.to_string()))
                .or_default()
                .entry(container.to_string())
                .or_insert((0.0, 0.0));
            let value = s.number().max(0.0);
            if is_cpu {
                entry.0 = value;
            } else {
                entry.1 = value;
            }
        }
    }

    let mut pod_label_map = extract_labels(&pod_labels, |s| {
        Some((
            s.label("namespace")?.to_string(),
            s.label("pod")?.to_string(),
        ))
    });

    let pods = containers
        .into_iter()
        .map(|(key, cs)| {
            let labels = pod_label_map.remove(&key).unwrap_or_default();
            let metric = PodMetric {
                namespace: key.0.clone(),
                name: key.1.clone(),
                containers: cs
                    .into_iter()
                    .map(|(name, (cpu, mem))| ContainerMetric {
                        name,
                        cpu_cores: cpu,
                        memory_bytes: mem,
                    })
                    .collect(),
                labels,
            };
            (key, metric)
        })
        .collect();

    let mut node_label_map = extract_labels(&node_labels, |s| Some(s.label("node")?.to_string()));
    let mut nodes: BTreeMap<String, NodeMetric> = BTreeMap::new();
    for (samples, is_cpu) in [(&node_cpu, true), (&node_mem, false)] {
        for s in samples {
            let Some(node) = s.label("node") else {
                continue;
            };
            let entry = nodes.entry(node.to_string()).or_insert_with(|| NodeMetric {
                name: node.to_string(),
                cpu_cores: 0.0,
                memory_bytes: 0.0,
                labels: HashMap::new(),
            });
            let value = s.number().max(0.0);
            if is_cpu {
                entry.cpu_cores = value;
            } else {
                entry.memory_bytes = value;
            }
        }
    }
    for (name, node) in &mut nodes {
        node.labels = node_label_map.remove(name).unwrap_or_default();
    }

    Snapshot {
        taken_at: Instant::now(),
        timestamp: rfc3339(SystemTime::now()),
        window: format_window(window),
        pods,
        nodes,
        labels_available,
    }
}

/// Turns kube-state-metrics `label_*` series into a key -> labels map.
/// Keys keep kube-state-metrics' sanitised form; selector keys are sanitised
/// the same way before lookup.
fn extract_labels<K: Ord>(
    samples: &[Sample],
    key_of: impl Fn(&Sample) -> Option<K>,
) -> BTreeMap<K, HashMap<String, String>> {
    let mut out = BTreeMap::new();
    for s in samples {
        let Some(key) = key_of(s) else { continue };
        let labels: HashMap<String, String> = s
            .metric
            .iter()
            .filter_map(|(k, v)| Some((k.strip_prefix(LABEL_PREFIX)?.to_string(), v.clone())))
            .collect();
        out.insert(key, labels);
    }
    out
}

/// kube-state-metrics replaces characters that are invalid in a Prometheus
/// label name with `_`; do the same to selector keys before matching.
#[must_use]
pub fn sanitize_label_key(key: &str) -> String {
    key.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Cache
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct Stats {
    pub query_errors: AtomicU64,
    pub last_duration_micros: AtomicU64,
    pub last_success_unix: AtomicU64,
    pub cached_pods: AtomicU64,
    pub cached_nodes: AtomicU64,
}

#[derive(Default)]
pub struct Store {
    snapshot: RwLock<Option<std::sync::Arc<Snapshot>>>,
    pub stats: Stats,
}

impl Store {
    #[must_use]
    pub fn load(&self) -> Option<std::sync::Arc<Snapshot>> {
        // Recover the value instead of panicking if a prior holder panicked while locked.
        self.snapshot
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Atomically swaps in a freshly built snapshot.
    pub fn replace(&self, snapshot: Snapshot) {
        let pods = u64::try_from(snapshot.pods.len()).unwrap_or(u64::MAX);
        let nodes = u64::try_from(snapshot.nodes.len()).unwrap_or(u64::MAX);
        self.stats.cached_pods.store(pods, Ordering::Relaxed);
        self.stats.cached_nodes.store(nodes, Ordering::Relaxed);
        self.stats.last_success_unix.store(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs()),
            Ordering::Relaxed,
        );
        // Recover the value instead of panicking if a prior holder panicked while locked.
        *self
            .snapshot
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(std::sync::Arc::new(snapshot));
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;

    #[test]
    fn formats_cpu_as_millicores() {
        assert_eq!(format_cpu(0.25), "250m");
        assert_eq!(format_cpu(1.0), "1000m");
        assert_eq!(format_cpu(0.01), "10m");
        assert_eq!(format_cpu(0.0), "0m");
        assert_eq!(format_cpu(0.0004), "0m");
        assert_eq!(format_cpu(-1.0), "0m");
        assert_eq!(format_cpu(f64::NAN), "0m");
    }

    #[test]
    fn formats_memory_with_binary_suffixes() {
        assert_eq!(format_memory(64.0 * 1024.0 * 1024.0), "64Mi");
        assert_eq!(format_memory(2.0 * 1024.0 * 1024.0 * 1024.0), "2Gi");
        assert_eq!(format_memory(1536.0 * 1024.0 * 1024.0), "1536Mi");
        assert_eq!(format_memory(4096.0), "4Ki");
        assert_eq!(format_memory(1_234_567.0), "1234567");
        assert_eq!(format_memory(0.0), "0");
        assert_eq!(format_memory(-5.0), "0");
    }

    #[test]
    fn formats_timestamps() {
        assert_eq!(rfc3339(UNIX_EPOCH), "1970-01-01T00:00:00Z");
        assert_eq!(
            rfc3339(UNIX_EPOCH + Duration::from_secs(1_700_000_000)),
            "2023-11-14T22:13:20Z"
        );
        // leap day
        assert_eq!(
            rfc3339(UNIX_EPOCH + Duration::from_hours(474_768)),
            "2024-02-29T00:00:00Z"
        );
    }

    #[test]
    fn formats_window() {
        assert_eq!(format_window(Duration::from_secs(30)), "30s");
        assert_eq!(format_window(Duration::from_millis(100)), "1s");
    }

    #[test]
    fn sanitizes_label_keys() {
        assert_eq!(
            sanitize_label_key("app.kubernetes.io/name"),
            "app_kubernetes_io_name"
        );
        assert_eq!(sanitize_label_key("app"), "app");
    }

    #[test]
    fn queries_are_broad_and_bounded() {
        let qs = queries(Duration::from_secs(45));
        assert!(qs[0].contains("[45s]"));
        assert!(qs[0].contains("sum by (namespace, pod, container)"));
        assert!(qs[1].contains("last_over_time"));
        assert!(qs[2].contains("id=\"/\""));
    }

    fn sample(labels: &[(&str, &str)], value: f64) -> Sample {
        Sample {
            metric: labels
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            value: (0.0, value.to_string()),
        }
    }

    fn container<'a>(pod: &'a PodMetric, name: &str) -> &'a ContainerMetric {
        pod.containers
            .iter()
            .find(|c| c.name == name)
            .expect("container")
    }

    #[test]
    fn aggregates_pods_and_handles_edge_cases() {
        let mi = 1024.0 * 1024.0;
        let snap = build_snapshot(
            Inputs {
                pod_cpu: vec![
                    sample(
                        &[
                            ("namespace", "default"),
                            ("pod", "web"),
                            ("container", "app"),
                        ],
                        0.25,
                    ),
                    sample(
                        &[
                            ("namespace", "default"),
                            ("pod", "web"),
                            ("container", "sidecar"),
                        ],
                        0.0,
                    ),
                    // pause container must be dropped
                    sample(
                        &[
                            ("namespace", "default"),
                            ("pod", "web"),
                            ("container", "POD"),
                        ],
                        9.0,
                    ),
                    // no container label: cannot be mapped to a Kubernetes object
                    sample(&[("namespace", "default"), ("pod", "web")], 5.0),
                    // unidentifiable series
                    sample(&[("container", "orphan")], 1.0),
                    // memory-only container also appears below
                    sample(
                        &[
                            ("namespace", "kube-system"),
                            ("pod", "dns"),
                            ("container", "coredns"),
                        ],
                        0.002,
                    ),
                ],
                pod_mem: vec![
                    sample(
                        &[
                            ("namespace", "default"),
                            ("pod", "web"),
                            ("container", "app"),
                        ],
                        64.0 * mi,
                    ),
                    // CPU sample missing for this container -> cpu defaults to zero
                    sample(
                        &[
                            ("namespace", "default"),
                            ("pod", "web"),
                            ("container", "init"),
                        ],
                        8.0 * mi,
                    ),
                    sample(
                        &[
                            ("namespace", "kube-system"),
                            ("pod", "dns"),
                            ("container", "coredns"),
                        ],
                        16.0 * mi,
                    ),
                ],
                ..Default::default()
            },
            Duration::from_secs(60),
        );

        assert_eq!(snap.pods.len(), 2);
        let web = &snap.pods[&("default".into(), "web".into())];
        let names: Vec<_> = web.containers.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["app", "init", "sidecar"]);
        assert_eq!(container(web, "app").cpu_cores, 0.25);
        assert_eq!(container(web, "app").memory_bytes, 64.0 * mi);
        assert_eq!(container(web, "init").cpu_cores, 0.0);
        assert_eq!(container(web, "sidecar").memory_bytes, 0.0);
        assert!(!snap.labels_available);
    }

    #[test]
    fn aggregates_nodes_and_ignores_unidentified_series() {
        let snap = build_snapshot(
            Inputs {
                node_cpu: vec![
                    sample(&[("node", "n1")], 1.5),
                    sample(&[("instance", "1.2.3.4")], 9.0),
                ],
                node_mem: vec![
                    sample(&[("node", "n1")], 2.0 * 1024.0 * 1024.0 * 1024.0),
                    sample(&[("node", "n2")], 1024.0),
                ],
                ..Default::default()
            },
            Duration::from_secs(60),
        );
        assert_eq!(snap.nodes.len(), 2);
        assert_eq!(snap.nodes["n1"].cpu_cores, 1.5);
        assert_eq!(format_memory(snap.nodes["n1"].memory_bytes), "2Gi");
        assert_eq!(snap.nodes["n2"].cpu_cores, 0.0);
    }

    #[test]
    fn attaches_kube_state_metrics_labels() {
        let snap = build_snapshot(
            Inputs {
                pod_cpu: vec![sample(
                    &[
                        ("namespace", "default"),
                        ("pod", "web"),
                        ("container", "app"),
                    ],
                    0.1,
                )],
                pod_labels: vec![sample(
                    &[
                        ("namespace", "default"),
                        ("pod", "web"),
                        ("label_app", "web"),
                        ("label_app_kubernetes_io_name", "web"),
                    ],
                    1.0,
                )],
                ..Default::default()
            },
            Duration::from_secs(60),
        );
        let web = &snap.pods[&("default".into(), "web".into())];
        assert_eq!(web.labels["app"], "web");
        assert_eq!(web.labels["app_kubernetes_io_name"], "web");
        assert!(!web.labels.contains_key("namespace"));
        assert!(snap.labels_available);
    }

    #[test]
    fn namespace_range_scan_only_returns_that_namespace() {
        let snap = build_snapshot(
            Inputs {
                pod_cpu: vec![
                    sample(
                        &[("namespace", "a"), ("pod", "p1"), ("container", "c")],
                        0.1,
                    ),
                    sample(
                        &[("namespace", "ab"), ("pod", "p2"), ("container", "c")],
                        0.1,
                    ),
                    sample(
                        &[("namespace", "b"), ("pod", "p3"), ("container", "c")],
                        0.1,
                    ),
                ],
                ..Default::default()
            },
            Duration::from_secs(60),
        );
        let names: Vec<_> = snap
            .pods_in_namespace("a")
            .map(|p| p.name.clone())
            .collect();
        assert_eq!(names, vec!["p1"]);
        assert_eq!(snap.pods_in_namespace("missing").count(), 0);
    }

    #[test]
    fn store_replaces_snapshot_atomically_and_tracks_age() {
        let store = Store::default();
        assert!(store.load().is_none());
        store.replace(build_snapshot(Inputs::default(), Duration::from_secs(60)));
        let snap = store.load().unwrap();
        assert!(snap.age() < Duration::from_secs(1));
        assert_eq!(store.stats.cached_pods.load(Ordering::Relaxed), 0);
    }
}
