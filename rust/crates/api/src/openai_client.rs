use std::collections::VecDeque;
use std::time::Duration;

use crate::error::ApiError;
use crate::openai_types::{ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse};

const DEFAULT_OPENAI_BASE_URL: &str = "https://api.openai.com";
const DEFAULT_INITIAL_BACKOFF: Duration = Duration::from_millis(200);
const DEFAULT_MAX_BACKOFF: Duration = Duration::from_secs(2);
const DEFAULT_MAX_RETRIES: u32 = 2;

#[derive(Debug, Clone)]
pub struct OpenAiClient {
    http: reqwest::Client,
    bearer_token: String,
    base_url: String,
    account_id: Option<String>,
    max_retries: u32,
    initial_backoff: Duration,
    max_backoff: Duration,
}

impl OpenAiClient {
    #[must_use]
    pub fn new(bearer_token: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            bearer_token: bearer_token.into(),
            base_url: DEFAULT_OPENAI_BASE_URL.to_string(),
            account_id: None,
            max_retries: DEFAULT_MAX_RETRIES,
            initial_backoff: DEFAULT_INITIAL_BACKOFF,
            max_backoff: DEFAULT_MAX_BACKOFF,
        }
    }

    #[must_use]
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    #[must_use]
    pub fn with_account_id(mut self, account_id: String) -> Self {
        if !account_id.is_empty() {
            self.account_id = Some(account_id);
        }
        self
    }

    pub async fn send_chat_completion(
        &self,
        request: &ChatCompletionRequest,
    ) -> Result<ChatCompletionResponse, ApiError> {
        let mut req = request.clone();
        req.stream = false;
        let response = self.send_with_retry(&req).await?;
        let body = response.text().await?;
        serde_json::from_str::<ChatCompletionResponse>(&body).map_err(|error| {
            ApiError::OpenAiApi {
                status: reqwest::StatusCode::OK,
                error_type: Some("parse_error".to_string()),
                message: Some(format!("failed to parse response: {error}")),
                body,
                retryable: false,
            }
        })
    }

    pub async fn stream_chat_completion(
        &self,
        request: &ChatCompletionRequest,
    ) -> Result<OpenAiMessageStream, ApiError> {
        let mut req = request.clone();
        req.stream = true;
        req.stream_options = Some(crate::openai_types::StreamOptions {
            include_usage: true,
        });
        let response = self.send_with_retry(&req).await?;
        Ok(OpenAiMessageStream {
            response,
            buffer: String::new(),
            pending: VecDeque::new(),
            done: false,
        })
    }

    async fn send_with_retry(
        &self,
        request: &ChatCompletionRequest,
    ) -> Result<reqwest::Response, ApiError> {
        let mut last_error: Option<ApiError> = None;
        for attempt in 0..=self.max_retries {
            if attempt > 0 {
                if let Some(ref error) = last_error {
                    if !error.is_retryable() {
                        return Err(last_error.unwrap());
                    }
                }
                let delay = self.backoff_delay(attempt)?;
                tokio::time::sleep(delay).await;
            }
            match self.send_raw_request(request).await {
                Ok(response) => {
                    let status = response.status();
                    if status.is_success() {
                        return Ok(response);
                    }
                    let body = response.text().await.unwrap_or_default();
                    let retryable = matches!(
                        status.as_u16(),
                        408 | 429 | 500 | 502 | 503 | 504
                    );
                    let (error_type, message) = parse_openai_error_body(&body);
                    last_error = Some(ApiError::OpenAiApi {
                        status,
                        error_type,
                        message,
                        body,
                        retryable,
                    });
                }
                Err(error) => {
                    last_error = Some(error);
                }
            }
        }
        Err(ApiError::RetriesExhausted {
            attempts: self.max_retries + 1,
            last_error: Box::new(last_error.unwrap_or(ApiError::MissingOpenAiKey)),
        })
    }

    async fn send_raw_request(
        &self,
        request: &ChatCompletionRequest,
    ) -> Result<reqwest::Response, ApiError> {
        let url = format!("{}/v1/chat/completions", self.base_url);
        let response = self
            .http
            .post(&url)
            .bearer_auth(&self.bearer_token)
            .header("content-type", "application/json")
            .json(request)
            .send()
            .await?;
        Ok(response)
    }

    pub async fn stream_responses(
        &self,
        request: &crate::openai_types::ResponsesRequest,
    ) -> Result<ResponsesStream, ApiError> {
        let mut req = request.clone();
        req.stream = true;
        let response = self.send_responses_raw(&req).await?;
        Ok(ResponsesStream {
            response,
            buffer: String::new(),
            done: false,
        })
    }

    async fn send_responses_raw(
        &self,
        request: &crate::openai_types::ResponsesRequest,
    ) -> Result<reqwest::Response, ApiError> {
        // When account_id is set, route through ChatGPT backend API
        let url = if self.account_id.is_some() {
            "https://chatgpt.com/backend-api/codex/responses".to_string()
        } else {
            format!("{}/v1/responses", self.base_url)
        };
        let mut last_error: Option<ApiError> = None;
        for attempt in 0..=self.max_retries {
            if attempt > 0 {
                if let Some(ref error) = last_error {
                    if !error.is_retryable() {
                        return Err(last_error.unwrap());
                    }
                }
                let delay = self.backoff_delay(attempt)?;
                tokio::time::sleep(delay).await;
            }
            let mut req_builder = self
                .http
                .post(&url)
                .bearer_auth(&self.bearer_token)
                .header("content-type", "application/json");

            // Add ChatGPT backend-specific headers
            if let Some(ref account_id) = self.account_id {
                req_builder = req_builder
                    .header("openai-account-id", account_id)
                    .header("openai-beta", "responses-v1")
                    .header("openai-originator", "codex_cli_rs")
                    .header("User-Agent", "codex-cli/0.116.0");
            }

            match req_builder
                .json(request)
                .send()
                .await
            {
                Ok(response) => {
                    let status = response.status();
                    if status.is_success() {
                        return Ok(response);
                    }
                    let body = response.text().await.unwrap_or_default();
                    let retryable = matches!(status.as_u16(), 408 | 429 | 500 | 502 | 503 | 504);
                    let (error_type, message) = parse_openai_error_body(&body);
                    last_error = Some(ApiError::OpenAiApi {
                        status,
                        error_type,
                        message,
                        body,
                        retryable,
                    });
                }
                Err(error) => {
                    last_error = Some(ApiError::from(error));
                }
            }
        }
        Err(ApiError::RetriesExhausted {
            attempts: self.max_retries + 1,
            last_error: Box::new(last_error.unwrap_or(ApiError::MissingOpenAiKey)),
        })
    }

    fn backoff_delay(&self, attempt: u32) -> Result<Duration, ApiError> {
        let Some(multiplier) = 2_u32.checked_pow(attempt.saturating_sub(1)) else {
            return Err(ApiError::BackoffOverflow {
                attempt,
                base_delay: self.initial_backoff,
            });
        };
        Ok(self
            .initial_backoff
            .checked_mul(multiplier)
            .map_or(self.max_backoff, |delay| delay.min(self.max_backoff)))
    }
}

