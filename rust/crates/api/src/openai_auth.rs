use std::fs;
use std::path::PathBuf;

use runtime::{
    load_oauth_credentials_for_provider, save_oauth_credentials_for_provider, OAuthTokenSet,
};

use crate::error::ApiError;

const OPENAI_OAUTH_PROVIDER_KEY: &str = "openai_oauth";
const OPENAI_TOKEN_EXCHANGE_URL: &str = "https://auth.openai.com/oauth/token";
const OPENAI_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

/// Credentials resolved from OpenAI auth sources.
#[derive(Clone)]
pub struct OpenAiCredentials {
    pub access_token: String,
    pub account_id: String,
}

impl std::fmt::Debug for OpenAiCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiCredentials")
            .field("access_token", &"[REDACTED]")
            .field("account_id", &self.account_id)
            .finish()
    }
}

pub fn resolve_openai_auth() -> Result<OpenAiCredentials, ApiError> {
    let mut saved_oauth_error = None;

    // 1. Check OPENAI_API_KEY environment variable
    if let Some(key) = read_env_non_empty("OPENAI_API_KEY")? {
        return Ok(OpenAiCredentials {
            access_token: key,
            account_id: String::new(),
        });
    }

    // 2. Check saved OAuth credentials under "openai_oauth" key
    if let Some(token_set) = load_oauth_credentials_for_provider(OPENAI_OAUTH_PROVIDER_KEY)
        .map_err(ApiError::Io)?
    {
        match resolve_saved_openai_oauth(token_set) {
            Ok(Some(credentials)) => return Ok(credentials),
            Ok(None) => {}
            Err(error) => saved_oauth_error = Some(error),
        }
    }

    // 3. Try reading Codex CLI credentials (ChatGPT OAuth tokens)
    if let Some(creds) = exchange_codex_cli_token()? {
        return Ok(creds);
    }

    if let Some(error) = saved_oauth_error {
        return Err(error);
    }

    Err(ApiError::MissingOpenAiKey)
}

fn resolve_saved_openai_oauth(token_set: OAuthTokenSet) -> Result<Option<OpenAiCredentials>, ApiError> {
    if token_set.access_token.is_empty() {
        return Ok(None);
    }

    if !expires_soon(token_set.expires_at) {
        return Ok(Some(OpenAiCredentials {
            account_id: extract_account_id_from_jwt(&token_set.access_token).unwrap_or_default(),
            access_token: token_set.access_token,
        }));
    }

    let Some(refresh_token) = token_set.refresh_token.clone() else {
        return Ok(None);
    };

    let rt = tokio::runtime::Runtime::new().map_err(|e| ApiError::Io(e.into()))?;
    let refreshed = rt.block_on(async {
        refresh_openai_token_set(&reqwest::Client::new(), &refresh_token, token_set.refresh_token)
            .await
    })?;
    save_oauth_credentials_for_provider(OPENAI_OAUTH_PROVIDER_KEY, &refreshed)
        .map_err(ApiError::Io)?;

    Ok(Some(OpenAiCredentials {
        account_id: extract_account_id_from_jwt(&refreshed.access_token).unwrap_or_default(),
        access_token: refreshed.access_token,
    }))
}

