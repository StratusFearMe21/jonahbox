//! # yaaxum-error
//! Yet Another Axum Error Handler
//!
//! This crate uses `eyre` to capture the error,
//! the error is then returned to the browser or
//! whatever it is, it's then nicely formatted to
//! a webpage using `ansi_to_html`

use std::fmt::{Debug, Display};

use axum::{
    http::StatusCode,
    response::{Html, IntoResponse},
};
use color_eyre::eyre::eyre;
use reqwest::Response;

pub type Result<T> = std::result::Result<T, Error>;

pub struct Error(StatusCode, color_eyre::eyre::Report);

impl Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.1.handler().display(self.1.as_ref(), f)
    }
}

impl Debug for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.1.handler().debug(self.1.as_ref(), f)
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> axum::response::Response {
        let ansi_string = format!("{:?}", self);
        let error = ansi_to_html::convert(&ansi_string).unwrap();

        (
            self.0,
            Html(format!(
                "<!DOCTYPE html><html><head><meta charset=\"utf8\"></head><body><pre><code>{}</code></pre></body></html>",
                error
            )),
        )
            .into_response()
    }
}

pub trait WithStatusCode<T> {
    fn with_status_code(self, code: StatusCode) -> Result<T>;
}

impl<T> WithStatusCode<T> for std::result::Result<T, color_eyre::eyre::Report> {
    fn with_status_code(self, code: StatusCode) -> Result<T> {
        self.map_err(|e| Error(code, e))
    }
}

pub trait PropogateRequest {
    fn propogate_request_if_err(self) -> Result<Response>;
}

impl PropogateRequest for Response {
    fn propogate_request_if_err(self) -> Result<Response> {
        let status = self.status();
        if status.is_server_error() || status.is_client_error() {
            return Err(eyre!("Reqwest request encountered an issue: {:?}", status))
                .with_status_code(status);
        }
        Ok(self)
    }
}