fn find_sse_frame_boundary(buffer: &str) -> Option<(usize, usize)> {
    let nn = buffer.find("\n\n").map(|p| (p, 2));
    let rn = buffer.find("\r\n\r\n").map(|p| (p, 4));
    [nn, rn].into_iter().flatten().min_by_key(|(p, _)| *p)
}

fn parse_openai_error_body(body: &str) -> (Option<String>, Option<String>) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return (None, None);
    };
    let error = value.get("error");
    let error_type = error
        .and_then(|e| e.get("type"))
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned);
    let message = error
        .and_then(|e| e.get("message"))
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned);
    (error_type, message)
}

pub struct OpenAiMessageStream {
    response: reqwest::Response,
    buffer: String,
    pending: VecDeque<ChatCompletionChunk>,
    done: bool,
}

impl OpenAiMessageStream {
    pub async fn next_chunk(&mut self) -> Result<Option<ChatCompletionChunk>, ApiError> {
        loop {
            if let Some(chunk) = self.pending.pop_front() {
                return Ok(Some(chunk));
            }
            if self.done {
                return Ok(None);
            }
            match self.response.chunk().await? {
                Some(bytes) => {
                    self.buffer
                        .push_str(&String::from_utf8_lossy(&bytes));
                    self.parse_buffered_events()?;
                }
                None => {
                    self.done = true;
                    self.parse_buffered_events()?;
                    return Ok(self.pending.pop_front());
                }
            }
        }
    }