/// Read tokens from ~/.codex/auth.json, refresh the access_token if expired,
/// extract account_id from the JWT, and return credentials for the ChatGPT
/// backend API.
fn exchange_codex_cli_token() -> Result<Option<OpenAiCredentials>, ApiError> {
    let codex_tokens = match read_codex_tokens() {
        Some(tokens) => tokens,
        None => return Ok(None),
    };

    let rt = tokio::runtime::Runtime::new().map_err(|e| ApiError::Io(e.into()))?;
    let creds = rt.block_on(async {
        let client = reqwest::Client::new();

        // Use the access_token directly (this is the ChatGPT OAuth token).
        // Check if it's expired by inspecting the JWT exp claim.
        let access_token = if is_jwt_expired(&codex_tokens.access_token) {
            // Token expired — refresh to get a new access_token
            let Some(ref refresh_token) = codex_tokens.refresh_token else {
                return Err(ApiError::OpenAiApi {
                    status: reqwest::StatusCode::UNAUTHORIZED,
                    error_type: Some("token_expired".to_string()),
                    message: Some(
                        "Codex CLI access_token expired and no refresh_token available. Run `codex login` to re-authenticate.".to_string(),
                    ),
                    body: String::new(),
                    retryable: false,
                });
            };
            refresh_codex_access_token(&client, refresh_token).await?
        } else {
            codex_tokens.access_token.clone()
        };

        // Extract account_id: prefer what's stored in auth.json, fall back to JWT claim
        let account_id = codex_tokens
            .account_id
            .or_else(|| extract_account_id_from_jwt(&access_token))
            .unwrap_or_default();

        Ok(OpenAiCredentials {
            access_token,
            account_id,
        })
    })?;

    Ok(Some(creds))
}

/// Check whether a JWT's `exp` claim is in the past.
fn is_jwt_expired(token: &str) -> bool {
    let Some(payload) = decode_jwt_payload(token) else {
        // Can't decode — treat as expired to force refresh
        return true;
    };
    let Some(exp) = payload.get("exp").and_then(serde_json::Value::as_u64) else {
        return true;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Add a 30-second buffer
    exp < now + 30
}

fn expires_soon(expires_at: Option<u64>) -> bool {
    let Some(exp) = expires_at else {
        return false;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    exp < now + 30
}

/// Extract `chatgpt_account_id` from JWT's `https://api.openai.com/auth` claim.
fn extract_account_id_from_jwt(token: &str) -> Option<String> {
    let payload = decode_jwt_payload(token)?;
    payload
        .get("https://api.openai.com/auth")
        .and_then(|auth| auth.get("chatgpt_account_id"))
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned)
}

/// Decode the payload (middle segment) of a JWT without signature verification.
fn decode_jwt_payload(token: &str) -> Option<serde_json::Value> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let bytes = base64url_decode(parts[1])?;
    serde_json::from_slice(&bytes).ok()
}

/// Decode base64url-encoded data (no external crate needed).
fn base64url_decode(input: &str) -> Option<Vec<u8>> {
    let stripped = input.trim_end_matches('=');
    let padded = match stripped.len() % 4 {
        0 => stripped.to_string(),
        2 => format!("{stripped}=="),
        3 => format!("{stripped}="),
        _ => return None, // 1 is invalid base64
    };
    let standard = padded.replace('-', "+").replace('_', "/");

    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut lookup = [255u8; 256];
    for (i, &b) in TABLE.iter().enumerate() {
        lookup[b as usize] = i as u8;
    }

    let bytes: Vec<u8> = standard.bytes().filter(|&b| b != b'=').collect();
    let mut result = Vec::with_capacity(bytes.len() * 3 / 4);

    for chunk in bytes.chunks(4) {
        let vals: Vec<u8> = chunk.iter().map(|&b| lookup[b as usize]).collect();
        if vals.iter().any(|&v| v == 255) {
            return None;
        }
        match vals.len() {
            4 => {
                result.push((vals[0] << 2) | (vals[1] >> 4));
                result.push((vals[1] << 4) | (vals[2] >> 2));
                result.push((vals[2] << 6) | vals[3]);
            }
            3 => {
                result.push((vals[0] << 2) | (vals[1] >> 4));
                result.push((vals[1] << 4) | (vals[2] >> 2));
            }
            2 => {
                result.push((vals[0] << 2) | (vals[1] >> 4));
            }
            _ => return None,
        }
    }
    Some(result)
}

