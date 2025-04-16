//! Errors.

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};
use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Error, Debug)]
pub enum Error {
    #[error("IO error: {0}")]
    IO(#[from] std::io::Error),

    #[error("GitHub API error: {0}")]
    Api(#[from] opendal::Error),

    #[error("Not Found")]
    NotFound,

    #[error("Bad Request")]
    BadRequest,

    #[error("I/O error: {0}. Context: {1}")]
    Io(std::io::Error, String),

    #[error("GHA cache is disabled")]
    GHADisabled,

    #[error("FlakeHub cache error: {0}")]
    FlakeHub(#[from] anyhow::Error),

    #[error("FlakeHub HTTP error: {0}")]
    FlakeHubHttp(#[from] reqwest::Error),

    #[error("Got HTTP response {0} getting the cache name from FlakeHub: {1}")]
    GetCacheName(reqwest::StatusCode, String),

    #[error("netrc parse error: {0}")]
    Netrc(netrc_rs::Error),

    #[error("Cannot find netrc credentials for {0}")]
    MissingCreds(String),

    #[error("Attic error: {0}")]
    Attic(#[from] attic::AtticError),

    #[error("Bad URL")]
    BadUrl(reqwest::Url),

    #[error("Configuration error: {0}")]
    Config(String),

    #[error("Internal error: {0}")]
    Internal(String),
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let code = match &self {
            Self::Api(err) if err.kind() == opendal::ErrorKind::RateLimited => {
                StatusCode::TOO_MANY_REQUESTS
            }
            Self::Api(err) if err.kind() == opendal::ErrorKind::NotFound => StatusCode::NOT_FOUND,
            // HACK: HTTP 418 makes Nix throw a visible error but not retry
            Self::Api(_) => StatusCode::IM_A_TEAPOT,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::BadRequest => StatusCode::BAD_REQUEST,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };

        (code, format!("{}", self)).into_response()
    }
}
