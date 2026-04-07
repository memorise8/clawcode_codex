use std::env::VarError;
use std::fmt::{Display, Formatter};
use std::time::Duration;

#[derive(Debug)]
pub enum ApiError {
    MissingApiKey,
    ExpiredOAuthToken,
    Auth(String),
    InvalidApiKeyEnv(VarError),
    Http(reqwest::Error),
    Io(std::io::Error),
    Json(serde_json::Error),
    Api {
        status: reqwest::StatusCode,
        error_type: Option<String>,
        message: Option<String>,
        body: String,
        retryable: bool,
    },
    RetriesExhausted {
        attempts: u32,
        last_error: Box<ApiError>,
    },
    InvalidSseFrame(&'static str),
    BackoffOverflow {
        attempt: u32,
        base_delay: Duration,
    },
    MissingOpenAiKey,
    OpenAiApi {
        status: reqwest::StatusCode,
        error_type: Option<String>,
        message: Option<String>,
        body: String,
        retryable: bool,
    },
}

impl ApiError {
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Http(error) => error.is_connect() || error.is_timeout() || error.is_request(),
            Self::Api { retryable, .. } => *retryable,
            Self::OpenAiApi { retryable, .. } => *retryable,
            Self::RetriesExhausted { last_error, .. } => last_error.is_retryable(),
            Self::MissingApiKey
            | Self::ExpiredOAuthToken
            | Self::Auth(_)
            | Self::InvalidApiKeyEnv(_)
            | Self::Io(_)
            | Self::Json(_)
            | Self::InvalidSseFrame(_)
            | Self::BackoffOverflow { .. }
            | Self::MissingOpenAiKey => false,
        }
    }
}

impl Display for ApiError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingApiKey => {
                write!(
                    f,
                    "ANTHROPIC_AUTH_TOKEN or ANTHROPIC_API_KEY is not set; export one before calling the Anthropic API"
                )
            }
            Self::ExpiredOAuthToken => {
                write!(
                    f,
                    "saved OAuth token is expired and no refresh token is available"
                )
            }
            Self::Auth(message) => write!(f, "auth error: {message}"),
            Self::InvalidApiKeyEnv(error) => {
                write!(
                    f,
                    "failed to read ANTHROPIC_AUTH_TOKEN / ANTHROPIC_API_KEY: {error}"
                )
            }
            Self::Http(error) => write!(f, "http error: {error}"),
            Self::Io(error) => write!(f, "io error: {error}"),
            Self::Json(error) => write!(f, "json error: {error}"),
            Self::Api {
                status,
                error_type,
                message,
                body,
                ..
            } => match (error_type, message) {
                (Some(error_type), Some(message)) => {
                    write!(
                        f,
                        "anthropic api returned {status} ({error_type}): {message}"
                    )
                }
                _ => write!(f, "anthropic api returned {status}: {body}"),
            },
            Self::RetriesExhausted {
                attempts,
                last_error,
            } => write!(
                f,
                "api failed after {attempts} attempts: {last_error}"
            ),
            Self::MissingOpenAiKey => {
                write!(f, "OPENAI_API_KEY is not set and no saved OpenAI OAuth credentials found")
            }
            Self::OpenAiApi { status, error_type, message, body, .. } => match (error_type, message) {
                (Some(error_type), Some(message)) => {
                    write!(f, "openai api returned {status} ({error_type}): {message}")
                }
                _ => write!(f, "openai api returned {status}: {body}"),
            },
            Self::InvalidSseFrame(message) => write!(f, "invalid sse frame: {message}"),
            Self::BackoffOverflow {
                attempt,
                base_delay,
            } => write!(
                f,
                "retry backoff overflowed on attempt {attempt} with base delay {base_delay:?}"
            ),
        }
    }
}

impl std::error::Error for ApiError {}

impl From<reqwest::Error> for ApiError {
    fn from(value: reqwest::Error) -> Self {
        Self::Http(value)
    }
}

impl From<std::io::Error> for ApiError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<serde_json::Error> for ApiError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

impl From<VarError> for ApiError {
    fn from(value: VarError) -> Self {
        Self::InvalidApiKeyEnv(value)
    }
}

#[cfg(test)]
mod tests {
    use super::ApiError;

    #[test]
    fn retries_exhausted_message_is_provider_neutral() {
        let inner = ApiError::Api {
            status: reqwest::StatusCode::TOO_MANY_REQUESTS,
            error_type: Some("rate_limit".to_string()),
            message: Some("slow down".to_string()),
            body: String::new(),
            retryable: true,
        };
        let error = ApiError::RetriesExhausted {
            attempts: 3,
            last_error: Box::new(inner),
        };
        let msg = error.to_string();
        assert!(msg.contains("api failed after 3 attempts"), "got: {msg}");
        assert!(!msg.contains("anthropic api failed"), "should be provider-neutral: {msg}");
    }

    #[test]
    fn openai_api_error_formats_with_type_and_message() {
        let error = ApiError::OpenAiApi {
            status: reqwest::StatusCode::BAD_REQUEST,
            error_type: Some("invalid_request".to_string()),
            message: Some("bad param".to_string()),
            body: String::new(),
            retryable: false,
        };
        let msg = error.to_string();
        assert!(msg.contains("openai api"), "got: {msg}");
        assert!(msg.contains("invalid_request"), "got: {msg}");
        assert!(msg.contains("bad param"), "got: {msg}");
    }

    #[test]
    fn anthropic_api_error_formats_with_type_and_message() {
        let error = ApiError::Api {
            status: reqwest::StatusCode::UNAUTHORIZED,
            error_type: Some("auth_error".to_string()),
            message: Some("invalid key".to_string()),
            body: String::new(),
            retryable: false,
        };
        let msg = error.to_string();
        assert!(msg.contains("anthropic api"), "got: {msg}");
        assert!(msg.contains("auth_error"), "got: {msg}");
    }
}