async fn refresh_codex_access_token(
    client: &reqwest::Client,
    refresh_token: &str,
) -> Result<String, ApiError> {
    let response = client
        .post(OPENAI_TOKEN_EXCHANGE_URL)
        .header("content-type", "application/x-www-form-urlencoded")
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", OPENAI_CLIENT_ID),
            ("refresh_token", refresh_token),
        ])
        .send()
        .await
        .map_err(ApiError::Http)?;

    let status = response.status();
    let body = response.text().await.map_err(ApiError::Http)?;

    if !status.is_success() {
        return Err(ApiError::OpenAiApi {
            status,
            error_type: Some("refresh_failed".to_string()),
            message: Some(format!(
                "failed to refresh Codex CLI token. Run `codex login` to re-authenticate: {body}"
            )),
            body,
            retryable: false,
        });
    }

    let value: serde_json::Value = serde_json::from_str(&body).map_err(ApiError::Json)?;
    value
        .get("access_token")
        .and_then(serde_json::Value::as_str)
        .filter(|t| !t.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| ApiError::OpenAiApi {
            status: reqwest::StatusCode::OK,
            error_type: Some("missing_access_token".to_string()),
            message: Some("refresh response did not contain access_token".to_string()),
            body: body.clone(),
            retryable: false,
        })
}

async fn refresh_openai_token_set(
    client: &reqwest::Client,
    refresh_token: &str,
    existing_refresh_token: Option<String>,
) -> Result<OAuthTokenSet, ApiError> {
    refresh_openai_token_set_with_url(
        client,
        OPENAI_TOKEN_EXCHANGE_URL,
        refresh_token,
        existing_refresh_token,
    )
    .await
}

async fn refresh_openai_token_set_with_url(
    client: &reqwest::Client,
    token_url: &str,
    refresh_token: &str,
    existing_refresh_token: Option<String>,
) -> Result<OAuthTokenSet, ApiError> {
    let response = client
        .post(token_url)
        .header("content-type", "application/x-www-form-urlencoded")
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", OPENAI_CLIENT_ID),
            ("refresh_token", refresh_token),
        ])
        .send()
        .await
        .map_err(ApiError::Http)?;

    let status = response.status();
    let body = response.text().await.map_err(ApiError::Http)?;

    if !status.is_success() {
        return Err(ApiError::OpenAiApi {
            status,
            error_type: Some("refresh_failed".to_string()),
            message: Some(format!("failed to refresh saved OpenAI OAuth token: {body}")),
            body,
            retryable: false,
        });
    }

    let value: serde_json::Value = serde_json::from_str(&body).map_err(ApiError::Json)?;
    let access_token = value
        .get("access_token")
        .and_then(serde_json::Value::as_str)
        .filter(|token| !token.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| ApiError::OpenAiApi {
            status: reqwest::StatusCode::OK,
            error_type: Some("missing_access_token".to_string()),
            message: Some("refresh response did not contain access_token".to_string()),
            body: body.clone(),
            retryable: false,
        })?;

    let refresh_token = value
        .get("refresh_token")
        .and_then(serde_json::Value::as_str)
        .filter(|token| !token.is_empty())
        .map(ToOwned::to_owned)
        .or(existing_refresh_token);

    let expires_at = value
        .get("expires_in")
        .and_then(serde_json::Value::as_u64)
        .map(|seconds| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_secs())
                .unwrap_or(0)
                + seconds
        });

    let scopes = value
        .get("scope")
        .and_then(serde_json::Value::as_str)
        .map(|scope| {
            scope
                .split_whitespace()
                .map(ToOwned::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    Ok(OAuthTokenSet {
        access_token,
        refresh_token,
        expires_at,
        scopes,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        refresh_openai_token_set_with_url, resolve_saved_openai_oauth, OAuthTokenSet,
    };
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;
    use tokio::runtime::Builder;

    fn base64url_encode(bytes: &[u8]) -> String {
        const TABLE: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut output = String::new();
        let mut index = 0;
        while index + 3 <= bytes.len() {
            let block = (u32::from(bytes[index]) << 16)
                | (u32::from(bytes[index + 1]) << 8)
                | u32::from(bytes[index + 2]);
            output.push(TABLE[((block >> 18) & 0x3F) as usize] as char);
            output.push(TABLE[((block >> 12) & 0x3F) as usize] as char);
            output.push(TABLE[((block >> 6) & 0x3F) as usize] as char);
            output.push(TABLE[(block & 0x3F) as usize] as char);
            index += 3;
        }
        match bytes.len().saturating_sub(index) {
            1 => {
                let block = u32::from(bytes[index]) << 16;
                output.push(TABLE[((block >> 18) & 0x3F) as usize] as char);
                output.push(TABLE[((block >> 12) & 0x3F) as usize] as char);
            }
            2 => {
                let block = (u32::from(bytes[index]) << 16) | (u32::from(bytes[index + 1]) << 8);
                output.push(TABLE[((block >> 18) & 0x3F) as usize] as char);
                output.push(TABLE[((block >> 12) & 0x3F) as usize] as char);
                output.push(TABLE[((block >> 6) & 0x3F) as usize] as char);
            }
            _ => {}
        }
        output
    }

    fn sample_jwt(account_id: &str, exp: u64) -> String {
        let header = base64url_encode(br#"{"alg":"none"}"#);
        let payload = base64url_encode(
            format!(
                r#"{{"exp":{exp},"https://api.openai.com/auth":{{"chatgpt_account_id":"{account_id}"}}}}"#
            )
            .as_bytes(),
        );
        format!("{header}.{payload}.signature")
    }

    #[test]
    fn resolves_saved_openai_oauth_with_account_id() {
        let credentials = resolve_saved_openai_oauth(OAuthTokenSet {
            access_token: sample_jwt("acct_test", 4_102_444_800),
            refresh_token: Some("refresh-token".to_string()),
            expires_at: Some(4_102_444_800),
            scopes: vec![],
        })
        .expect("saved oauth should resolve")
        .expect("credentials should exist");

        assert_eq!(credentials.account_id, "acct_test");
    }

    #[test]
    fn refresh_preserves_existing_refresh_token_when_response_omits_it() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind token server");
        let addr = listener.local_addr().expect("local addr");
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buffer = [0_u8; 4096];
            let _ = stream.read(&mut buffer).expect("read request");
            let body = r#"{"access_token":"refreshed-token","expires_in":60,"scope":"model:read model:write"}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body,
            );
            stream
                .write_all(response.as_bytes())
                .expect("write response");
        });

        let runtime = Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let token_set = runtime
            .block_on(async {
                refresh_openai_token_set_with_url(
                    &reqwest::Client::new(),
                    &format!("http://{addr}/oauth/token"),
                    "refresh-token",
                    Some("refresh-token".to_string()),
                )
                .await
            })
            .expect("refresh should succeed");

        handle.join().expect("join token server");
        assert_eq!(token_set.access_token, "refreshed-token");
        assert_eq!(token_set.refresh_token.as_deref(), Some("refresh-token"));
        assert_eq!(token_set.scopes, vec!["model:read", "model:write"]);
        assert!(token_set.expires_at.is_some());
    }
}

