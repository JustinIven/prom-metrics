use std::{env, net::SocketAddr, time::Duration};

use crate::error::Error;

pub struct Config {
    pub prometheus_url: String,
    pub poll_interval: Duration,
    pub max_metrics_age: Duration,
    /// Range window used for `rate()` and for bounding instant queries.
    /// Must be at least twice the Prometheus scrape interval.
    pub cpu_rate_window: Duration,
    pub listen_addr: SocketAddr,
    pub tls: Option<(String, String)>,
}

const DEFAULT_CERT: &str = "/var/run/serving-cert/tls.crt";
const DEFAULT_KEY: &str = "/var/run/serving-cert/tls.key";

impl Config {
    /// # Errors
    /// Returns an error if a required environment variable is invalid or TLS
    /// material is only partially present.
    pub fn from_env() -> Result<Self, Error> {
        let prometheus_url = var("PROMETHEUS_URL", "http://prometheus:9090")
            .trim_end_matches('/')
            .to_string();
        if !prometheus_url.starts_with("http://") && !prometheus_url.starts_with("https://") {
            return Err(Error::Config(
                "PROMETHEUS_URL must be an http(s) URL".into(),
            ));
        }

        let poll_interval = duration_var("POLL_INTERVAL", "15s")?;
        let max_metrics_age = duration_var("MAX_METRICS_AGE", "60s")?;
        let cpu_rate_window = duration_var("CPU_RATE_WINDOW", "60s")?;
        if poll_interval.is_zero() || cpu_rate_window.is_zero() {
            return Err(Error::Config(
                "POLL_INTERVAL and CPU_RATE_WINDOW must be greater than zero".into(),
            ));
        }

        let listen = var("LISTEN_ADDR", "0.0.0.0:8443");
        let listen_addr = listen.parse().map_err(|_| {
            Error::Config(format!(
                "LISTEN_ADDR is not a valid socket address: {listen}"
            ))
        })?;

        let cert = var("TLS_CERT_FILE", DEFAULT_CERT);
        let key = var("TLS_KEY_FILE", DEFAULT_KEY);
        // TLS is used when the material is actually present, so the same binary can
        // run behind the aggregation layer or plain HTTP locally.
        let tls = match (
            std::path::Path::new(&cert).exists(),
            std::path::Path::new(&key).exists(),
        ) {
            (true, true) => Some((cert, key)),
            (false, false) => {
                if env::var_os("TLS_CERT_FILE").is_some() || env::var_os("TLS_KEY_FILE").is_some() {
                    return Err(Error::Config(format!(
                        "TLS_CERT_FILE/TLS_KEY_FILE not found: {cert}, {key}"
                    )));
                }
                None
            }
            _ => {
                return Err(Error::Config(
                    "TLS_CERT_FILE and TLS_KEY_FILE must both exist".into(),
                ));
            }
        };

        Ok(Self {
            prometheus_url,
            poll_interval,
            max_metrics_age,
            cpu_rate_window,
            listen_addr,
            tls,
        })
    }
}

fn var(key: &str, default: &str) -> String {
    match env::var(key) {
        Ok(v) if !v.trim().is_empty() => v.trim().to_string(),
        _ => default.to_string(),
    }
}

fn duration_var(key: &str, default: &str) -> Result<Duration, Error> {
    let raw = var(key, default);
    parse_duration(&raw).map_err(|e| Error::Config(format!("{key}: {e}")))
}

/// Accepts `500ms`, `15s`, `2m`, `1h` and bare numbers (seconds).
///
/// # Errors
/// Returns an error if the string is not a recognised duration format.
pub fn parse_duration(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    // An if/else-if chain over fixed suffixes reads more clearly here than a
    // nested `map_or_else`.
    #[allow(clippy::option_if_let_else)]
    let (num, unit_ns) = if let Some(v) = s.strip_suffix("ms") {
        (v, 1_000_000u64)
    } else if let Some(v) = s.strip_suffix('s') {
        (v, 1_000_000_000)
    } else if let Some(v) = s.strip_suffix('m') {
        (v, 60_000_000_000)
    } else if let Some(v) = s.strip_suffix('h') {
        (v, 3_600_000_000_000)
    } else {
        (s, 1_000_000_000)
    };
    let value: f64 = num
        .trim()
        .parse()
        .map_err(|_| format!("invalid duration {s:?}"))?;
    if !value.is_finite() || value < 0.0 {
        return Err(format!("invalid duration {s:?}"));
    }
    // No lossless conversion exists between `f64` and `u64`; `value` is
    // checked finite and non-negative immediately above.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::as_conversions
    )]
    let nanos = {
        #[allow(clippy::cast_precision_loss)]
        let unit_ns = unit_ns as f64;
        (value * unit_ns) as u64
    };
    Ok(Duration::from_nanos(nanos))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_durations() {
        assert_eq!(parse_duration("15s").unwrap(), Duration::from_secs(15));
        assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(parse_duration("2m").unwrap(), Duration::from_secs(120));
        assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
        assert_eq!(parse_duration(" 30 ").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("1.5s").unwrap(), Duration::from_millis(1500));
    }

    #[test]
    fn rejects_bad_durations() {
        assert!(parse_duration("").is_err());
        assert!(parse_duration("abc").is_err());
        assert!(parse_duration("-5s").is_err());
        assert!(parse_duration("5 minutes").is_err());
    }
}
