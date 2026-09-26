use std::{collections::HashMap, time::Duration};

use serde::Deserialize;

use crate::error::Error;

/// One instant-vector sample returned by `/api/v1/query`.
#[derive(Debug, Deserialize)]
pub struct Sample {
    pub metric: HashMap<String, String>,
    /// `[ <unix_time>, "<sample_value>" ]`
    pub value: (f64, String),
}

impl Sample {
    #[must_use]
    pub fn number(&self) -> f64 {
        match self.value.1.parse::<f64>() {
            Ok(v) if v.is_finite() => v,
            _ => 0.0,
        }
    }

    pub fn label(&self, key: &str) -> Option<&str> {
        self.metric
            .get(key)
            .map(String::as_str)
            .filter(|v| !v.is_empty())
    }
}

#[derive(Deserialize)]
struct QueryResponse {
    status: String,
    #[serde(default)]
    data: Option<QueryData>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Deserialize)]
struct QueryData {
    #[serde(default)]
    result: Vec<Sample>,
}

pub struct PromClient {
    http: reqwest::Client,
    query_url: String,
}

impl PromClient {
    /// # Errors
    /// Returns an error if the HTTP client cannot be constructed.
    pub fn new(base_url: &str, timeout: Duration) -> Result<Self, Error> {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .pool_max_idle_per_host(2)
            .user_agent(concat!("prom-metrics/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self {
            http,
            query_url: format!("{}/api/v1/query", base_url.trim_end_matches('/')),
        })
    }

    /// Executes one internally defined `PromQL` query. Never fed by HTTP clients.
    ///
    /// # Errors
    /// Returns an error if the request fails or Prometheus reports an error status.
    pub async fn query(&self, promql: &str) -> Result<Vec<Sample>, Error> {
        let resp = self
            .http
            .post(&self.query_url)
            .form(&[("query", promql)])
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            return Err(Error::Prometheus(format!(
                "query returned HTTP {}",
                status.as_u16()
            )));
        }

        let body: QueryResponse = resp.json().await?;
        if body.status != "success" {
            return Err(Error::Prometheus(
                body.error
                    .unwrap_or_else(|| format!("status {}", body.status)),
            ));
        }
        Ok(body.data.map(|d| d.result).unwrap_or_default())
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;

    fn parse(json: &str) -> Result<Vec<Sample>, String> {
        let body: QueryResponse = serde_json::from_str(json).map_err(|e| e.to_string())?;
        if body.status != "success" {
            return Err(body.error.unwrap_or_default());
        }
        Ok(body.data.map(|d| d.result).unwrap_or_default())
    }

    #[test]
    fn parses_vector_response() {
        let out = parse(
            r#"{"status":"success","data":{"resultType":"vector","result":[
                {"metric":{"namespace":"default","pod":"web","container":"app"},"value":[1700000000.5,"0.25"]}
            ]}}"#,
        )
        .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].label("pod"), Some("web"));
        assert_eq!(out[0].number(), 0.25);
    }

    #[test]
    fn parses_empty_result() {
        assert!(
            parse(r#"{"status":"success","data":{"resultType":"vector","result":[]}}"#)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn propagates_prometheus_error() {
        let err = parse(r#"{"status":"error","errorType":"bad_data","error":"parse error"}"#)
            .unwrap_err();
        assert_eq!(err, "parse error");
    }

    #[test]
    fn handles_non_numeric_and_special_values() {
        let out = parse(
            r#"{"status":"success","data":{"result":[
                {"metric":{"pod":"a"},"value":[1.0,"NaN"]},
                {"metric":{"pod":"b"},"value":[1.0,"+Inf"]},
                {"metric":{"pod":"c"},"value":[1.0,"1e3"]}
            ]}}"#,
        )
        .unwrap();
        assert_eq!(out[0].number(), 0.0);
        assert_eq!(out[1].number(), 0.0);
        assert_eq!(out[2].number(), 1000.0);
    }

    #[test]
    fn empty_labels_are_treated_as_absent() {
        let out = parse(r#"{"status":"success","data":{"result":[{"metric":{"container":""},"value":[1.0,"1"]}]}}"#)
            .unwrap();
        assert_eq!(out[0].label("container"), None);
    }
}