struct CodexTokens {
    access_token: String,
    account_id: Option<String>,
    refresh_token: Option<String>,
}

fn read_codex_tokens() -> Option<CodexTokens> {
    let codex_home = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex"))
        })?;
    let auth_path = codex_home.join("auth.json");
    let contents = fs::read_to_string(auth_path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&contents).ok()?;
    let tokens = value.get("tokens")?;
    let access_token = tokens
        .get("access_token")
        .and_then(serde_json::Value::as_str)
        .filter(|t| !t.is_empty())?
        .to_owned();
    let account_id = tokens
        .get("account_id")
        .and_then(serde_json::Value::as_str)
        .filter(|t| !t.is_empty())
        .map(ToOwned::to_owned);
    let refresh_token = tokens
        .get("refresh_token")
        .and_then(serde_json::Value::as_str)
        .filter(|t| !t.is_empty())
        .map(ToOwned::to_owned);
    Some(CodexTokens {
        access_token,
        account_id,
        refresh_token,
    })
}

fn read_env_non_empty(key: &str) -> Result<Option<String>, ApiError> {
    match std::env::var(key) {
        Ok(value) if !value.is_empty() => Ok(Some(value)),
        Ok(_) | Err(std::env::VarError::NotPresent) => Ok(None),
        Err(error) => Err(ApiError::from(error)),
    }
}