    fn parse_buffered_events(&mut self) -> Result<(), ApiError> {
        while let Some((pos, sep_len)) = find_sse_frame_boundary(&self.buffer) {
            let frame = self.buffer[..pos].to_string();
            self.buffer = self.buffer[pos + sep_len..].to_string();
            for line in frame.lines() {
                if line.starts_with(':') {
                    continue; // SSE comment / keepalive
                }
                let data = if let Some(stripped) = line.strip_prefix("data: ") {
                    stripped.trim()
                } else if let Some(stripped) = line.strip_prefix("data:") {
                    stripped.trim()
                } else {
                    continue;
                };
                if data == "[DONE]" {
                    self.done = true;
                    return Ok(());
                }
                if data.is_empty() {
                    continue;
                }
                match serde_json::from_str::<ChatCompletionChunk>(data) {
                    Ok(chunk) => self.pending.push_back(chunk),
                    Err(_error) => {
                        return Err(ApiError::InvalidSseFrame(
                            "failed to parse OpenAI stream chunk",
                        ));
                    }
                }
            }
        }
        Ok(())
    }
}

pub struct ResponsesStream {
    response: reqwest::Response,
    buffer: String,
    done: bool,
}

impl ResponsesStream {
    /// Returns the next SSE event as a (event_type, json_data) pair.
    /// Returns None when the stream is done.
    pub async fn next_event(&mut self) -> Result<Option<(String, serde_json::Value)>, ApiError> {
        loop {
            if self.done {
                return Ok(None);
            }
            // Try to extract a complete SSE frame from buffer
            if let Some(event) = self.try_parse_next_frame()? {
                return Ok(Some(event));
            }
            // Need more data
            match self.response.chunk().await? {
                Some(bytes) => {
                    self.buffer.push_str(&String::from_utf8_lossy(&bytes));
                }
                None => {
                    self.done = true;
                    // Try to parse any remaining data
                    if let Some(event) = self.try_parse_next_frame()? {
                        return Ok(Some(event));
                    }
                    return Ok(None);
                }
            }
        }
    }

    fn try_parse_next_frame(&mut self) -> Result<Option<(String, serde_json::Value)>, ApiError> {
        // SSE frames are delimited by \n\n or \r\n\r\n
        let Some((pos, sep_len)) = find_sse_frame_boundary(&self.buffer) else {
            return Ok(None);
        };
        let frame = self.buffer[..pos].to_string();
        self.buffer = self.buffer[pos + sep_len..].to_string();

        let mut event_type = String::new();
        let mut data_lines = Vec::new();

        for line in frame.lines() {
            if line.starts_with(':') {
                continue; // SSE comment / keepalive
            }
            if let Some(value) = line.strip_prefix("event: ") {
                event_type = value.trim().to_string();
            } else if let Some(value) = line.strip_prefix("data: ") {
                data_lines.push(value.to_string());
            } else if let Some(value) = line.strip_prefix("data:") {
                data_lines.push(value.to_string());
            }
        }

        if data_lines.is_empty() {
            return Ok(None);
        }

        let data_str = data_lines.join("\n");
        if data_str.trim() == "[DONE]" {
            self.done = true;
            return Ok(None);
        }

        let data: serde_json::Value = serde_json::from_str(&data_str).unwrap_or_else(|_| {
            serde_json::Value::String(data_str)
        });

        Ok(Some((event_type, data)))
    }
}

pub fn read_openai_base_url() -> String {
    std::env::var("OPENAI_BASE_URL").unwrap_or_else(|_| DEFAULT_OPENAI_BASE_URL.to_string())
}
