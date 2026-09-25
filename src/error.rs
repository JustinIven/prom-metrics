use std::fmt;

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

#[derive(Debug)]
pub enum Error {
    Config(String),
    Prometheus(String),
    Io(std::io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Config(m) => write!(f, "configuration error: {m}"),
            Error::Prometheus(m) => write!(f, "prometheus error: {m}"),
            Error::Io(e) => write!(f, "io error: {e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<reqwest::Error> for Error {
    // Never render the URL: it may carry credentials.
    fn from(e: reqwest::Error) -> Self {
        Error::Prometheus(format!("{}", e.without_url()))
    }
}

/// `metav1.Status`, the error representation every Kubernetes client expects.
pub fn k8s_status(code: StatusCode, reason: &str, message: impl Into<String>) -> Response {
    let body = json!({
        "kind": "Status",
        "apiVersion": "v1",
        "metadata": {},
        "status": "Failure",
        "message": message.into(),
        "reason": reason,
        "code": code.as_u16(),
    });
    (code, Json(body)).into_response()
}

pub fn not_found(resource: &str, name: &str) -> Response {
    k8s_status(
        StatusCode::NOT_FOUND,
        "NotFound",
        format!("{resource}.metrics.k8s.io \"{name}\" not found"),
    )
}
