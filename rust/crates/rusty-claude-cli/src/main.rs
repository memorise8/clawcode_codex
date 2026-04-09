mod init;
mod input;
mod render;

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use api::{
    resolve_startup_auth_source, AnthropicClient, AuthSource, ContentBlockDelta, InputContentBlock,
    InputMessage, MessageRequest, MessageResponse, OutputContentBlock,
    StreamEvent as ApiStreamEvent, ToolChoice, ToolDefinition, ToolResultContentBlock,
};
use api::{
    resolve_openai_auth, read_openai_base_url, OpenAiClient,
    ResponsesRequest, ResponsesInput, ResponsesMessage, ResponsesContent, ResponsesTool,
    ResponsesFunctionCallInput, ResponsesFunctionCallOutputInput,
    ChatCompletionRequest, ChatMessage, ChatTool, ChatFunction, ChatToolChoice,
};

use commands::{
    render_slash_command_help, resume_supported_slash_commands, slash_command_specs, SlashCommand,
};
use compat_harness::{extract_manifest, UpstreamPaths};
use init::initialize_repo;
use render::{Spinner, TerminalRenderer};
use runtime::{
    clear_oauth_credentials, clear_oauth_credentials_for_provider, generate_pkce_pair,
    generate_state, load_system_prompt, parse_oauth_callback_request_target,
    save_oauth_credentials, save_oauth_credentials_for_provider, ApiClient, ApiRequest,
    AssistantEvent, CompactionConfig, ConfigLoader, ConfigSource, ContentBlock,
    ConversationMessage, ConversationRuntime, McpServerManager, MessageRole,
    OAuthAuthorizationRequest, OAuthTokenExchangeRequest, PermissionMode, PermissionPolicy,
    ProjectContext, RuntimeError, Session, TokenUsage, ToolError, ToolExecutor, UsageTracker,
    compact_session, should_compact,
};
use serde_json::json;
use tools::{execute_tool, mvp_tool_specs, ToolSpec};

const DEFAULT_MODEL: &str = "claude-opus-4-6";
const DEFAULT_MAX_TOKENS: u32 = 32;
const DEFAULT_DATE: &str = "2026-03-31";
const DEFAULT_OAUTH_CALLBACK_PORT: u16 = 4545;
const VERSION: &str = env!("CARGO_PKG_VERSION");
const BUILD_TARGET: Option<&str> = option_env!("TARGET");
const GIT_SHA: Option<&str> = option_env!("GIT_SHA");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Provider {
    Anthropic,
    OpenAi,
    Ollama,
}

impl Provider {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "anthropic" | "claude" => Ok(Self::Anthropic),
            "openai" | "codex" => Ok(Self::OpenAi),
            "ollama" | "local" | "gemma" => Ok(Self::Ollama),
            other => Err(format!(
                "unsupported provider: {other} (expected: anthropic, claude, openai, codex, ollama, local, gemma)"
            )),
        }
    }

    fn default_model(&self) -> &'static str {
        match self {
            Self::Anthropic => DEFAULT_MODEL,
            Self::OpenAi => "codex-mini-latest",
            Self::Ollama => "gemma4:31b-it-q4_K_M",
        }
    }
}

type AllowedToolSet = BTreeSet<String>;

/// A tool specification discovered from an MCP server, ready to be sent to the model.
#[derive(Debug, Clone)]
struct McpToolSpec {
    name: String,
    description: String,
    input_schema: serde_json::Value,
}

fn main() {
    if let Err(error) = run() {
        eprintln!(
            "error: {error}

Run `claw --help` for usage."
        );
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().skip(1).collect();
    match parse_args(&args)? {
        CliAction::DumpManifests => dump_manifests(),
        CliAction::BootstrapPlan => print_bootstrap_plan(),
        CliAction::PrintSystemPrompt { cwd, date } => print_system_prompt(cwd, date),
        CliAction::Version => print_version(),
        CliAction::ResumeSession {
            session_path,
            commands,
        } => resume_session(&session_path, &commands),
        CliAction::Prompt {
            prompt,
            model,
            output_format,
            allowed_tools,
            permission_mode,
            provider,
        } => LiveCli::new(model, true, allowed_tools, permission_mode, provider)?
            .run_turn_with_output(&prompt, output_format)?,
        CliAction::Login { provider } => run_login(provider)?,
        CliAction::Logout { provider } => run_logout(provider)?,
        CliAction::Init => run_init()?,
        CliAction::Repl {
            model,
            allowed_tools,
            permission_mode,
            provider,
        } => run_repl(model, allowed_tools, permission_mode, provider)?,
        CliAction::Help => print_help(),
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CliAction {
    DumpManifests,
    BootstrapPlan,
    PrintSystemPrompt {
        cwd: PathBuf,
        date: String,
    },
    Version,
    ResumeSession {
        session_path: PathBuf,
        commands: Vec<String>,
    },
    Prompt {
        prompt: String,
        model: String,
        output_format: CliOutputFormat,
        allowed_tools: Option<AllowedToolSet>,
        permission_mode: PermissionMode,
        provider: Provider,
    },
    Login {
        provider: Provider,
    },
    Logout {
        provider: Provider,
    },
    Init,
    Repl {
        model: String,
        allowed_tools: Option<AllowedToolSet>,
        permission_mode: PermissionMode,
        provider: Provider,
    },
    // prompt-mode formatting is only supported for non-interactive runs
    Help,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CliOutputFormat {
    Text,
    Json,
}

impl CliOutputFormat {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "text" => Ok(Self::Text),
            "json" => Ok(Self::Json),
            other => Err(format!(
                "unsupported value for --output-format: {other} (expected text or json)"
            )),
        }
    }
}

#[allow(clippy::too_many_lines)]
fn parse_args(args: &[String]) -> Result<CliAction, String> {
    let mut model = DEFAULT_MODEL.to_string();
    let mut model_explicitly_set = false;
    let mut provider = Provider::Anthropic;
    let mut output_format = CliOutputFormat::Text;
    let mut permission_mode = default_permission_mode();
    let mut wants_version = false;
    let mut allowed_tool_values = Vec::new();
    let mut rest = Vec::new();
    let mut index = 0;

    while index < args.len() {
        match args[index].as_str() {
            "--version" | "-V" => {
                wants_version = true;
                index += 1;
            }
            "--model" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "missing value for --model".to_string())?;
                model.clone_from(value);
                model_explicitly_set = true;
                index += 2;
            }
            flag if flag.starts_with("--model=") => {
                model = flag[8..].to_string();
                model_explicitly_set = true;
                index += 1;
            }
            "--output-format" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "missing value for --output-format".to_string())?;
                output_format = CliOutputFormat::parse(value)?;
                index += 2;
            }
            "--permission-mode" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "missing value for --permission-mode".to_string())?;
                permission_mode = parse_permission_mode_arg(value)?;
                index += 2;
            }
            flag if flag.starts_with("--output-format=") => {
                output_format = CliOutputFormat::parse(&flag[16..])?;
                index += 1;
            }
            flag if flag.starts_with("--permission-mode=") => {
                permission_mode = parse_permission_mode_arg(&flag[18..])?;
                index += 1;
            }
            "--allowedTools" | "--allowed-tools" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "missing value for --allowedTools".to_string())?;
                allowed_tool_values.push(value.clone());
                index += 2;
            }
            flag if flag.starts_with("--allowedTools=") => {
                allowed_tool_values.push(flag[15..].to_string());
                index += 1;
            }
            flag if flag.starts_with("--allowed-tools=") => {
                allowed_tool_values.push(flag[16..].to_string());
                index += 1;
            }
            "--provider" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "missing value for --provider".to_string())?;
                provider = Provider::parse(value)?;
                index += 2;
            }
            flag if flag.starts_with("--provider=") => {
                provider = Provider::parse(&flag[11..])?;
                index += 1;
            }
            other => {
                rest.push(other.to_string());
                index += 1;
            }
        }
    }

    if !model_explicitly_set {
        model = provider.default_model().to_string();
    }

    if wants_version {
        return Ok(CliAction::Version);
    }

    let allowed_tools = normalize_allowed_tools(&allowed_tool_values)?;

    if rest.is_empty() {
        return Ok(CliAction::Repl {
            model,
            allowed_tools,
            permission_mode,
            provider,
        });
    }
    if matches!(rest.first().map(String::as_str), Some("--help" | "-h")) {
        return Ok(CliAction::Help);
    }
    if rest.first().map(String::as_str) == Some("--resume") {
        return parse_resume_args(&rest[1..]);
    }

    match rest[0].as_str() {
        "dump-manifests" => Ok(CliAction::DumpManifests),
        "bootstrap-plan" => Ok(CliAction::BootstrapPlan),
        "system-prompt" => parse_system_prompt_args(&rest[1..]),
        "login" => Ok(CliAction::Login { provider }),
        "logout" => Ok(CliAction::Logout { provider }),
        "init" => Ok(CliAction::Init),
        "prompt" => {
            let prompt = rest[1..].join(" ");
            if prompt.trim().is_empty() {
                return Err("prompt subcommand requires a prompt string".to_string());
            }
            Ok(CliAction::Prompt {
                prompt,
                model,
                output_format,
                allowed_tools,
                permission_mode,
                provider,
            })
        }
        other if !other.starts_with('/') => Ok(CliAction::Prompt {
            prompt: rest.join(" "),
            model,
            output_format,
            allowed_tools,
            permission_mode,
            provider,
        }),
        other => Err(format!("unknown subcommand: {other}")),
    }
}

fn normalize_allowed_tools(values: &[String]) -> Result<Option<AllowedToolSet>, String> {
    if values.is_empty() {
        return Ok(None);
    }

    let canonical_names = mvp_tool_specs()
        .into_iter()
        .map(|spec| spec.name.to_string())
        .collect::<Vec<_>>();
    let mut name_map = canonical_names
        .iter()
        .map(|name| (normalize_tool_name(name), name.clone()))
        .collect::<BTreeMap<_, _>>();

    for (alias, canonical) in [
        ("read", "read_file"),
        ("write", "write_file"),
        ("edit", "edit_file"),
        ("glob", "glob_search"),
        ("grep", "grep_search"),
    ] {
        name_map.insert(alias.to_string(), canonical.to_string());
    }

    let mut allowed = AllowedToolSet::new();
    for value in values {
        for token in value
            .split(|ch: char| ch == ',' || ch.is_whitespace())
            .filter(|token| !token.is_empty())
        {
            if token.starts_with("mcp__") {
                allowed.insert(token.to_string());
                continue;
            }
            let normalized = normalize_tool_name(token);
            let canonical = name_map.get(&normalized).ok_or_else(|| {
                format!(
                    "unsupported tool in --allowedTools: {token} (expected one of: {} or an exact mcp__server__tool name)",
                    canonical_names.join(", ")
                )
            })?;
            allowed.insert(canonical.clone());
        }
    }

    Ok(Some(allowed))
}

fn normalize_tool_name(value: &str) -> String {
    value.trim().replace('-', "_").to_ascii_lowercase()
}

fn parse_permission_mode_arg(value: &str) -> Result<PermissionMode, String> {
    normalize_permission_mode(value)
        .ok_or_else(|| {
            format!(
                "unsupported permission mode '{value}'. Use read-only, workspace-write, or danger-full-access."
            )
        })
        .map(permission_mode_from_label)
}

fn permission_mode_from_label(mode: &str) -> PermissionMode {
    match mode {
        "read-only" => PermissionMode::ReadOnly,
        "workspace-write" => PermissionMode::WorkspaceWrite,
        "danger-full-access" => PermissionMode::DangerFullAccess,
        other => panic!("unsupported permission mode label: {other}"),
    }
}

fn default_permission_mode() -> PermissionMode {
    env::var("RUSTY_CLAUDE_PERMISSION_MODE")
        .ok()
        .as_deref()
        .and_then(normalize_permission_mode)
        .map_or(PermissionMode::WorkspaceWrite, permission_mode_from_label)
}

fn filter_tool_specs(allowed_tools: Option<&AllowedToolSet>) -> Vec<tools::ToolSpec> {
    mvp_tool_specs()
        .into_iter()
        .filter(|spec| allowed_tools.is_none_or(|allowed| allowed.contains(spec.name)))
        .collect()
}

fn filter_mcp_tools(
    mcp_tools: &[McpToolSpec],
    allowed_tools: Option<&AllowedToolSet>,
) -> Vec<McpToolSpec> {
    mcp_tools
        .iter()
        .filter(|spec| allowed_tools.is_none_or(|allowed| allowed.contains(&spec.name)))
        .cloned()
        .collect()
}

fn child_allowed_tools(allowed_tools: Option<&AllowedToolSet>) -> Option<AllowedToolSet> {
    match allowed_tools {
        Some(allowed) => {
            let mut child = allowed.clone();
            child.remove("Agent");
            child.remove("Skill");
            Some(child)
        }
        None => Some(
            mvp_tool_specs()
                .into_iter()
                .filter(|spec| spec.name != "Agent" && spec.name != "Skill")
                .map(|spec| spec.name.to_string())
                .collect(),
        ),
    }
}

fn parse_system_prompt_args(args: &[String]) -> Result<CliAction, String> {
    let mut cwd = env::current_dir().map_err(|error| error.to_string())?;
    let mut date = DEFAULT_DATE.to_string();
    let mut index = 0;

    while index < args.len() {
        match args[index].as_str() {
            "--cwd" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "missing value for --cwd".to_string())?;
                cwd = PathBuf::from(value);
                index += 2;
            }
            "--date" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| "missing value for --date".to_string())?;
                date.clone_from(value);
                index += 2;
            }
            other => return Err(format!("unknown system-prompt option: {other}")),
        }
    }

    Ok(CliAction::PrintSystemPrompt { cwd, date })
}

fn parse_resume_args(args: &[String]) -> Result<CliAction, String> {
    let session_path = args
        .first()
        .ok_or_else(|| "missing session path for --resume".to_string())
        .map(PathBuf::from)?;
    let commands = args[1..].to_vec();
    if commands
        .iter()
        .any(|command| !command.trim_start().starts_with('/'))
    {
        return Err("--resume trailing arguments must be slash commands".to_string());
    }
    Ok(CliAction::ResumeSession {
        session_path,
        commands,
    })
}

fn dump_manifests() {
    let workspace_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let paths = UpstreamPaths::from_workspace_dir(&workspace_dir);
    match extract_manifest(&paths) {
        Ok(manifest) => {
            println!("commands: {}", manifest.commands.entries().len());
            println!("tools: {}", manifest.tools.entries().len());
            println!("bootstrap phases: {}", manifest.bootstrap.phases().len());
        }
        Err(error) => {
            eprintln!("failed to extract manifests: {error}");
            std::process::exit(1);
        }
    }
}

fn print_bootstrap_plan() {
    for phase in runtime::BootstrapPlan::claude_code_default().phases() {
        println!("- {phase:?}");
    }
}

fn run_login(provider: Provider) -> Result<(), Box<dyn std::error::Error>> {
    match provider {
        Provider::Anthropic => run_anthropic_login(),
        Provider::OpenAi => run_openai_login(),
        Provider::Ollama => {
            println!("Ollama does not require authentication. Ensure ollama is running locally.");
            Ok(())
        }
    }
}

fn run_anthropic_login() -> Result<(), Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    let config = ConfigLoader::default_for(&cwd).load()?;
    let oauth = config.oauth().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "OAuth config is missing. Add settings.oauth.clientId/authorizeUrl/tokenUrl first.",
        )
    })?;
    let callback_port = oauth.callback_port.unwrap_or(DEFAULT_OAUTH_CALLBACK_PORT);
    let redirect_uri = runtime::loopback_redirect_uri(callback_port);
    let pkce = generate_pkce_pair()?;
    let state = generate_state()?;
    let authorize_url =
        OAuthAuthorizationRequest::from_config(oauth, redirect_uri.clone(), state.clone(), &pkce)
            .build_url();

    println!("Starting Claude OAuth login...");
    println!("Listening for callback on {redirect_uri}");
    if let Err(error) = open_browser(&authorize_url) {
        eprintln!("warning: failed to open browser automatically: {error}");
        println!("Open this URL manually:\n{authorize_url}");
    }

    let callback = wait_for_oauth_callback(callback_port, "Claude")?;
    if let Some(error) = callback.error {
        let description = callback
            .error_description
            .unwrap_or_else(|| "authorization failed".to_string());
        return Err(io::Error::other(format!("{error}: {description}")).into());
    }
    let code = callback.code.ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "callback did not include code")
    })?;
    let returned_state = callback.state.ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "callback did not include state")
    })?;
    if returned_state != state {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "oauth state mismatch").into());
    }

    let client = AnthropicClient::from_auth(AuthSource::None).with_base_url(api::read_base_url());
    let exchange_request =
        OAuthTokenExchangeRequest::from_config(oauth, code, state, pkce.verifier, redirect_uri);
    let runtime = tokio::runtime::Runtime::new()?;
    let token_set = runtime.block_on(client.exchange_oauth_code(oauth, &exchange_request))?;
    save_oauth_credentials(&runtime::OAuthTokenSet {
        access_token: token_set.access_token,
        refresh_token: token_set.refresh_token,
        expires_at: token_set.expires_at,
        scopes: token_set.scopes,
    })?;
    println!("Claude OAuth login complete.");
    Ok(())
}

fn run_openai_login() -> Result<(), Box<dyn std::error::Error>> {
    let oauth_config = runtime::OAuthConfig {
        client_id: api::OPENAI_CLIENT_ID.to_string(),
        authorize_url: "https://auth.openai.com/oauth/authorize".to_string(),
        token_url: "https://auth.openai.com/oauth/token".to_string(),
        callback_port: Some(4546),
        manual_redirect_url: None,
        scopes: vec![],
    };
    let callback_port = oauth_config.callback_port.unwrap_or(4546);
    let redirect_uri = runtime::loopback_redirect_uri(callback_port);
    let pkce = generate_pkce_pair()?;
    let state = generate_state()?;
    let authorize_url = OAuthAuthorizationRequest::from_config(
        &oauth_config,
        redirect_uri.clone(),
        state.clone(),
        &pkce,
    )
    .build_url();

    println!("Starting OpenAI Codex OAuth login...");
    println!("Listening for callback on {redirect_uri}");
    if let Err(error) = open_browser(&authorize_url) {
        eprintln!("warning: failed to open browser automatically: {error}");
        println!("Open this URL manually:\n{authorize_url}");
    }

    let callback = wait_for_oauth_callback(callback_port, "OpenAI Codex")?;
    if let Some(error) = callback.error {
        let description = callback
            .error_description
            .unwrap_or_else(|| "authorization failed".to_string());
        return Err(io::Error::other(format!("{error}: {description}")).into());
    }
    let code = callback.code.ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "callback did not include code")
    })?;
    let returned_state = callback.state.ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "callback did not include state")
    })?;
    if returned_state != state {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "oauth state mismatch").into());
    }

    let http = reqwest::Client::new();
    let rt = tokio::runtime::Runtime::new()?;
    let token_response: serde_json::Value = rt.block_on(async {
        let form = [
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", &redirect_uri),
            ("code_verifier", &pkce.verifier),
            ("client_id", &oauth_config.client_id),
        ];
        let response = http.post(&oauth_config.token_url)
            .form(&form)
            .send()
            .await
            .map_err(|e| io::Error::other(e.to_string()))?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(io::Error::other(format!("token exchange failed ({status}): {body}")));
        }
        response.json().await.map_err(|e| io::Error::other(e.to_string()))
    })?;

    let access_token = token_response
        .get("access_token")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "missing access_token in response",
            )
        })?;
    let refresh_token = token_response
        .get("refresh_token")
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned);
    let expires_at = token_response
        .get("expires_in")
        .and_then(serde_json::Value::as_u64)
        .map(|secs| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
                + secs
        });

    save_oauth_credentials_for_provider(
        "openai_oauth",
        &runtime::OAuthTokenSet {
            access_token: access_token.to_string(),
            refresh_token,
            expires_at,
            scopes: vec![],
        },
    )?;
    println!("OpenAI Codex OAuth login complete.");
    Ok(())
}

fn run_logout(provider: Provider) -> Result<(), Box<dyn std::error::Error>> {
    match provider {
        Provider::Anthropic => {
            clear_oauth_credentials()?;
            println!("Claude OAuth credentials cleared.");
        }
        Provider::OpenAi => {
            clear_oauth_credentials_for_provider("openai_oauth")?;
            println!("OpenAI Codex OAuth credentials cleared.");
        }
        Provider::Ollama => {
            println!("Ollama does not use stored credentials. Nothing to clear.");
        }
    }
    Ok(())
}

fn open_browser(url: &str) -> io::Result<()> {
    let commands = if cfg!(target_os = "macos") {
        vec![("open", vec![url])]
    } else if cfg!(target_os = "windows") {
        vec![("cmd", vec!["/C", "start", "", url])]
    } else {
        vec![("xdg-open", vec![url])]
    };
    for (program, args) in commands {
        match Command::new(program).args(args).spawn() {
            Ok(_) => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "no supported browser opener command found",
    ))
}

fn oauth_callback_body(provider_label: &str, is_error: bool) -> String {
    if is_error {
        format!("{provider_label} OAuth login failed. You can close this window.")
    } else {
        format!("{provider_label} OAuth login succeeded. You can close this window.")
    }
}

fn wait_for_oauth_callback(
    port: u16,
    provider_label: &str,
) -> Result<runtime::OAuthCallbackParams, Box<dyn std::error::Error>> {
    let listener = TcpListener::bind(("127.0.0.1", port))?;
    let (mut stream, _) = listener.accept()?;
    let mut buffer = [0_u8; 4096];
    let bytes_read = stream.read(&mut buffer)?;
    let request = String::from_utf8_lossy(&buffer[..bytes_read]);
    let request_line = request.lines().next().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "missing callback request line")
    })?;
    let target = request_line.split_whitespace().nth(1).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "missing callback request target",
        )
    })?;
    let callback = parse_oauth_callback_request_target(target)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let body = oauth_callback_body(provider_label, callback.error.is_some());
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/plain; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    stream.write_all(response.as_bytes())?;
    Ok(callback)
}

fn print_system_prompt(cwd: PathBuf, date: String) {
    match load_system_prompt(cwd, date, env::consts::OS, "unknown") {
        Ok(sections) => println!("{}", sections.join("\n\n")),
        Err(error) => {
            eprintln!("failed to build system prompt: {error}");
            std::process::exit(1);
        }
    }
}

fn print_version() {
    println!("{}", render_version_report());
}

fn resume_session(session_path: &Path, commands: &[String]) {
    let session = match Session::load_from_path(session_path) {
        Ok(session) => session,
        Err(error) => {
            eprintln!("failed to restore session: {error}");
            std::process::exit(1);
        }
    };

    if commands.is_empty() {
        println!(
            "Restored session from {} ({} messages).",
            session_path.display(),
            session.messages.len()
        );
        return;
    }

    let mut session = session;
    for raw_command in commands {
        let Some(command) = SlashCommand::parse(raw_command) else {
            eprintln!("unsupported resumed command: {raw_command}");
            std::process::exit(2);
        };
        match run_resume_command(session_path, &session, &command) {
            Ok(ResumeCommandOutcome {
                session: next_session,
                message,
            }) => {
                session = next_session;
                if let Some(message) = message {
                    println!("{message}");
                }
            }
            Err(error) => {
                eprintln!("{error}");
                std::process::exit(2);
            }
        }
    }
}

#[derive(Debug, Clone)]
struct ResumeCommandOutcome {
    session: Session,
    message: Option<String>,
}

#[derive(Debug, Clone)]
struct StatusContext {
    cwd: PathBuf,
    session_path: Option<PathBuf>,
    loaded_config_files: usize,
    discovered_config_files: usize,
    memory_file_count: usize,
    project_root: Option<PathBuf>,
    git_branch: Option<String>,
}

#[derive(Debug, Clone, Copy)]
struct StatusUsage {
    message_count: usize,
    turns: u32,
    latest: TokenUsage,
    cumulative: TokenUsage,
    estimated_tokens: usize,
}

fn format_model_report(model: &str, message_count: usize, turns: u32) -> String {
    format!(
        "Model
  Current model    {model}
  Session messages {message_count}
  Session turns    {turns}

Usage
  Inspect current model with /model
  Switch models with /model <name>"
    )
}

fn format_model_switch_report(previous: &str, next: &str, message_count: usize) -> String {
    format!(
        "Model updated
  Previous         {previous}
  Current          {next}
  Preserved msgs   {message_count}"
    )
}

fn format_permissions_report(mode: &str) -> String {
    let modes = [
        ("read-only", "Read/search tools only", mode == "read-only"),
        (
            "workspace-write",
            "Edit files inside the workspace",
            mode == "workspace-write",
        ),
        (
            "danger-full-access",
            "Unrestricted tool access",
            mode == "danger-full-access",
        ),
    ]
    .into_iter()
    .map(|(name, description, is_current)| {
        let marker = if is_current {
            "● current"
        } else {
            "○ available"
        };
        format!("  {name:<18} {marker:<11} {description}")
    })
    .collect::<Vec<_>>()
    .join(
        "
",
    );

    format!(
        "Permissions
  Active mode      {mode}
  Mode status      live session default

Modes
{modes}

Usage
  Inspect current mode with /permissions
  Switch modes with /permissions <mode>"
    )
}

fn format_permissions_switch_report(previous: &str, next: &str) -> String {
    format!(
        "Permissions updated
  Result           mode switched
  Previous mode    {previous}
  Active mode      {next}
  Applies to       subsequent tool calls
  Usage            /permissions to inspect current mode"
    )
}

fn format_cost_report(usage: TokenUsage) -> String {
    format!(
        "Cost
  Input tokens     {}
  Output tokens    {}
  Cache create     {}
  Cache read       {}
  Total tokens     {}",
        usage.input_tokens,
        usage.output_tokens,
        usage.cache_creation_input_tokens,
        usage.cache_read_input_tokens,
        usage.total_tokens(),
    )
}

fn format_resume_report(session_path: &str, message_count: usize, turns: u32) -> String {
    format!(
        "Session resumed
  Session file     {session_path}
  Messages         {message_count}
  Turns            {turns}"
    )
}

fn format_compact_report(removed: usize, resulting_messages: usize, skipped: bool) -> String {
    if skipped {
        format!(
            "Compact
  Result           skipped
  Reason           session below compaction threshold
  Messages kept    {resulting_messages}"
        )
    } else {
        format!(
            "Compact
  Result           compacted
  Messages removed {removed}
  Messages kept    {resulting_messages}"
        )
    }
}

fn parse_git_status_metadata(status: Option<&str>) -> (Option<PathBuf>, Option<String>) {
    let Some(status) = status else {
        return (None, None);
    };
    let branch = status.lines().next().and_then(|line| {
        line.strip_prefix("## ")
            .map(|line| {
                line.split(['.', ' '])
                    .next()
                    .unwrap_or_default()
                    .to_string()
            })
            .filter(|value| !value.is_empty())
    });
    let project_root = find_git_root().ok();
    (project_root, branch)
}

fn find_git_root() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(env::current_dir()?)
        .output()?;
    if !output.status.success() {
        return Err("not a git repository".into());
    }
    let path = String::from_utf8(output.stdout)?.trim().to_string();
    if path.is_empty() {
        return Err("empty git root".into());
    }
    Ok(PathBuf::from(path))
}

#[allow(clippy::too_many_lines)]
fn run_resume_command(
    session_path: &Path,
    session: &Session,
    command: &SlashCommand,
) -> Result<ResumeCommandOutcome, Box<dyn std::error::Error>> {
    match command {
        SlashCommand::Help => Ok(ResumeCommandOutcome {
            session: session.clone(),
            message: Some(render_repl_help()),
        }),
        SlashCommand::Compact => {
            let result = runtime::compact_session(
                session,
                CompactionConfig {
                    max_estimated_tokens: 0,
                    ..CompactionConfig::default()
                },
            );
            let removed = result.removed_message_count;
            let kept = result.compacted_session.messages.len();
            let skipped = removed == 0;
            result.compacted_session.save_to_path(session_path)?;
            Ok(ResumeCommandOutcome {
                session: result.compacted_session,
                message: Some(format_compact_report(removed, kept, skipped)),
            })
        }
        SlashCommand::Clear { confirm } => {
            if !confirm {
                return Ok(ResumeCommandOutcome {
                    session: session.clone(),
                    message: Some(
                        "clear: confirmation required; rerun with /clear --confirm".to_string(),
                    ),
                });
            }
            let cleared = Session::new();
            cleared.save_to_path(session_path)?;
            Ok(ResumeCommandOutcome {
                session: cleared,
                message: Some(format!(
                    "Cleared resumed session file {}.",
                    session_path.display()
                )),
            })
        }
        SlashCommand::Status => {
            let tracker = UsageTracker::from_session(session);
            let usage = tracker.cumulative_usage();
            Ok(ResumeCommandOutcome {
                session: session.clone(),
                message: Some(format_status_report(
                    "restored-session",
                    StatusUsage {
                        message_count: session.messages.len(),
                        turns: tracker.turns(),
                        latest: tracker.current_turn_usage(),
                        cumulative: usage,
                        estimated_tokens: 0,
                    },
                    default_permission_mode().as_str(),
                    &status_context(Some(session_path))?,
                )),
            })
        }
        SlashCommand::Cost => {
            let usage = UsageTracker::from_session(session).cumulative_usage();
            Ok(ResumeCommandOutcome {
                session: session.clone(),
                message: Some(format_cost_report(usage)),
            })
        }
        SlashCommand::Config { section, .. } => Ok(ResumeCommandOutcome {
            session: session.clone(),
            message: Some(render_config_report(section.as_deref())?),
        }),
        SlashCommand::Memory => Ok(ResumeCommandOutcome {
            session: session.clone(),
            message: Some(render_memory_report()?),
        }),
        SlashCommand::Init => Ok(ResumeCommandOutcome {
            session: session.clone(),
            message: Some(init_claude_md()?),
        }),
        SlashCommand::Diff => Ok(ResumeCommandOutcome {
            session: session.clone(),
            message: Some(render_diff_report()?),
        }),
        SlashCommand::Version => Ok(ResumeCommandOutcome {
            session: session.clone(),
            message: Some(render_version_report()),
        }),
        SlashCommand::Export { path } => {
            let export_path = resolve_export_path(path.as_deref(), session)?;
            fs::write(&export_path, render_export_text(session))?;
            Ok(ResumeCommandOutcome {
                session: session.clone(),
                message: Some(format!(
                    "Export\n  Result           wrote transcript\n  File             {}\n  Messages         {}",
                    export_path.display(),
                    session.messages.len(),
                )),
            })
        }
        SlashCommand::Resume { .. }
        | SlashCommand::Model { .. }
        | SlashCommand::Permissions { .. }
        | SlashCommand::Session { .. }
        | SlashCommand::Commit
        | SlashCommand::Pr
        | SlashCommand::Unknown(_) => Err("unsupported resumed slash command".into()),
    }
}

fn run_repl(
    model: String,
    allowed_tools: Option<AllowedToolSet>,
    permission_mode: PermissionMode,
    provider: Provider,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut cli = LiveCli::new(model, true, allowed_tools, permission_mode, provider)?;
    let mut editor = input::LineEditor::new("> ", slash_command_completion_candidates());
    println!("{}", cli.startup_banner());

    loop {
        match editor.read_line()? {
            input::ReadOutcome::Submit(input) => {
                let trimmed = input.trim().to_string();
                if trimmed.is_empty() {
                    continue;
                }
                if matches!(trimmed.as_str(), "/exit" | "/quit") {
                    cli.persist_session()?;
                    break;
                }
                if let Some(command) = SlashCommand::parse(&trimmed) {
                    if cli.handle_repl_command(command)? {
                        cli.persist_session()?;
                    }
                    continue;
                }
                editor.push_history(input);
                let expanded = expand_file_references(&trimmed);
                cli.run_turn(&expanded)?;
                // Auto-compact if conversation is getting long
                let _ = cli.try_auto_compact();
            }
            input::ReadOutcome::Cancel => {}
            input::ReadOutcome::Exit => {
                cli.persist_session()?;
                break;
            }
        }
    }

    cli.hooks.run_stop();
    Ok(())
}

#[derive(Debug, Clone)]
struct SessionHandle {
    id: String,
    path: PathBuf,
}

#[derive(Debug, Clone)]
struct ManagedSessionSummary {
    id: String,
    path: PathBuf,
    modified_epoch_secs: u64,
    message_count: usize,
}

struct LiveCli {
    model: String,
    allowed_tools: Option<AllowedToolSet>,
    permission_mode: PermissionMode,
    provider: Provider,
    system_prompt: Vec<String>,
    runtime: ConversationRuntime<ProviderClient, CliToolExecutor>,
    session: SessionHandle,
    hooks: runtime::HookRunner,
    mcp_tools: Vec<McpToolSpec>,
    mcp_manager: Option<Arc<Mutex<McpServerManager>>>,
}

impl LiveCli {
    fn new(
        model: String,
        enable_tools: bool,
        allowed_tools: Option<AllowedToolSet>,
        permission_mode: PermissionMode,
        provider: Provider,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let system_prompt = build_system_prompt()?;
        let session = create_managed_session_handle()?;
        let cwd = env::current_dir().unwrap_or_default();
        let config = ConfigLoader::default_for(&cwd)
            .load()
            .unwrap_or_else(|_| runtime::RuntimeConfig::empty());
        let hooks = runtime::HookRunner::from_config_value(config.get("hooks"));

        // Initialize MCP servers from config
        let (mcp_tools, mcp_manager) = initialize_mcp_servers(&config);

        let runtime = build_runtime(
            Session::new(),
            model.clone(),
            system_prompt.clone(),
            enable_tools,
            allowed_tools.clone(),
            permission_mode,
            provider,
            hooks.clone(),
            mcp_tools.clone(),
            mcp_manager.clone(),
        )?;
        let cli = Self {
            model,
            allowed_tools,
            permission_mode,
            provider,
            system_prompt,
            runtime,
            session,
            hooks,
            mcp_tools,
            mcp_manager,
        };
        cli.persist_session()?;
        Ok(cli)
    }

    fn startup_banner(&self) -> String {
        let cwd = env::current_dir().map_or_else(
            |_| "<unknown>".to_string(),
            |path| path.display().to_string(),
        );
        let provider_label = match self.provider {
            Provider::Anthropic => "Anthropic",
            Provider::OpenAi => "OpenAI Codex",
            Provider::Ollama => "Ollama (local)",
        };
        let mcp_line = if self.mcp_tools.is_empty() {
            String::new()
        } else {
            format!(
                "\n  \x1b[2mMCP tools\x1b[0m        {} tool(s)",
                self.mcp_tools.len()
            )
        };
        format!(
            "\x1b[38;5;196m\
 ██████╗██╗      █████╗ ██╗    ██╗\n\
██╔════╝██║     ██╔══██╗██║    ██║\n\
██║     ██║     ███████║██║ █╗ ██║\n\
██║     ██║     ██╔══██║██║███╗██║\n\
╚██████╗███████╗██║  ██║╚███╔███╔╝\n\
 ╚═════╝╚══════╝╚═╝  ╚═╝ ╚══╝╚══╝\x1b[0m \x1b[38;5;208mCode\x1b[0m 🦞\n\n\
  \x1b[2mModel\x1b[0m            {}\n\
  \x1b[2mProvider\x1b[0m         {}\n\
  \x1b[2mPermissions\x1b[0m      {}\n\
  \x1b[2mDirectory\x1b[0m        {}\n\
  \x1b[2mSession\x1b[0m          {}{}\n\n\
  Type \x1b[1m/help\x1b[0m for commands · \x1b[2mShift+Enter\x1b[0m for newline",
            self.model,
            provider_label,
            self.permission_mode.as_str(),
            cwd,
            self.session.id,
            mcp_line,
        )
    }

    fn run_turn(&mut self, input: &str) -> Result<(), Box<dyn std::error::Error>> {
        let mut spinner = Spinner::new();
        let mut stdout = io::stdout();
        spinner.tick(
            "🦀 Thinking...",
            TerminalRenderer::new().color_theme(),
            &mut stdout,
        )?;
        let mut permission_prompter = CliPermissionPrompter::new(self.permission_mode);
        let result = self.runtime.run_turn(input, Some(&mut permission_prompter));
        match result {
            Ok(_) => {
                spinner.finish(
                    "✨ Done",
                    TerminalRenderer::new().color_theme(),
                    &mut stdout,
                )?;
                write!(stdout, "\x07")?; // terminal bell
                println!();
                self.persist_session()?;
                Ok(())
            }
            Err(error) => {
                spinner.fail(
                    "❌ Request failed",
                    TerminalRenderer::new().color_theme(),
                    &mut stdout,
                )?;
                write!(stdout, "\x07")?; // terminal bell
                Err(Box::new(error))
            }
        }
    }

    fn run_turn_with_output(
        &mut self,
        input: &str,
        output_format: CliOutputFormat,
    ) -> Result<(), Box<dyn std::error::Error>> {
        match output_format {
            CliOutputFormat::Text => self.run_turn(input),
            CliOutputFormat::Json => self.run_prompt_json(input),
        }
    }

    fn run_prompt_json(&mut self, input: &str) -> Result<(), Box<dyn std::error::Error>> {
        match self.provider {
            Provider::Anthropic => self.run_prompt_json_anthropic(input),
            Provider::OpenAi => self.run_prompt_json_openai(input),
            Provider::Ollama => self.run_prompt_json_ollama(input),
        }
    }

    fn run_prompt_json_anthropic(&mut self, input: &str) -> Result<(), Box<dyn std::error::Error>> {
        let client = AnthropicClient::from_auth(resolve_cli_auth_source()?).with_base_url(api::read_base_url());
        let request = MessageRequest {
            model: self.model.clone(),
            max_tokens: DEFAULT_MAX_TOKENS,
            messages: vec![InputMessage {
                role: "user".to_string(),
                content: vec![InputContentBlock::Text {
                    text: input.to_string(),
                }],
            }],
            system: (!self.system_prompt.is_empty()).then(|| self.system_prompt.join("\n\n")),
            tools: None,
            tool_choice: None,
            stream: false,
        };
        let runtime = tokio::runtime::Runtime::new()?;
        let response = runtime.block_on(client.send_message(&request))?;
        let text = response
            .content
            .iter()
            .filter_map(|block| match block {
                OutputContentBlock::Text { text } => Some(text.as_str()),
                OutputContentBlock::ToolUse { .. } => None,
            })
            .collect::<Vec<_>>()
            .join("");
        println!(
            "{}",
            json!({
                "message": text,
                "model": self.model,
                "usage": {
                    "input_tokens": response.usage.input_tokens,
                    "output_tokens": response.usage.output_tokens,
                    "cache_creation_input_tokens": response.usage.cache_creation_input_tokens,
                    "cache_read_input_tokens": response.usage.cache_read_input_tokens,
                }
            })
        );
        Ok(())
    }

    fn run_prompt_json_openai(&mut self, input: &str) -> Result<(), Box<dyn std::error::Error>> {
        let creds = resolve_openai_auth()?;
        let client = OpenAiClient::new(&creds.access_token)
            .with_base_url(read_openai_base_url())
            .with_account_id(creds.account_id);
        let request = ResponsesRequest {
            model: self.model.clone(),
            input: vec![ResponsesInput::Text(input.to_string())],
            max_output_tokens: None,
            tools: None,
            tool_choice: None,
            instructions: (!self.system_prompt.is_empty()).then(|| self.system_prompt.join("\n\n")),
            stream: true,
            store: Some(false),
            include: None,
        };
        let runtime = tokio::runtime::Runtime::new()?;
        let (text, input_tokens, output_tokens) = runtime.block_on(async {
            let mut stream = client.stream_responses(&request).await?;
            let mut text = String::new();
            let mut input_tokens = 0u32;
            let mut output_tokens = 0u32;
            while let Some((event_type, data)) = stream.next_event().await? {
                match event_type.as_str() {
                    "response.output_text.delta" => {
                        if let Some(delta) = data.get("delta").and_then(|d| d.as_str()) {
                            text.push_str(delta);
                        }
                    }
                    "response.completed" => {
                        if let Some(response) = data.get("response") {
                            if let Some(usage) = response.get("usage") {
                                input_tokens = usage.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                                output_tokens = usage.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                            }
                        }
                    }
                    _ => {}
                }
            }
            Ok::<_, api::ApiError>((text, input_tokens, output_tokens))
        })?;
        println!(
            "{}",
            json!({
                "message": text,
                "model": self.model,
                "usage": {
                    "input_tokens": input_tokens,
                    "output_tokens": output_tokens,
                }
            })
        );
        Ok(())
    }

    fn run_prompt_json_ollama(&mut self, input: &str) -> Result<(), Box<dyn std::error::Error>> {
        let base_url = env::var("OLLAMA_BASE_URL")
            .unwrap_or_else(|_| "http://localhost:11434".to_string());
        let client = OpenAiClient::new("ollama").with_base_url(base_url);
        let request = ChatCompletionRequest {
            model: self.model.clone(),
            messages: vec![ChatMessage {
                role: "user".to_string(),
                content: Some(input.to_string()),
                tool_calls: None,
                tool_call_id: None,
            }],
            tools: None,
            tool_choice: None,
            stream: false,
            max_tokens: None,
            stream_options: None,
        };
        let runtime = tokio::runtime::Runtime::new()?;
        let response = runtime.block_on(client.send_chat_completion(&request))?;
        let text = response
            .choices
            .first()
            .and_then(|c| c.message.content.as_deref())
            .unwrap_or("");
        let (input_tokens, output_tokens) = response
            .usage
            .map(|u| (u.prompt_tokens, u.completion_tokens))
            .unwrap_or((0, 0));
        println!(
            "{}",
            json!({
                "message": text,
                "model": self.model,
                "usage": {
                    "input_tokens": input_tokens,
                    "output_tokens": output_tokens,
                }
            })
        );
        Ok(())
    }

    fn handle_repl_command(
        &mut self,
        command: SlashCommand,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        Ok(match command {
            SlashCommand::Help => {
                println!("{}", render_repl_help());
                false
            }
            SlashCommand::Status => {
                self.print_status();
                false
            }
            SlashCommand::Compact => {
                self.compact()?;
                false
            }
            SlashCommand::Model { model } => self.set_model(model)?,
            SlashCommand::Permissions { mode } => self.set_permissions(mode)?,
            SlashCommand::Clear { confirm } => self.clear_session(confirm)?,
            SlashCommand::Cost => {
                self.print_cost();
                false
            }
            SlashCommand::Resume { session_path } => self.resume_session(session_path)?,
            SlashCommand::Config { section, set_key, set_value } => {
                if let (Some(key), Some(value)) = (set_key, set_value) {
                    match execute_tool("Config", &serde_json::json!({
                        "setting": key,
                        "value": value,
                    })) {
                        Ok(output) => println!("{output}"),
                        Err(error) => eprintln!("config set failed: {error}"),
                    }
                } else {
                    Self::print_config(section.as_deref())?;
                }
                false
            }
            SlashCommand::Memory => {
                Self::print_memory()?;
                false
            }
            SlashCommand::Init => {
                run_init()?;
                false
            }
            SlashCommand::Diff => {
                Self::print_diff()?;
                false
            }
            SlashCommand::Version => {
                Self::print_version();
                false
            }
            SlashCommand::Export { path } => {
                self.export_session(path.as_deref())?;
                false
            }
            SlashCommand::Session { action, target } => {
                self.handle_session_command(action.as_deref(), target.as_deref())?
            }
            SlashCommand::Commit => {
                self.run_turn("Review the current git diff and create an appropriate git commit. Use `git add` for relevant files and `git commit` with a good commit message following conventional commits format.")?;
                true
            }
            SlashCommand::Pr => {
                self.run_turn("Create a pull request for the current branch. Use `gh pr create` with an appropriate title and description based on the commits. If not on a feature branch, suggest creating one first.")?;
                true
            }
            SlashCommand::Unknown(name) => {
                eprintln!("unknown slash command: /{name}");
                false
            }
        })
    }

    fn persist_session(&self) -> Result<(), Box<dyn std::error::Error>> {
        self.runtime.session().save_to_path(&self.session.path)?;
        Ok(())
    }

    fn print_status(&self) {
        let cumulative = self.runtime.usage().cumulative_usage();
        let latest = self.runtime.usage().current_turn_usage();
        println!(
            "{}",
            format_status_report(
                &self.model,
                StatusUsage {
                    message_count: self.runtime.session().messages.len(),
                    turns: self.runtime.usage().turns(),
                    latest,
                    cumulative,
                    estimated_tokens: self.runtime.estimated_tokens(),
                },
                self.permission_mode.as_str(),
                &status_context(Some(&self.session.path)).expect("status context should load"),
            )
        );
    }

    fn set_model(&mut self, model: Option<String>) -> Result<bool, Box<dyn std::error::Error>> {
        let Some(model) = model else {
            println!(
                "{}",
                format_model_report(
                    &self.model,
                    self.runtime.session().messages.len(),
                    self.runtime.usage().turns(),
                )
            );
            return Ok(false);
        };

        if model == self.model {
            println!(
                "{}",
                format_model_report(
                    &self.model,
                    self.runtime.session().messages.len(),
                    self.runtime.usage().turns(),
                )
            );
            return Ok(false);
        }

        let previous = self.model.clone();
        let session = self.runtime.session().clone();
        let message_count = session.messages.len();
        self.runtime = build_runtime(
            session,
            model.clone(),
            self.system_prompt.clone(),
            true,
            self.allowed_tools.clone(),
            self.permission_mode,
            self.provider,
            self.hooks.clone(),
            self.mcp_tools.clone(),
            self.mcp_manager.clone(),
        )?;
        self.model.clone_from(&model);
        println!(
            "{}",
            format_model_switch_report(&previous, &model, message_count)
        );
        Ok(true)
    }

    fn set_permissions(
        &mut self,
        mode: Option<String>,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        let Some(mode) = mode else {
            println!(
                "{}",
                format_permissions_report(self.permission_mode.as_str())
            );
            return Ok(false);
        };

        let normalized = normalize_permission_mode(&mode).ok_or_else(|| {
            format!(
                "unsupported permission mode '{mode}'. Use read-only, workspace-write, or danger-full-access."
            )
        })?;

        if normalized == self.permission_mode.as_str() {
            println!("{}", format_permissions_report(normalized));
            return Ok(false);
        }

        let previous = self.permission_mode.as_str().to_string();
        let session = self.runtime.session().clone();
        self.permission_mode = permission_mode_from_label(normalized);
        self.runtime = build_runtime(
            session,
            self.model.clone(),
            self.system_prompt.clone(),
            true,
            self.allowed_tools.clone(),
            self.permission_mode,
            self.provider,
            self.hooks.clone(),
            self.mcp_tools.clone(),
            self.mcp_manager.clone(),
        )?;
        println!(
            "{}",
            format_permissions_switch_report(&previous, normalized)
        );
        Ok(true)
    }

    fn clear_session(&mut self, confirm: bool) -> Result<bool, Box<dyn std::error::Error>> {
        if !confirm {
            println!(
                "clear: confirmation required; run /clear --confirm to start a fresh session."
            );
            return Ok(false);
        }

        self.session = create_managed_session_handle()?;
        self.runtime = build_runtime(
            Session::new(),
            self.model.clone(),
            self.system_prompt.clone(),
            true,
            self.allowed_tools.clone(),
            self.permission_mode,
            self.provider,
            self.hooks.clone(),
            self.mcp_tools.clone(),
            self.mcp_manager.clone(),
        )?;
        println!(
            "Session cleared\n  Mode             fresh session\n  Preserved model  {}\n  Permission mode  {}\n  Session          {}",
            self.model,
            self.permission_mode.as_str(),
            self.session.id,
        );
        Ok(true)
    }

    fn print_cost(&self) {
        let cumulative = self.runtime.usage().cumulative_usage();
        println!("{}", format_cost_report(cumulative));
    }

    fn try_auto_compact(&mut self) -> Option<()> {
        let config = CompactionConfig::default();
        if !should_compact(self.runtime.session(), config) {
            return None;
        }
        let result = compact_session(self.runtime.session(), config);
        if result.removed_message_count == 0 {
            return None;
        }
        println!(
            "\n\x1b[2m[Auto-compact: removed {} messages, ~{} tokens saved]\x1b[0m",
            result.removed_message_count,
            result.removed_message_count * 200
        );
        match build_runtime(
            result.compacted_session,
            self.model.clone(),
            self.system_prompt.clone(),
            true,
            self.allowed_tools.clone(),
            self.permission_mode,
            self.provider,
            self.hooks.clone(),
            self.mcp_tools.clone(),
            self.mcp_manager.clone(),
        ) {
            Ok(new_runtime) => {
                self.runtime = new_runtime;
                Some(())
            }
            Err(_) => None,
        }
    }

    fn resume_session(
        &mut self,
        session_path: Option<String>,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        let Some(session_ref) = session_path else {
            println!("Usage: /resume <session-path>");
            return Ok(false);
        };

        let handle = resolve_session_reference(&session_ref)?;
        let session = Session::load_from_path(&handle.path)?;
        let message_count = session.messages.len();
        self.runtime = build_runtime(
            session,
            self.model.clone(),
            self.system_prompt.clone(),
            true,
            self.allowed_tools.clone(),
            self.permission_mode,
            self.provider,
            self.hooks.clone(),
            self.mcp_tools.clone(),
            self.mcp_manager.clone(),
        )?;
        self.session = handle;
        println!(
            "{}",
            format_resume_report(
                &self.session.path.display().to_string(),
                message_count,
                self.runtime.usage().turns(),
            )
        );
        Ok(true)
    }

    fn print_config(section: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
        println!("{}", render_config_report(section)?);
        Ok(())
    }

    fn print_memory() -> Result<(), Box<dyn std::error::Error>> {
        println!("{}", render_memory_report()?);
        Ok(())
    }

    fn print_diff() -> Result<(), Box<dyn std::error::Error>> {
        println!("{}", render_diff_report()?);
        Ok(())
    }

    fn print_version() {
        println!("{}", render_version_report());
    }

    fn export_session(
        &self,
        requested_path: Option<&str>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let export_path = resolve_export_path(requested_path, self.runtime.session())?;
        fs::write(&export_path, render_export_text(self.runtime.session()))?;
        println!(
            "Export\n  Result           wrote transcript\n  File             {}\n  Messages         {}",
            export_path.display(),
            self.runtime.session().messages.len(),
        );
        Ok(())
    }

    fn handle_session_command(
        &mut self,
        action: Option<&str>,
        target: Option<&str>,
    ) -> Result<bool, Box<dyn std::error::Error>> {
        match action {
            None | Some("list") => {
                println!("{}", render_session_list(&self.session.id)?);
                Ok(false)
            }
            Some("switch") => {
                let Some(target) = target else {
                    println!("Usage: /session switch <session-id>");
                    return Ok(false);
                };
                let handle = resolve_session_reference(target)?;
                let session = Session::load_from_path(&handle.path)?;
                let message_count = session.messages.len();
                self.runtime = build_runtime(
                    session,
                    self.model.clone(),
                    self.system_prompt.clone(),
                    true,
                    self.allowed_tools.clone(),
                    self.permission_mode,
                    self.provider,
                    self.hooks.clone(),
                    self.mcp_tools.clone(),
                    self.mcp_manager.clone(),
                )?;
                self.session = handle;
                println!(
                    "Session switched\n  Active session   {}\n  File             {}\n  Messages         {}",
                    self.session.id,
                    self.session.path.display(),
                    message_count,
                );
                Ok(true)
            }
            Some(other) => {
                println!("Unknown /session action '{other}'. Use /session list or /session switch <session-id>.");
                Ok(false)
            }
        }
    }

    fn compact(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let result = self.runtime.compact(CompactionConfig::default());
        let removed = result.removed_message_count;
        let kept = result.compacted_session.messages.len();
        let skipped = removed == 0;
        self.runtime = build_runtime(
            result.compacted_session,
            self.model.clone(),
            self.system_prompt.clone(),
            true,
            self.allowed_tools.clone(),
            self.permission_mode,
            self.provider,
            self.hooks.clone(),
            self.mcp_tools.clone(),
            self.mcp_manager.clone(),
        )?;
        self.persist_session()?;
        println!("{}", format_compact_report(removed, kept, skipped));
        Ok(())
    }
}

fn prompt_json_backend(provider: Provider) -> &'static str {
    match provider {
        Provider::Anthropic => "anthropic",
        Provider::OpenAi => "openai",
        Provider::Ollama => "ollama",
    }
}

fn sessions_dir() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    let path = cwd.join(".claude").join("sessions");
    fs::create_dir_all(&path)?;
    Ok(path)
}

fn create_managed_session_handle() -> Result<SessionHandle, Box<dyn std::error::Error>> {
    let id = generate_session_id();
    let path = sessions_dir()?.join(format!("{id}.json"));
    Ok(SessionHandle { id, path })
}

fn generate_session_id() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default();
    format!("session-{millis}")
}

fn resolve_session_reference(reference: &str) -> Result<SessionHandle, Box<dyn std::error::Error>> {
    let direct = PathBuf::from(reference);
    let path = if direct.exists() {
        direct
    } else {
        sessions_dir()?.join(format!("{reference}.json"))
    };
    if !path.exists() {
        return Err(format!("session not found: {reference}").into());
    }
    let id = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or(reference)
        .to_string();
    Ok(SessionHandle { id, path })
}

fn list_managed_sessions() -> Result<Vec<ManagedSessionSummary>, Box<dyn std::error::Error>> {
    let mut sessions = Vec::new();
    for entry in fs::read_dir(sessions_dir()?)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let metadata = entry.metadata()?;
        let modified_epoch_secs = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs())
            .unwrap_or_default();
        let message_count = Session::load_from_path(&path)
            .map(|session| session.messages.len())
            .unwrap_or_default();
        let id = path
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or("unknown")
            .to_string();
        sessions.push(ManagedSessionSummary {
            id,
            path,
            modified_epoch_secs,
            message_count,
        });
    }
    sessions.sort_by(|left, right| right.modified_epoch_secs.cmp(&left.modified_epoch_secs));
    Ok(sessions)
}

fn render_session_list(active_session_id: &str) -> Result<String, Box<dyn std::error::Error>> {
    let sessions = list_managed_sessions()?;
    let mut lines = vec![
        "Sessions".to_string(),
        format!("  Directory         {}", sessions_dir()?.display()),
    ];
    if sessions.is_empty() {
        lines.push("  No managed sessions saved yet.".to_string());
        return Ok(lines.join("\n"));
    }
    for session in sessions {
        let marker = if session.id == active_session_id {
            "● current"
        } else {
            "○ saved"
        };
        lines.push(format!(
            "  {id:<20} {marker:<10} msgs={msgs:<4} modified={modified} path={path}",
            id = session.id,
            msgs = session.message_count,
            modified = session.modified_epoch_secs,
            path = session.path.display(),
        ));
    }
    Ok(lines.join("\n"))
}

fn render_repl_help() -> String {
    [
        "REPL".to_string(),
        "  /exit                Quit the REPL".to_string(),
        "  /quit                Quit the REPL".to_string(),
        "  Up/Down              Navigate prompt history".to_string(),
        "  Tab                  Complete slash commands".to_string(),
        "  Ctrl-C               Clear input (or exit on empty prompt)".to_string(),
        "  Shift+Enter/Ctrl+J   Insert a newline".to_string(),
        String::new(),
        render_slash_command_help(),
    ]
    .join(
        "
",
    )
}

fn status_context(
    session_path: Option<&Path>,
) -> Result<StatusContext, Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    let loader = ConfigLoader::default_for(&cwd);
    let discovered_config_files = loader.discover().len();
    let runtime_config = loader.load()?;
    let project_context = ProjectContext::discover_with_git(&cwd, DEFAULT_DATE)?;
    let (project_root, git_branch) =
        parse_git_status_metadata(project_context.git_status.as_deref());
    Ok(StatusContext {
        cwd,
        session_path: session_path.map(Path::to_path_buf),
        loaded_config_files: runtime_config.loaded_entries().len(),
        discovered_config_files,
        memory_file_count: project_context.instruction_files.len(),
        project_root,
        git_branch,
    })
}

fn format_status_report(
    model: &str,
    usage: StatusUsage,
    permission_mode: &str,
    context: &StatusContext,
) -> String {
    [
        format!(
            "Status
  Model            {model}
  Permission mode  {permission_mode}
  Messages         {}
  Turns            {}
  Estimated tokens {}",
            usage.message_count, usage.turns, usage.estimated_tokens,
        ),
        format!(
            "Usage
  Latest total     {}
  Cumulative input {}
  Cumulative output {}
  Cumulative total {}",
            usage.latest.total_tokens(),
            usage.cumulative.input_tokens,
            usage.cumulative.output_tokens,
            usage.cumulative.total_tokens(),
        ),
        format!(
            "Workspace
  Cwd              {}
  Project root     {}
  Git branch       {}
  Session          {}
  Config files     loaded {}/{}
  Memory files     {}",
            context.cwd.display(),
            context
                .project_root
                .as_ref()
                .map_or_else(|| "unknown".to_string(), |path| path.display().to_string()),
            context.git_branch.as_deref().unwrap_or("unknown"),
            context.session_path.as_ref().map_or_else(
                || "live-repl".to_string(),
                |path| path.display().to_string()
            ),
            context.loaded_config_files,
            context.discovered_config_files,
            context.memory_file_count,
        ),
    ]
    .join(
        "

",
    )
}

fn render_config_report(section: Option<&str>) -> Result<String, Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    let loader = ConfigLoader::default_for(&cwd);
    let discovered = loader.discover();
    let runtime_config = loader.load()?;

    let mut lines = vec![
        format!(
            "Config
  Working directory {}
  Loaded files      {}
  Merged keys       {}",
            cwd.display(),
            runtime_config.loaded_entries().len(),
            runtime_config.merged().len()
        ),
        "Discovered files".to_string(),
    ];
    for entry in discovered {
        let source = match entry.source {
            ConfigSource::User => "user",
            ConfigSource::Project => "project",
            ConfigSource::Local => "local",
        };
        let status = if runtime_config
            .loaded_entries()
            .iter()
            .any(|loaded_entry| loaded_entry.path == entry.path)
        {
            "loaded"
        } else {
            "missing"
        };
        lines.push(format!(
            "  {source:<7} {status:<7} {}",
            entry.path.display()
        ));
    }

    if let Some(section) = section {
        lines.push(format!("Merged section: {section}"));
        let value = match section {
            "env" => runtime_config.get("env"),
            "hooks" => runtime_config.get("hooks"),
            "model" => runtime_config.get("model"),
            other => {
                lines.push(format!(
                    "  Unsupported config section '{other}'. Use env, hooks, or model."
                ));
                return Ok(lines.join(
                    "
",
                ));
            }
        };
        lines.push(format!(
            "  {}",
            match value {
                Some(value) => value.render(),
                None => "<unset>".to_string(),
            }
        ));
        return Ok(lines.join(
            "
",
        ));
    }

    lines.push("Merged JSON".to_string());
    lines.push(format!("  {}", runtime_config.as_json().render()));
    Ok(lines.join(
        "
",
    ))
}

fn render_memory_report() -> Result<String, Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    let project_context = ProjectContext::discover(&cwd, DEFAULT_DATE)?;
    let mut lines = vec![format!(
        "Memory
  Working directory {}
  Instruction files {}",
        cwd.display(),
        project_context.instruction_files.len()
    )];
    if project_context.instruction_files.is_empty() {
        lines.push("Discovered files".to_string());
        lines.push(
            "  No CLAUDE instruction files discovered in the current directory ancestry."
                .to_string(),
        );
    } else {
        lines.push("Discovered files".to_string());
        for (index, file) in project_context.instruction_files.iter().enumerate() {
            let preview = file.content.lines().next().unwrap_or("").trim();
            let preview = if preview.is_empty() {
                "<empty>"
            } else {
                preview
            };
            lines.push(format!("  {}. {}", index + 1, file.path.display(),));
            lines.push(format!(
                "     lines={} preview={}",
                file.content.lines().count(),
                preview
            ));
        }
    }
    Ok(lines.join(
        "
",
    ))
}

fn init_claude_md() -> Result<String, Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    Ok(initialize_repo(&cwd)?.render())
}

fn run_init() -> Result<(), Box<dyn std::error::Error>> {
    println!("{}", init_claude_md()?);
    Ok(())
}

fn normalize_permission_mode(mode: &str) -> Option<&'static str> {
    match mode.trim() {
        "read-only" => Some("read-only"),
        "workspace-write" => Some("workspace-write"),
        "danger-full-access" => Some("danger-full-access"),
        _ => None,
    }
}

fn render_diff_report() -> Result<String, Box<dyn std::error::Error>> {
    let output = std::process::Command::new("git")
        .args(["diff", "--", ":(exclude).omx"])
        .current_dir(env::current_dir()?)
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(format!("git diff failed: {stderr}").into());
    }
    let diff = String::from_utf8(output.stdout)?;
    if diff.trim().is_empty() {
        return Ok(
            "Diff\n  Result           clean working tree\n  Detail           no current changes"
                .to_string(),
        );
    }
    Ok(format!("Diff\n\n{}", diff.trim_end()))
}

fn render_version_report() -> String {
    let git_sha = GIT_SHA.unwrap_or("unknown");
    let target = BUILD_TARGET.unwrap_or("unknown");
    format!(
        "Claw Code\n  Version          {VERSION}\n  Git SHA          {git_sha}\n  Target           {target}\n  Build date       {DEFAULT_DATE}"
    )
}

fn render_export_text(session: &Session) -> String {
    let mut lines = vec!["# Conversation Export".to_string(), String::new()];
    for (index, message) in session.messages.iter().enumerate() {
        let role = match message.role {
            MessageRole::System => "system",
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
            MessageRole::Tool => "tool",
        };
        lines.push(format!("## {}. {role}", index + 1));
        for block in &message.blocks {
            match block {
                ContentBlock::Text { text } => lines.push(text.clone()),
                ContentBlock::ToolUse { id, name, input } => {
                    lines.push(format!("[tool_use id={id} name={name}] {input}"));
                }
                ContentBlock::ToolResult {
                    tool_use_id,
                    tool_name,
                    output,
                    is_error,
                } => {
                    lines.push(format!(
                        "[tool_result id={tool_use_id} name={tool_name} error={is_error}] {output}"
                    ));
                }
            }
        }
        lines.push(String::new());
    }
    lines.join("\n")
}

fn default_export_filename(session: &Session) -> String {
    let stem = session
        .messages
        .iter()
        .find_map(|message| match message.role {
            MessageRole::User => message.blocks.iter().find_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            }),
            _ => None,
        })
        .map_or("conversation", |text| {
            text.lines().next().unwrap_or("conversation")
        })
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|part| !part.is_empty())
        .take(8)
        .collect::<Vec<_>>()
        .join("-");
    let fallback = if stem.is_empty() {
        "conversation"
    } else {
        &stem
    };
    format!("{fallback}.txt")
}

fn resolve_export_path(
    requested_path: Option<&str>,
    session: &Session,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let cwd = env::current_dir()?;
    let file_name =
        requested_path.map_or_else(|| default_export_filename(session), ToOwned::to_owned);
    let final_name = if Path::new(&file_name)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("txt"))
    {
        file_name
    } else {
        format!("{file_name}.txt")
    };
    Ok(cwd.join(final_name))
}

fn build_system_prompt() -> Result<Vec<String>, Box<dyn std::error::Error>> {
    Ok(load_system_prompt(
        env::current_dir()?,
        DEFAULT_DATE,
        env::consts::OS,
        "unknown",
    )?)
}

fn initialize_mcp_servers(
    config: &runtime::RuntimeConfig,
) -> (Vec<McpToolSpec>, Option<Arc<Mutex<McpServerManager>>>) {
    let mcp_config = config.mcp();
    if mcp_config.servers().is_empty() {
        return (Vec::new(), None);
    }

    let mut manager = McpServerManager::from_runtime_config(config);

    // Log unsupported servers
    for unsupported in manager.unsupported_servers() {
        eprintln!(
            "  \x1b[33mMCP server `{}` skipped: {}\x1b[0m",
            unsupported.server_name, unsupported.reason
        );
    }

    // Discover tools from supported servers using a temporary tokio runtime
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(error) => {
            eprintln!("  \x1b[33mMCP: failed to create async runtime: {error}\x1b[0m");
            return (Vec::new(), None);
        }
    };

    let discovered = match rt.block_on(manager.discover_tools()) {
        Ok(tools) => tools,
        Err(error) => {
            eprintln!("  \x1b[33mMCP: failed to discover tools: {error}\x1b[0m");
            // Return the manager even on partial failure so it can retry later
            let arc = Arc::new(Mutex::new(manager));
            return (Vec::new(), Some(arc));
        }
    };

    let mcp_tools: Vec<McpToolSpec> = discovered
        .iter()
        .map(|t| McpToolSpec {
            name: t.qualified_name.clone(),
            description: t
                .tool
                .description
                .clone()
                .unwrap_or_else(|| format!("MCP tool {} from server {}", t.raw_name, t.server_name)),
            input_schema: t
                .tool
                .input_schema
                .clone()
                .unwrap_or_else(|| serde_json::json!({"type": "object"})),
        })
        .collect();

    if !mcp_tools.is_empty() {
        eprintln!(
            "  \x1b[2mMCP tools\x1b[0m         {} tool(s) from {} server(s)",
            mcp_tools.len(),
            mcp_config
                .servers()
                .keys()
                .filter(|name| discovered.iter().any(|t| &t.server_name == *name))
                .count()
        );
    }

    let arc = Arc::new(Mutex::new(manager));
    (mcp_tools, Some(arc))
}

fn build_runtime(
    session: Session,
    model: String,
    system_prompt: Vec<String>,
    enable_tools: bool,
    allowed_tools: Option<AllowedToolSet>,
    permission_mode: PermissionMode,
    provider: Provider,
    hooks: runtime::HookRunner,
    mcp_tools: Vec<McpToolSpec>,
    mcp_manager: Option<Arc<Mutex<McpServerManager>>>,
) -> Result<ConversationRuntime<ProviderClient, CliToolExecutor>, Box<dyn std::error::Error>> {
    let model_for_executor = model.clone();
    let mcp_tools_for_executor = mcp_tools.clone();
    let client = match provider {
        Provider::Anthropic => ProviderClient::Anthropic(AnthropicRuntimeClient::new(
            model,
            enable_tools,
            allowed_tools.clone(),
            mcp_tools.clone(),
        )?),
        Provider::OpenAi => ProviderClient::OpenAi(OpenAiRuntimeClient::new(
            model,
            enable_tools,
            allowed_tools.clone(),
            mcp_tools,
        )?),
        Provider::Ollama => ProviderClient::Ollama(OllamaRuntimeClient::new(
            model,
            enable_tools,
            allowed_tools.clone(),
            mcp_tools,
        )?),
    };
    Ok(ConversationRuntime::new(
        session,
        client,
        CliToolExecutor::new(allowed_tools, hooks, mcp_manager, mcp_tools_for_executor, provider, model_for_executor, permission_mode),
        permission_policy(permission_mode),
        system_prompt,
    ))
}

struct CliPermissionPrompter {
    current_mode: PermissionMode,
}

impl CliPermissionPrompter {
    fn new(current_mode: PermissionMode) -> Self {
        Self { current_mode }
    }
}

impl runtime::PermissionPrompter for CliPermissionPrompter {
    fn decide(
        &mut self,
        request: &runtime::PermissionRequest,
    ) -> runtime::PermissionPromptDecision {
        println!();
        println!("Permission approval required");
        println!("  Tool             {}", request.tool_name);
        println!("  Current mode     {}", self.current_mode.as_str());
        println!("  Required mode    {}", request.required_mode.as_str());
        println!("  Input            {}", request.input);
        print!("Approve this tool call? [y/N]: ");
        let _ = io::stdout().flush();

        let mut response = String::new();
        match io::stdin().read_line(&mut response) {
            Ok(_) => {
                let normalized = response.trim().to_ascii_lowercase();
                if matches!(normalized.as_str(), "y" | "yes") {
                    runtime::PermissionPromptDecision::Allow
                } else {
                    runtime::PermissionPromptDecision::Deny {
                        reason: format!(
                            "tool '{}' denied by user approval prompt",
                            request.tool_name
                        ),
                    }
                }
            }
            Err(error) => runtime::PermissionPromptDecision::Deny {
                reason: format!("permission approval failed: {error}"),
            },
        }
    }
}

struct AnthropicRuntimeClient {
    runtime: tokio::runtime::Runtime,
    client: AnthropicClient,
    model: String,
    enable_tools: bool,
    allowed_tools: Option<AllowedToolSet>,
    mcp_tools: Vec<McpToolSpec>,
}

impl AnthropicRuntimeClient {
    fn new(
        model: String,
        enable_tools: bool,
        allowed_tools: Option<AllowedToolSet>,
        mcp_tools: Vec<McpToolSpec>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        Ok(Self {
            runtime: tokio::runtime::Runtime::new()?,
            client: AnthropicClient::from_auth(resolve_cli_auth_source()?).with_base_url(api::read_base_url()),
            model,
            enable_tools,
            allowed_tools,
            mcp_tools,
        })
    }
}

fn resolve_cli_auth_source() -> Result<AuthSource, Box<dyn std::error::Error>> {
    Ok(resolve_startup_auth_source(|| {
        let cwd = env::current_dir().map_err(api::ApiError::from)?;
        let config = ConfigLoader::default_for(&cwd).load().map_err(|error| {
            api::ApiError::Auth(format!("failed to load runtime OAuth config: {error}"))
        })?;
        Ok(config.oauth().cloned())
    })?)
}

impl ApiClient for AnthropicRuntimeClient {
    #[allow(clippy::too_many_lines)]
    fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
        let message_request = MessageRequest {
            model: self.model.clone(),
            max_tokens: DEFAULT_MAX_TOKENS,
            messages: convert_messages(&request.messages),
            system: (!request.system_prompt.is_empty()).then(|| request.system_prompt.join("\n\n")),
            tools: self.enable_tools.then(|| {
                let mut tools: Vec<ToolDefinition> = filter_tool_specs(self.allowed_tools.as_ref())
                    .into_iter()
                    .map(|spec| ToolDefinition {
                        name: spec.name.to_string(),
                        description: Some(spec.description.to_string()),
                        input_schema: spec.input_schema,
                    })
                    .collect();
                for mcp in filter_mcp_tools(&self.mcp_tools, self.allowed_tools.as_ref()) {
                    tools.push(ToolDefinition {
                        name: mcp.name,
                        description: Some(mcp.description),
                        input_schema: mcp.input_schema,
                    });
                }
                tools
            }),
            tool_choice: self.enable_tools.then_some(ToolChoice::Auto),
            stream: true,
        };

        self.runtime.block_on(async {
            let mut stream = self
                .client
                .stream_message(&message_request)
                .await
                .map_err(|error| RuntimeError::new(error.to_string()))?;
            let mut stdout = io::stdout();
            let mut events = Vec::new();
            let mut pending_tool: Option<(String, String, String)> = None;
            let mut saw_stop = false;

            while let Some(event) = stream
                .next_event()
                .await
                .map_err(|error| RuntimeError::new(error.to_string()))?
            {
                match event {
                    ApiStreamEvent::MessageStart(start) => {
                        for block in start.message.content {
                            push_output_block(block, &mut stdout, &mut events, &mut pending_tool)?;
                        }
                    }
                    ApiStreamEvent::ContentBlockStart(start) => {
                        push_output_block(
                            start.content_block,
                            &mut stdout,
                            &mut events,
                            &mut pending_tool,
                        )?;
                    }
                    ApiStreamEvent::ContentBlockDelta(delta) => match delta.delta {
                        ContentBlockDelta::TextDelta { text } => {
                            if !text.is_empty() {
                                write!(stdout, "{text}")
                                    .and_then(|()| stdout.flush())
                                    .map_err(|error| RuntimeError::new(error.to_string()))?;
                                events.push(AssistantEvent::TextDelta(text));
                            }
                        }
                        ContentBlockDelta::InputJsonDelta { partial_json } => {
                            if let Some((_, _, input)) = &mut pending_tool {
                                input.push_str(&partial_json);
                            }
                        }
                    },
                    ApiStreamEvent::ContentBlockStop(_) => {
                        if let Some((id, name, input)) = pending_tool.take() {
                            events.push(AssistantEvent::ToolUse { id, name, input });
                        }
                    }
                    ApiStreamEvent::MessageDelta(delta) => {
                        events.push(AssistantEvent::Usage(TokenUsage {
                            input_tokens: delta.usage.input_tokens,
                            output_tokens: delta.usage.output_tokens,
                            cache_creation_input_tokens: 0,
                            cache_read_input_tokens: 0,
                        }));
                    }
                    ApiStreamEvent::MessageStop(_) => {
                        saw_stop = true;
                        events.push(AssistantEvent::MessageStop);
                    }
                }
            }

            if !saw_stop
                && events.iter().any(|event| {
                    matches!(event, AssistantEvent::TextDelta(text) if !text.is_empty())
                        || matches!(event, AssistantEvent::ToolUse { .. })
                })
            {
                events.push(AssistantEvent::MessageStop);
            }

            if events
                .iter()
                .any(|event| matches!(event, AssistantEvent::MessageStop))
            {
                return Ok(events);
            }

            let response = self
                .client
                .send_message(&MessageRequest {
                    stream: false,
                    ..message_request.clone()
                })
                .await
                .map_err(|error| RuntimeError::new(error.to_string()))?;
            response_to_events(response, &mut stdout)
        })
    }
}

struct OpenAiRuntimeClient {
    runtime: tokio::runtime::Runtime,
    client: OpenAiClient,
    model: String,
    enable_tools: bool,
    allowed_tools: Option<AllowedToolSet>,
    mcp_tools: Vec<McpToolSpec>,
}

impl OpenAiRuntimeClient {
    fn new(
        model: String,
        enable_tools: bool,
        allowed_tools: Option<AllowedToolSet>,
        mcp_tools: Vec<McpToolSpec>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let creds = resolve_openai_auth()?;
        Ok(Self {
            runtime: tokio::runtime::Runtime::new()?,
            client: OpenAiClient::new(&creds.access_token)
                .with_base_url(read_openai_base_url())
                .with_account_id(creds.account_id),
            model,
            enable_tools,
            allowed_tools,
            mcp_tools,
        })
    }
}

impl ApiClient for OpenAiRuntimeClient {
    #[allow(clippy::too_many_lines)]
    fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
        let tools: Option<Vec<ResponsesTool>> = self.enable_tools.then(|| {
            let mut tools: Vec<ResponsesTool> = filter_tool_specs(self.allowed_tools.as_ref())
                .into_iter()
                .filter(|spec| {
                    // ChatGPT backend requires "properties" in object schemas
                    let has_props = spec.input_schema
                        .get("properties")
                        .is_some_and(|p| p.is_object());
                    if !has_props {
                        eprintln!(
                            "\x1b[33mwarning: tool `{}` excluded from OpenAI request (missing properties in schema)\x1b[0m",
                            spec.name
                        );
                    }
                    has_props
                })
                .map(|spec| ResponsesTool {
                    kind: "function".to_string(),
                    name: spec.name.to_string(),
                    description: Some(spec.description.to_string()),
                    parameters: Some(spec.input_schema),
                })
                .collect();
            for mcp in filter_mcp_tools(&self.mcp_tools, self.allowed_tools.as_ref()) {
                if !mcp.input_schema.get("properties").is_some_and(|p| p.is_object()) {
                    eprintln!(
                        "\x1b[33mwarning: MCP tool `{}` excluded from OpenAI request (missing properties in schema)\x1b[0m",
                        mcp.name
                    );
                    continue;
                }
                {
                    tools.push(ResponsesTool {
                        kind: "function".to_string(),
                        name: mcp.name,
                        description: Some(mcp.description),
                        parameters: Some(mcp.input_schema),
                    });
                }
            }
            tools
        });

        let instructions = if request.system_prompt.is_empty() {
            Some("You are a helpful coding assistant.".to_string())
        } else {
            Some(request.system_prompt.join("\n\n"))
        };

        let responses_request = ResponsesRequest {
            model: self.model.clone(),
            input: convert_to_responses_input(&request.messages),
            max_output_tokens: None,
            tools,
            tool_choice: self.enable_tools.then(|| "auto".to_string()),
            instructions,
            stream: true,
            store: Some(false),
            include: Some(vec!["reasoning.encrypted_content".to_string()]),
        };

        self.runtime.block_on(async {
            let mut stream = self
                .client
                .stream_responses(&responses_request)
                .await
                .map_err(|error| RuntimeError::new(error.to_string()))?;
            let mut stdout = io::stdout();
            let mut events = Vec::new();
            let mut saw_stop = false;

            // Track in-progress function calls by their item ID
            let mut pending_fn_calls: BTreeMap<String, (String, String, String)> = BTreeMap::new();
            // call_id, name, arguments

            while let Some((event_type, data)) = stream
                .next_event()
                .await
                .map_err(|error| RuntimeError::new(error.to_string()))?
            {
                match event_type.as_str() {
                    "response.output_text.delta" => {
                        if let Some(delta) = data.get("delta").and_then(|d| d.as_str()) {
                            if !delta.is_empty() {
                                write!(stdout, "{delta}")
                                    .and_then(|()| stdout.flush())
                                    .map_err(|error| RuntimeError::new(error.to_string()))?;
                                events.push(AssistantEvent::TextDelta(delta.to_string()));
                            }
                        }
                    }
                    "response.function_call_arguments.delta" => {
                        // Accumulate function call arguments
                        let item_id = data.get("item_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let delta = data.get("delta").and_then(|v| v.as_str()).unwrap_or("");
                        let entry = pending_fn_calls
                            .entry(item_id)
                            .or_insert_with(|| (String::new(), String::new(), String::new()));
                        entry.2.push_str(delta);
                    }
                    "response.output_item.added" => {
                        // A new output item is being streamed
                        if let Some(item) = data.get("item") {
                            let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
                            if item_type == "function_call" {
                                let item_id = item.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                                let call_id = item.get("call_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                                let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                                pending_fn_calls.insert(item_id, (call_id, name, String::new()));
                            }
                        }
                    }
                    "response.function_call_arguments.done" => {
                        let item_id = data.get("item_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        if let Some((call_id, name, arguments)) = pending_fn_calls.remove(&item_id) {
                            writeln!(
                                stdout,
                                "\n{}",
                                format_tool_call_start(&name, &arguments)
                            )
                            .and_then(|()| stdout.flush())
                            .map_err(|error| RuntimeError::new(error.to_string()))?;
                            // Encode both IDs: fc_ item ID and call_ ID
                            // so convert_to_responses_input can reconstruct both
                            let combined_id = format!("{item_id}:{call_id}");
                            events.push(AssistantEvent::ToolUse {
                                id: combined_id,
                                name,
                                input: arguments,
                            });
                        }
                    }
                    "response.completed" => {
                        // Extract usage from the completed response
                        if let Some(response) = data.get("response") {
                            if let Some(usage) = response.get("usage") {
                                let input_tokens = usage.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                                let output_tokens = usage.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                                events.push(AssistantEvent::Usage(TokenUsage {
                                    input_tokens,
                                    output_tokens,
                                    cache_creation_input_tokens: 0,
                                    cache_read_input_tokens: 0,
                                }));
                            }
                        }
                        saw_stop = true;
                        events.push(AssistantEvent::MessageStop);
                    }
                    _ => {
                        // Ignore other event types (response.created, response.output_item.done, etc.)
                    }
                }
            }

            if !saw_stop && !events.is_empty() {
                // Flush remaining pending function calls
                for (item_id, (call_id, name, arguments)) in std::mem::take(&mut pending_fn_calls) {
                    if !name.is_empty() {
                        let combined_id = format!("{item_id}:{call_id}");
                        events.push(AssistantEvent::ToolUse {
                            id: combined_id,
                            name,
                            input: arguments,
                        });
                    }
                }
                events.push(AssistantEvent::MessageStop);
            }

            Ok(events)
        })
    }
}

struct OllamaRuntimeClient {
    runtime: tokio::runtime::Runtime,
    client: OpenAiClient,
    model: String,
    enable_tools: bool,
    allowed_tools: Option<AllowedToolSet>,
    mcp_tools: Vec<McpToolSpec>,
}

impl OllamaRuntimeClient {
    fn new(
        model: String,
        enable_tools: bool,
        allowed_tools: Option<AllowedToolSet>,
        mcp_tools: Vec<McpToolSpec>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let base_url = env::var("OLLAMA_BASE_URL")
            .unwrap_or_else(|_| "http://localhost:11434".to_string());
        Ok(Self {
            runtime: tokio::runtime::Runtime::new()?,
            client: OpenAiClient::new("ollama").with_base_url(base_url),
            model,
            enable_tools,
            allowed_tools,
            mcp_tools,
        })
    }
}

impl ApiClient for OllamaRuntimeClient {
    #[allow(clippy::too_many_lines)]
    fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
        let tools: Option<Vec<ChatTool>> = self.enable_tools.then(|| {
            let mut tools: Vec<ChatTool> = filter_tool_specs(self.allowed_tools.as_ref())
                .into_iter()
                .map(|spec| ChatTool {
                    kind: "function".to_string(),
                    function: ChatFunction {
                        name: spec.name.to_string(),
                        description: Some(spec.description.to_string()),
                        parameters: spec.input_schema,
                    },
                })
                .collect();
            for mcp in filter_mcp_tools(&self.mcp_tools, self.allowed_tools.as_ref()) {
                tools.push(ChatTool {
                    kind: "function".to_string(),
                    function: ChatFunction {
                        name: mcp.name,
                        description: Some(mcp.description),
                        parameters: mcp.input_schema,
                    },
                });
            }
            tools
        });

        let system_text = if request.system_prompt.is_empty() {
            None
        } else {
            Some(request.system_prompt.join("\n\n"))
        };

        let mut messages = Vec::new();
        if let Some(system) = system_text {
            messages.push(ChatMessage {
                role: "system".to_string(),
                content: Some(system),
                tool_calls: None,
                tool_call_id: None,
            });
        }
        messages.extend(convert_to_chat_messages(&request.messages));

        let chat_request = ChatCompletionRequest {
            model: self.model.clone(),
            messages,
            tools,
            tool_choice: self.enable_tools.then(|| ChatToolChoice::Mode("auto".to_string())),
            stream: true,
            max_tokens: None,
            stream_options: None,
        };

        self.runtime.block_on(async {
            let mut stream = self
                .client
                .stream_chat_completion(&chat_request)
                .await
                .map_err(|error| RuntimeError::new(error.to_string()))?;
            let mut stdout = io::stdout();
            let mut events = Vec::new();
            let mut saw_stop = false;

            // Accumulate tool calls by index
            let mut pending_tool_calls: BTreeMap<u32, (String, String, String)> = BTreeMap::new();
            // (id, name, arguments)

            while let Some(chunk) = stream
                .next_chunk()
                .await
                .map_err(|error| RuntimeError::new(error.to_string()))?
            {
                if let Some(choice) = chunk.choices.first() {
                    // Text content
                    if let Some(ref content) = choice.delta.content {
                        if !content.is_empty() {
                            write!(stdout, "{content}")
                                .and_then(|()| stdout.flush())
                                .map_err(|error| RuntimeError::new(error.to_string()))?;
                            events.push(AssistantEvent::TextDelta(content.clone()));
                        }
                    }

                    // Tool call deltas
                    if let Some(ref tool_calls) = choice.delta.tool_calls {
                        for tc in tool_calls {
                            let entry = pending_tool_calls
                                .entry(tc.index)
                                .or_insert_with(|| (String::new(), String::new(), String::new()));
                            if let Some(ref id) = tc.id {
                                entry.0 = id.clone();
                            }
                            if let Some(ref func) = tc.function {
                                if let Some(ref name) = func.name {
                                    entry.1 = name.clone();
                                }
                                if let Some(ref args) = func.arguments {
                                    entry.2.push_str(args);
                                }
                            }
                        }
                    }

                    // Finish reason
                    if let Some(ref reason) = choice.finish_reason {
                        match reason.as_str() {
                            "stop" | "tool_calls" => {
                                // Flush pending tool calls
                                for (_idx, (id, name, arguments)) in std::mem::take(&mut pending_tool_calls) {
                                    if !name.is_empty() {
                                        writeln!(
                                            stdout,
                                            "\n{}",
                                            format_tool_call_start(&name, &arguments)
                                        )
                                        .and_then(|()| stdout.flush())
                                        .map_err(|error| RuntimeError::new(error.to_string()))?;
                                        events.push(AssistantEvent::ToolUse {
                                            id,
                                            name,
                                            input: arguments,
                                        });
                                    }
                                }
                                saw_stop = true;
                            }
                            _ => {}
                        }
                    }
                }

                // Usage
                if let Some(ref usage) = chunk.usage {
                    events.push(AssistantEvent::Usage(TokenUsage {
                        input_tokens: usage.prompt_tokens,
                        output_tokens: usage.completion_tokens,
                        cache_creation_input_tokens: 0,
                        cache_read_input_tokens: 0,
                    }));
                }
            }

            if !saw_stop && !events.is_empty() {
                // Flush remaining pending tool calls
                for (_idx, (id, name, arguments)) in std::mem::take(&mut pending_tool_calls) {
                    if !name.is_empty() {
                        events.push(AssistantEvent::ToolUse {
                            id,
                            name,
                            input: arguments,
                        });
                    }
                }
            }

            if saw_stop || !events.is_empty() {
                events.push(AssistantEvent::MessageStop);
            }

            Ok(events)
        })
    }
}

enum ProviderClient {
    Anthropic(AnthropicRuntimeClient),
    OpenAi(OpenAiRuntimeClient),
    Ollama(OllamaRuntimeClient),
}

impl ApiClient for ProviderClient {
    fn stream(&mut self, request: ApiRequest) -> Result<Vec<AssistantEvent>, RuntimeError> {
        match self {
            Self::Anthropic(client) => client.stream(request),
            Self::OpenAi(client) => client.stream(request),
            Self::Ollama(client) => client.stream(request),
        }
    }
}

fn slash_command_completion_candidates() -> Vec<String> {
    slash_command_specs()
        .iter()
        .map(|spec| format!("/{}", spec.name))
        .collect()
}

fn format_tool_call_start(name: &str, input: &str) -> String {
    format!(
        "Tool call
  Name             {name}
  Input            {}",
        summarize_tool_payload(input)
    )
}

fn format_tool_result(name: &str, output: &str, is_error: bool) -> String {
    let status = if is_error { "error" } else { "ok" };
    format!(
        "### Tool `{name}`

- Status: {status}
- Output:

```json
{}
```
",
        prettify_tool_payload(output)
    )
}

fn summarize_tool_payload(payload: &str) -> String {
    let compact = match serde_json::from_str::<serde_json::Value>(payload) {
        Ok(value) => value.to_string(),
        Err(_) => payload.trim().to_string(),
    };
    truncate_for_summary(&compact, 96)
}

fn prettify_tool_payload(payload: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(payload) {
        Ok(value) => serde_json::to_string_pretty(&value).unwrap_or_else(|_| payload.to_string()),
        Err(_) => payload.to_string(),
    }
}

fn truncate_for_summary(value: &str, limit: usize) -> String {
    let mut chars = value.chars();
    let truncated = chars.by_ref().take(limit).collect::<String>();
    if chars.next().is_some() {
        format!("{truncated}…")
    } else {
        truncated
    }
}

fn push_output_block(
    block: OutputContentBlock,
    out: &mut impl Write,
    events: &mut Vec<AssistantEvent>,
    pending_tool: &mut Option<(String, String, String)>,
) -> Result<(), RuntimeError> {
    match block {
        OutputContentBlock::Text { text } => {
            if !text.is_empty() {
                write!(out, "{text}")
                    .and_then(|()| out.flush())
                    .map_err(|error| RuntimeError::new(error.to_string()))?;
                events.push(AssistantEvent::TextDelta(text));
            }
        }
        OutputContentBlock::ToolUse { id, name, input } => {
            writeln!(
                out,
                "
{}",
                format_tool_call_start(&name, &input.to_string())
            )
            .and_then(|()| out.flush())
            .map_err(|error| RuntimeError::new(error.to_string()))?;
            *pending_tool = Some((id, name, input.to_string()));
        }
    }
    Ok(())
}

fn response_to_events(
    response: MessageResponse,
    out: &mut impl Write,
) -> Result<Vec<AssistantEvent>, RuntimeError> {
    let mut events = Vec::new();
    let mut pending_tool = None;

    for block in response.content {
        push_output_block(block, out, &mut events, &mut pending_tool)?;
        if let Some((id, name, input)) = pending_tool.take() {
            events.push(AssistantEvent::ToolUse { id, name, input });
        }
    }

    events.push(AssistantEvent::Usage(TokenUsage {
        input_tokens: response.usage.input_tokens,
        output_tokens: response.usage.output_tokens,
        cache_creation_input_tokens: response.usage.cache_creation_input_tokens,
        cache_read_input_tokens: response.usage.cache_read_input_tokens,
    }));
    events.push(AssistantEvent::MessageStop);
    Ok(events)
}

struct CliToolExecutor {
    renderer: TerminalRenderer,
    allowed_tools: Option<AllowedToolSet>,
    hooks: runtime::HookRunner,
    mcp_manager: Option<Arc<Mutex<McpServerManager>>>,
    mcp_tools: Vec<McpToolSpec>,
    provider: Provider,
    model: String,
    permission_mode: PermissionMode,
}

impl CliToolExecutor {
    fn new(
        allowed_tools: Option<AllowedToolSet>,
        hooks: runtime::HookRunner,
        mcp_manager: Option<Arc<Mutex<McpServerManager>>>,
        mcp_tools: Vec<McpToolSpec>,
        provider: Provider,
        model: String,
        permission_mode: PermissionMode,
    ) -> Self {
        Self {
            renderer: TerminalRenderer::new(),
            allowed_tools,
            hooks,
            mcp_manager,
            mcp_tools,
            provider,
            model,
            permission_mode,
        }
    }

    fn execute_mcp_tool(&self, tool_name: &str, input: &str) -> Result<String, ToolError> {
        let manager = self
            .mcp_manager
            .as_ref()
            .ok_or_else(|| ToolError::new(format!("MCP tool `{tool_name}` called but no MCP manager is available")))?;

        let arguments: Option<serde_json::Value> = if input.trim().is_empty() {
            None
        } else {
            Some(
                serde_json::from_str(input)
                    .map_err(|error| ToolError::new(format!("invalid MCP tool input JSON: {error}")))?,
            )
        };

        let manager = Arc::clone(manager);
        let tool_name = tool_name.to_string();
        let response = block_on_new_thread(move || {
            let rt = tokio::runtime::Runtime::new()
                .map_err(|e| ToolError::new(format!("failed to create tokio runtime: {e}")))?;
            rt.block_on(async {
                let mut mgr = manager.lock().map_err(|error| {
                    ToolError::new(format!("failed to lock MCP manager: {error}"))
                })?;
                mgr.call_tool(&tool_name, arguments)
                    .await
                    .map_err(|error| ToolError::new(format!("MCP tool call failed: {error}")))
            })
        })?;

        if let Some(error) = response.error {
            return Err(ToolError::new(format!(
                "MCP JSON-RPC error: {} ({})",
                error.message, error.code
            )));
        }

        match response.result {
            Some(result) => {
                let is_error = result.is_error.unwrap_or(false);
                let text = result
                    .content
                    .iter()
                    .filter_map(|c| c.data.get("text").and_then(|v| v.as_str()))
                    .collect::<Vec<_>>()
                    .join("\n");
                if is_error {
                    Err(ToolError::new(text))
                } else {
                    Ok(text)
                }
            }
            None => Ok(String::new()),
        }
    }

    fn execute_agent_subconversation(&self, input: &serde_json::Value) -> Result<String, ToolError> {
        let description = input.get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("sub-agent task");
        let prompt = input.get("prompt")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::new("Agent tool requires a 'prompt' field"))?;
        let agent_model = input.get("model")
            .and_then(|v| v.as_str())
            .unwrap_or(&self.model);

        eprintln!("\n\x1b[2m[Sub-agent: {description}]\x1b[0m");

        // Build a child runtime with the prompt as system prompt
        // Exclude "Agent" from child tools to prevent infinite recursion
        let child_allowed = child_allowed_tools(self.allowed_tools.as_ref());

        let child_hooks = self.hooks.clone();
        let mut child_runtime = build_runtime(
            Session::new(),
            agent_model.to_string(),
            vec![prompt.to_string()],
            true,
            child_allowed,
            self.permission_mode,
            self.provider,
            child_hooks,
            self.mcp_tools.clone(),
            self.mcp_manager.clone(),
        ).map_err(|e| ToolError::new(format!("failed to create sub-agent runtime: {e}")))?;

        // Run the child conversation
        let turn_input = "Execute the task described in your instructions. When done, summarize what you accomplished.";

        match child_runtime.run_turn(turn_input, None) {
            Ok(summary) => {
                // Extract the last assistant text from the child session
                let session = child_runtime.session();
                let result_text = session.messages.iter()
                    .rev()
                    .find(|m| m.role == MessageRole::Assistant)
                    .map(|m| {
                        m.blocks.iter()
                            .filter_map(|b| match b {
                                ContentBlock::Text { text } => Some(text.as_str()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .unwrap_or_else(|| "Sub-agent completed but produced no text output.".to_string());

                eprintln!("\x1b[2m[Sub-agent completed: {} iterations]\x1b[0m", summary.iterations);
                Ok(serde_json::json!({
                    "status": "completed",
                    "description": description,
                    "result": result_text,
                    "iterations": summary.iterations,
                }).to_string())
            }
            Err(error) => {
                Ok(serde_json::json!({
                    "status": "failed",
                    "description": description,
                    "error": error.to_string(),
                }).to_string())
            }
        }
    }
}

/// Run an async closure on a dedicated thread with its own tokio runtime,
/// avoiding "Cannot start a runtime from within a runtime" panics when
/// the caller is already inside a `block_on` context.
fn block_on_new_thread<F, T>(f: F) -> Result<T, ToolError>
where
    F: FnOnce() -> Result<T, ToolError> + Send,
    T: Send,
{
    std::thread::scope(|s| {
        s.spawn(f)
            .join()
            .map_err(|_| ToolError::new("tool runtime thread panicked".to_string()))?
    })
}

fn is_mcp_tool(tool_name: &str) -> bool {
    tool_name.starts_with("mcp__")
}

impl ToolExecutor for CliToolExecutor {
    fn execute(&mut self, tool_name: &str, input: &str) -> Result<String, ToolError> {
        if self
            .allowed_tools
            .as_ref()
            .is_some_and(|allowed| !allowed.contains(tool_name))
        {
            return Err(ToolError::new(format!(
                "tool `{tool_name}` is not enabled by the current --allowedTools setting"
            )));
        }
        if let runtime::HookOutcome::Block { reason } =
            self.hooks.run_pre_tool_use(tool_name, input)
        {
            return Err(ToolError::new(format!(
                "blocked by PreToolUse hook: {reason}"
            )));
        }

        // Route MCP tools to the MCP manager
        if is_mcp_tool(tool_name) {
            return match self.execute_mcp_tool(tool_name, input) {
                Ok(output) => {
                    let _ = self.hooks.run_post_tool_use(tool_name, &output);
                    let markdown = format_tool_result(tool_name, &output, false);
                    self.renderer
                        .stream_markdown(&markdown, &mut io::stdout())
                        .map_err(|error| ToolError::new(error.to_string()))?;
                    Ok(output)
                }
                Err(error) => {
                    let error_str = error.to_string();
                    let _ = self.hooks.run_post_tool_use(tool_name, &error_str);
                    let markdown = format_tool_result(tool_name, &error_str, true);
                    self.renderer
                        .stream_markdown(&markdown, &mut io::stdout())
                        .map_err(|stream_error| ToolError::new(stream_error.to_string()))?;
                    Err(error)
                }
            };
        }

        let value: serde_json::Value = serde_json::from_str(input)
            .map_err(|error| ToolError::new(format!("invalid tool input JSON: {error}")))?;

        // Intercept Agent tool — run a real sub-conversation
        if tool_name == "Agent" {
            let result = self.execute_agent_subconversation(&value);
            match &result {
                Ok(output) => {
                    let _ = self.hooks.run_post_tool_use(tool_name, output);
                    let markdown = format_tool_result(tool_name, output, false);
                    self.renderer
                        .stream_markdown(&markdown, &mut io::stdout())
                        .map_err(|error| ToolError::new(error.to_string()))?;
                }
                Err(error) => {
                    let error_str = error.to_string();
                    let _ = self.hooks.run_post_tool_use(tool_name, &error_str);
                    let markdown = format_tool_result(tool_name, &error_str, true);
                    self.renderer
                        .stream_markdown(&markdown, &mut io::stdout())
                        .map_err(|stream_error| ToolError::new(stream_error.to_string()))?;
                }
            }
            return result;
        }

        // Intercept Skill tool — load skill prompt then run as sub-conversation
        if tool_name == "Skill" {
            let skill_output = execute_tool("Skill", &value)
                .map_err(|e| ToolError::new(e))?;

            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&skill_output) {
                if let Some(prompt) = parsed.get("prompt").and_then(|v| v.as_str()) {
                    let skill_name = parsed.get("skill")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown");
                    let agent_input = serde_json::json!({
                        "description": format!("Skill: {}", skill_name),
                        "prompt": prompt,
                    });
                    let result = self.execute_agent_subconversation(&agent_input);
                    match &result {
                        Ok(output) => {
                            let _ = self.hooks.run_post_tool_use(tool_name, output);
                            let markdown = format_tool_result(tool_name, output, false);
                            self.renderer
                                .stream_markdown(&markdown, &mut io::stdout())
                                .map_err(|error| ToolError::new(error.to_string()))?;
                        }
                        Err(error) => {
                            let error_str = error.to_string();
                            let _ = self.hooks.run_post_tool_use(tool_name, &error_str);
                            let markdown = format_tool_result(tool_name, &error_str, true);
                            self.renderer
                                .stream_markdown(&markdown, &mut io::stdout())
                                .map_err(|stream_error| ToolError::new(stream_error.to_string()))?;
                        }
                    }
                    return result;
                }
            }

            // No prompt in the skill output — return the raw output
            let _ = self.hooks.run_post_tool_use(tool_name, &skill_output);
            let markdown = format_tool_result(tool_name, &skill_output, false);
            self.renderer
                .stream_markdown(&markdown, &mut io::stdout())
                .map_err(|error| ToolError::new(error.to_string()))?;
            return Ok(skill_output);
        }

        match execute_tool(tool_name, &value) {
            Ok(output) => {
                let _ = self.hooks.run_post_tool_use(tool_name, &output);
                let markdown = format_tool_result(tool_name, &output, false);
                self.renderer
                    .stream_markdown(&markdown, &mut io::stdout())
                    .map_err(|error| ToolError::new(error.to_string()))?;
                Ok(output)
            }
            Err(error) => {
                let _ = self.hooks.run_post_tool_use(tool_name, &error);
                let markdown = format_tool_result(tool_name, &error, true);
                self.renderer
                    .stream_markdown(&markdown, &mut io::stdout())
                    .map_err(|stream_error| ToolError::new(stream_error.to_string()))?;
                Err(ToolError::new(error))
            }
        }
    }
}

fn permission_policy(mode: PermissionMode) -> PermissionPolicy {
    tool_permission_specs()
        .into_iter()
        .fold(PermissionPolicy::new(mode), |policy, spec| {
            policy.with_tool_requirement(spec.name, spec.required_permission)
        })
}

fn tool_permission_specs() -> Vec<ToolSpec> {
    mvp_tool_specs()
}

fn convert_messages(messages: &[ConversationMessage]) -> Vec<InputMessage> {
    messages
        .iter()
        .filter_map(|message| {
            let role = match message.role {
                MessageRole::System | MessageRole::User | MessageRole::Tool => "user",
                MessageRole::Assistant => "assistant",
            };
            let content = message
                .blocks
                .iter()
                .map(|block| match block {
                    ContentBlock::Text { text } => InputContentBlock::Text { text: text.clone() },
                    ContentBlock::ToolUse { id, name, input } => InputContentBlock::ToolUse {
                        id: id.clone(),
                        name: name.clone(),
                        input: serde_json::from_str(input)
                            .unwrap_or_else(|_| serde_json::json!({ "raw": input })),
                    },
                    ContentBlock::ToolResult {
                        tool_use_id,
                        output,
                        is_error,
                        ..
                    } => InputContentBlock::ToolResult {
                        tool_use_id: tool_use_id.clone(),
                        content: vec![ToolResultContentBlock::Text {
                            text: output.clone(),
                        }],
                        is_error: *is_error,
                    },
                })
                .collect::<Vec<_>>();
            (!content.is_empty()).then(|| InputMessage {
                role: role.to_string(),
                content,
            })
        })
        .collect()
}

fn convert_to_responses_input(
    messages: &[ConversationMessage],
) -> Vec<ResponsesInput> {
    let mut result = Vec::new();

    for message in messages {
        match message.role {
            MessageRole::System | MessageRole::User => {
                let text: String = message
                    .blocks
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                if !text.is_empty() {
                    result.push(ResponsesInput::Message(ResponsesMessage {
                        role: "user".to_string(),
                        content: ResponsesContent::Text(text),
                    }));
                }
            }
            MessageRole::Assistant => {
                // For assistant messages, we need to output both text and function calls
                for block in &message.blocks {
                    match block {
                        ContentBlock::Text { text } => {
                            if !text.is_empty() {
                                result.push(ResponsesInput::Message(ResponsesMessage {
                                    role: "assistant".to_string(),
                                    content: ResponsesContent::Text(text.clone()),
                                }));
                            }
                        }
                        ContentBlock::ToolUse { id, name, input } => {
                            // id may be "fc_xxx:call_xxx" (combined) or plain
                            let (item_id, call_id) = id
                                .split_once(':')
                                .unwrap_or((id, id));
                            result.push(ResponsesInput::FunctionCall(ResponsesFunctionCallInput {
                                kind: "function_call".to_string(),
                                id: item_id.to_string(),
                                call_id: call_id.to_string(),
                                name: name.clone(),
                                arguments: input.clone(),
                            }));
                        }
                        _ => {}
                    }
                }
            }
            MessageRole::Tool => {
                // Tool results become function_call_output items
                for block in &message.blocks {
                    if let ContentBlock::ToolResult {
                        tool_use_id,
                        output,
                        ..
                    } = block
                    {
                        // tool_use_id may be "fc_xxx:call_xxx" — extract call_id part
                        let call_id = tool_use_id
                            .split_once(':')
                            .map(|(_, c)| c)
                            .unwrap_or(tool_use_id);
                        result.push(ResponsesInput::FunctionCallOutput(ResponsesFunctionCallOutputInput {
                            kind: "function_call_output".to_string(),
                            call_id: call_id.to_string(),
                            output: output.clone(),
                        }));
                    }
                }
            }
        }
    }
    result
}

fn convert_to_chat_messages(messages: &[ConversationMessage]) -> Vec<ChatMessage> {
    let mut result = Vec::new();

    for message in messages {
        match message.role {
            MessageRole::System | MessageRole::User => {
                let text: String = message
                    .blocks
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                if !text.is_empty() {
                    result.push(ChatMessage {
                        role: "user".to_string(),
                        content: Some(text),
                        tool_calls: None,
                        tool_call_id: None,
                    });
                }
            }
            MessageRole::Assistant => {
                let mut text_parts = Vec::new();
                let mut tool_calls = Vec::new();

                for block in &message.blocks {
                    match block {
                        ContentBlock::Text { text } => {
                            if !text.is_empty() {
                                text_parts.push(text.as_str());
                            }
                        }
                        ContentBlock::ToolUse { id, name, input } => {
                            tool_calls.push(api::ChatToolCall {
                                id: id.clone(),
                                kind: "function".to_string(),
                                function: api::ChatFunctionCall {
                                    name: name.clone(),
                                    arguments: input.clone(),
                                },
                            });
                        }
                        _ => {}
                    }
                }

                let content = if text_parts.is_empty() {
                    None
                } else {
                    Some(text_parts.join("\n"))
                };
                let tc = if tool_calls.is_empty() {
                    None
                } else {
                    Some(tool_calls)
                };

                if content.is_some() || tc.is_some() {
                    result.push(ChatMessage {
                        role: "assistant".to_string(),
                        content,
                        tool_calls: tc,
                        tool_call_id: None,
                    });
                }
            }
            MessageRole::Tool => {
                for block in &message.blocks {
                    if let ContentBlock::ToolResult {
                        tool_use_id,
                        output,
                        ..
                    } = block
                    {
                        result.push(ChatMessage {
                            role: "tool".to_string(),
                            content: Some(output.clone()),
                            tool_calls: None,
                            tool_call_id: Some(tool_use_id.clone()),
                        });
                    }
                }
            }
        }
    }
    result
}

/// Expand `@path` references in user input to include file contents inline.
/// e.g. "explain @src/main.rs" becomes "explain \n<file path=\"src/main.rs\">\n...contents...\n</file>"
fn expand_file_references(input: &str) -> String {
    let mut result = String::new();
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '@' {
            // Collect the file path (non-whitespace characters after @)
            let mut path_str = String::new();
            while let Some(&next) = chars.peek() {
                if next.is_whitespace() {
                    break;
                }
                path_str.push(chars.next().unwrap());
            }
            if path_str.is_empty() {
                result.push('@');
                continue;
            }
            let path = Path::new(&path_str);
            if path.exists() && path.is_file() {
                match fs::read_to_string(path) {
                    Ok(contents) => {
                        result.push_str(&format!(
                            "\n<file path=\"{path_str}\">\n{contents}\n</file>\n"
                        ));
                    }
                    Err(_) => {
                        // Could not read — keep the original @reference
                        result.push('@');
                        result.push_str(&path_str);
                    }
                }
            } else {
                // Not a valid file — keep the original text
                result.push('@');
                result.push_str(&path_str);
            }
        } else {
            result.push(ch);
        }
    }
    result
}

fn print_help_to(out: &mut impl Write) -> io::Result<()> {
    writeln!(out, "claw v{VERSION}")?;
    writeln!(out)?;
    writeln!(out, "Usage:")?;
    writeln!(
        out,
        "  claw [--model MODEL] [--allowedTools TOOL[,TOOL...]]"
    )?;
    writeln!(out, "      Start the interactive REPL")?;
    writeln!(
        out,
        "  claw [--model MODEL] [--output-format text|json] prompt TEXT"
    )?;
    writeln!(out, "      Send one prompt and exit")?;
    writeln!(
        out,
        "  claw [--model MODEL] [--output-format text|json] TEXT"
    )?;
    writeln!(out, "      Shorthand non-interactive prompt mode")?;
    writeln!(
        out,
        "  claw --resume SESSION.json [/status] [/compact] [...]"
    )?;
    writeln!(
        out,
        "      Inspect or maintain a saved session without entering the REPL"
    )?;
    writeln!(out, "  claw dump-manifests")?;
    writeln!(out, "  claw bootstrap-plan")?;
    writeln!(
        out,
        "  claw system-prompt [--cwd PATH] [--date YYYY-MM-DD]"
    )?;
    writeln!(out, "  claw login")?;
    writeln!(out, "  claw logout")?;
    writeln!(out, "  claw init")?;
    writeln!(out)?;
    writeln!(out, "Flags:")?;
    writeln!(
        out,
        "  --model MODEL              Override the active model"
    )?;
    writeln!(
        out,
        "  --output-format FORMAT     Non-interactive output format: text or json"
    )?;
    writeln!(
        out,
        "  --permission-mode MODE     Set read-only, workspace-write, or danger-full-access"
    )?;
    writeln!(out, "  --allowedTools TOOLS       Restrict enabled tools (repeatable; comma-separated aliases supported)")?;
    writeln!(
        out,
        "  --provider PROVIDER        Select provider: anthropic, claude, openai, codex"
    )?;
    writeln!(
        out,
        "  --version, -V              Print version and build information locally"
    )?;
    writeln!(out)?;
    writeln!(out, "Interactive slash commands:")?;
    writeln!(out, "{}", render_slash_command_help())?;
    writeln!(out)?;
    let resume_commands = resume_supported_slash_commands()
        .into_iter()
        .map(|spec| match spec.argument_hint {
            Some(argument_hint) => format!("/{} {}", spec.name, argument_hint),
            None => format!("/{}", spec.name),
        })
        .collect::<Vec<_>>()
        .join(", ");
    writeln!(out, "Resume-safe commands: {resume_commands}")?;
    writeln!(out, "Examples:")?;
    writeln!(
        out,
        "  claw --model claude-opus \"summarize this repo\""
    )?;
    writeln!(
        out,
        "  claw --output-format json prompt \"explain src/main.rs\""
    )?;
    writeln!(
        out,
        "  claw --allowedTools read,glob \"summarize Cargo.toml\""
    )?;
    writeln!(
        out,
        "  claw --resume session.json /status /diff /export notes.txt"
    )?;
    writeln!(out, "  claw login")?;
    writeln!(out, "  claw init")?;
    Ok(())
}

fn print_help() {
    let _ = print_help_to(&mut io::stdout());
}

#[cfg(test)]
mod tests {
    use super::{
        child_allowed_tools, filter_mcp_tools, filter_tool_specs, format_compact_report,
        format_cost_report, format_model_report, format_model_switch_report,
        format_permissions_report, format_permissions_switch_report, format_resume_report,
        format_status_report, format_tool_call_start, format_tool_result,
        normalize_permission_mode, parse_args, parse_git_status_metadata, print_help_to,
        render_config_report, render_memory_report, render_repl_help,
        resume_supported_slash_commands, status_context, CliAction, CliOutputFormat, Provider,
        SlashCommand, StatusUsage, DEFAULT_MODEL, McpToolSpec,
    };
    use runtime::{ContentBlock, ConversationMessage, MessageRole, PermissionMode};
    use std::path::PathBuf;

    #[test]
    fn defaults_to_repl_when_no_args() {
        assert_eq!(
            parse_args(&[]).expect("args should parse"),
            CliAction::Repl {
                model: DEFAULT_MODEL.to_string(),
                allowed_tools: None,
                permission_mode: PermissionMode::WorkspaceWrite,
                provider: Provider::Anthropic,
            }
        );
    }

    #[test]
    fn parses_prompt_subcommand() {
        let args = vec![
            "prompt".to_string(),
            "hello".to_string(),
            "world".to_string(),
        ];
        assert_eq!(
            parse_args(&args).expect("args should parse"),
            CliAction::Prompt {
                prompt: "hello world".to_string(),
                model: DEFAULT_MODEL.to_string(),
                output_format: CliOutputFormat::Text,
                allowed_tools: None,
                permission_mode: PermissionMode::WorkspaceWrite,
                provider: Provider::Anthropic,
            }
        );
    }

    #[test]
    fn parses_bare_prompt_and_json_output_flag() {
        let args = vec![
            "--output-format=json".to_string(),
            "--model".to_string(),
            "claude-opus".to_string(),
            "explain".to_string(),
            "this".to_string(),
        ];
        assert_eq!(
            parse_args(&args).expect("args should parse"),
            CliAction::Prompt {
                prompt: "explain this".to_string(),
                model: "claude-opus".to_string(),
                output_format: CliOutputFormat::Json,
                allowed_tools: None,
                permission_mode: PermissionMode::WorkspaceWrite,
                provider: Provider::Anthropic,
            }
        );
    }

    #[test]
    fn parses_version_flags_without_initializing_prompt_mode() {
        assert_eq!(
            parse_args(&["--version".to_string()]).expect("args should parse"),
            CliAction::Version
        );
        assert_eq!(
            parse_args(&["-V".to_string()]).expect("args should parse"),
            CliAction::Version
        );
    }

    #[test]
    fn parses_permission_mode_flag() {
        let args = vec!["--permission-mode=read-only".to_string()];
        assert_eq!(
            parse_args(&args).expect("args should parse"),
            CliAction::Repl {
                model: DEFAULT_MODEL.to_string(),
                allowed_tools: None,
                permission_mode: PermissionMode::ReadOnly,
                provider: Provider::Anthropic,
            }
        );
    }

    #[test]
    fn parses_allowed_tools_flags_with_aliases_and_lists() {
        let args = vec![
            "--allowedTools".to_string(),
            "read,glob".to_string(),
            "--allowed-tools=write_file".to_string(),
        ];
        assert_eq!(
            parse_args(&args).expect("args should parse"),
            CliAction::Repl {
                model: DEFAULT_MODEL.to_string(),
                allowed_tools: Some(
                    ["glob_search", "read_file", "write_file"]
                        .into_iter()
                        .map(str::to_string)
                        .collect()
                ),
                permission_mode: PermissionMode::WorkspaceWrite,
                provider: Provider::Anthropic,
            }
        );
    }

    #[test]
    fn parses_allowed_mcp_tools_by_exact_name() {
        let args = vec![
            "--allowedTools".to_string(),
            "read,mcp__demo__echo".to_string(),
        ];
        assert_eq!(
            parse_args(&args).expect("args should parse"),
            CliAction::Repl {
                model: DEFAULT_MODEL.to_string(),
                allowed_tools: Some(
                    ["mcp__demo__echo", "read_file"]
                        .into_iter()
                        .map(str::to_string)
                        .collect()
                ),
                permission_mode: PermissionMode::WorkspaceWrite,
                provider: Provider::Anthropic,
            }
        );
    }

    #[test]
    fn rejects_unknown_allowed_tools() {
        let error = parse_args(&["--allowedTools".to_string(), "teleport".to_string()])
            .expect_err("tool should be rejected");
        assert!(error.contains("unsupported tool in --allowedTools: teleport"));
    }

    #[test]
    fn parses_system_prompt_options() {
        let args = vec![
            "system-prompt".to_string(),
            "--cwd".to_string(),
            "/tmp/project".to_string(),
            "--date".to_string(),
            "2026-04-01".to_string(),
        ];
        assert_eq!(
            parse_args(&args).expect("args should parse"),
            CliAction::PrintSystemPrompt {
                cwd: PathBuf::from("/tmp/project"),
                date: "2026-04-01".to_string(),
            }
        );
    }

    #[test]
    fn parses_login_and_logout_subcommands() {
        assert_eq!(
            parse_args(&["login".to_string()]).expect("login should parse"),
            CliAction::Login { provider: Provider::Anthropic }
        );
        assert_eq!(
            parse_args(&["logout".to_string()]).expect("logout should parse"),
            CliAction::Logout { provider: Provider::Anthropic }
        );
        assert_eq!(
            parse_args(&["init".to_string()]).expect("init should parse"),
            CliAction::Init
        );
    }

    #[test]
    fn parses_resume_flag_with_slash_command() {
        let args = vec![
            "--resume".to_string(),
            "session.json".to_string(),
            "/compact".to_string(),
        ];
        assert_eq!(
            parse_args(&args).expect("args should parse"),
            CliAction::ResumeSession {
                session_path: PathBuf::from("session.json"),
                commands: vec!["/compact".to_string()],
            }
        );
    }

    #[test]
    fn parses_resume_flag_with_multiple_slash_commands() {
        let args = vec![
            "--resume".to_string(),
            "session.json".to_string(),
            "/status".to_string(),
            "/compact".to_string(),
            "/cost".to_string(),
        ];
        assert_eq!(
            parse_args(&args).expect("args should parse"),
            CliAction::ResumeSession {
                session_path: PathBuf::from("session.json"),
                commands: vec![
                    "/status".to_string(),
                    "/compact".to_string(),
                    "/cost".to_string(),
                ],
            }
        );
    }

    #[test]
    fn filtered_tool_specs_respect_allowlist() {
        let allowed = ["read_file", "grep_search"]
            .into_iter()
            .map(str::to_string)
            .collect();
        let filtered = filter_tool_specs(Some(&allowed));
        let names = filtered
            .into_iter()
            .map(|spec| spec.name)
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["read_file", "grep_search"]);
    }

    #[test]
    fn filtered_mcp_tools_respect_allowlist() {
        let allowed = ["mcp__demo__echo"]
            .into_iter()
            .map(str::to_string)
            .collect();
        let filtered = filter_mcp_tools(
            &[
                McpToolSpec {
                    name: "mcp__demo__echo".to_string(),
                    description: "Echo".to_string(),
                    input_schema: serde_json::json!({"type": "object"}),
                },
                McpToolSpec {
                    name: "mcp__demo__sum".to_string(),
                    description: "Sum".to_string(),
                    input_schema: serde_json::json!({"type": "object"}),
                },
            ],
            Some(&allowed),
        );
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name, "mcp__demo__echo");
    }

    #[test]
    fn child_allowed_tools_preserve_parent_restrictions() {
        let allowed = ["read_file", "Agent", "Skill", "mcp__demo__echo"]
            .into_iter()
            .map(str::to_string)
            .collect();
        let child = child_allowed_tools(Some(&allowed)).expect("child allowlist");
        assert!(child.contains("read_file"));
        assert!(child.contains("mcp__demo__echo"));
        assert!(!child.contains("Agent"));
        assert!(!child.contains("Skill"));
    }

    #[test]
    fn shared_help_uses_resume_annotation_copy() {
        let help = commands::render_slash_command_help();
        assert!(help.contains("Slash commands"));
        assert!(help.contains("works with --resume SESSION.json"));
    }

    #[test]
    fn repl_help_includes_shared_commands_and_exit() {
        let help = render_repl_help();
        assert!(help.contains("REPL"));
        assert!(help.contains("/help"));
        assert!(help.contains("/status"));
        assert!(help.contains("/model [model]"));
        assert!(help.contains("/permissions [read-only|workspace-write|danger-full-access]"));
        assert!(help.contains("/clear [--confirm]"));
        assert!(help.contains("/cost"));
        assert!(help.contains("/resume <session-path>"));
        assert!(help.contains("/config [env|hooks|model]"));
        assert!(help.contains("/memory"));
        assert!(help.contains("/init"));
        assert!(help.contains("/diff"));
        assert!(help.contains("/version"));
        assert!(help.contains("/export [file]"));
        assert!(help.contains("/session [list|switch <session-id>]"));
        assert!(help.contains("/exit"));
    }

    #[test]
    fn resume_supported_command_list_matches_expected_surface() {
        let names = resume_supported_slash_commands()
            .into_iter()
            .map(|spec| spec.name)
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            vec![
                "help", "status", "compact", "clear", "cost", "config", "memory", "init", "diff",
                "version", "export",
            ]
        );
    }

    #[test]
    fn resume_report_uses_sectioned_layout() {
        let report = format_resume_report("session.json", 14, 6);
        assert!(report.contains("Session resumed"));
        assert!(report.contains("Session file     session.json"));
        assert!(report.contains("Messages         14"));
        assert!(report.contains("Turns            6"));
    }

    #[test]
    fn compact_report_uses_structured_output() {
        let compacted = format_compact_report(8, 5, false);
        assert!(compacted.contains("Compact"));
        assert!(compacted.contains("Result           compacted"));
        assert!(compacted.contains("Messages removed 8"));
        let skipped = format_compact_report(0, 3, true);
        assert!(skipped.contains("Result           skipped"));
    }

    #[test]
    fn cost_report_uses_sectioned_layout() {
        let report = format_cost_report(runtime::TokenUsage {
            input_tokens: 20,
            output_tokens: 8,
            cache_creation_input_tokens: 3,
            cache_read_input_tokens: 1,
        });
        assert!(report.contains("Cost"));
        assert!(report.contains("Input tokens     20"));
        assert!(report.contains("Output tokens    8"));
        assert!(report.contains("Cache create     3"));
        assert!(report.contains("Cache read       1"));
        assert!(report.contains("Total tokens     32"));
    }

    #[test]
    fn permissions_report_uses_sectioned_layout() {
        let report = format_permissions_report("workspace-write");
        assert!(report.contains("Permissions"));
        assert!(report.contains("Active mode      workspace-write"));
        assert!(report.contains("Modes"));
        assert!(report.contains("read-only          ○ available Read/search tools only"));
        assert!(report.contains("workspace-write    ● current   Edit files inside the workspace"));
        assert!(report.contains("danger-full-access ○ available Unrestricted tool access"));
    }

    #[test]
    fn permissions_switch_report_is_structured() {
        let report = format_permissions_switch_report("read-only", "workspace-write");
        assert!(report.contains("Permissions updated"));
        assert!(report.contains("Result           mode switched"));
        assert!(report.contains("Previous mode    read-only"));
        assert!(report.contains("Active mode      workspace-write"));
        assert!(report.contains("Applies to       subsequent tool calls"));
    }

    #[test]
    fn init_help_mentions_direct_subcommand() {
        let mut help = Vec::new();
        print_help_to(&mut help).expect("help should render");
        let help = String::from_utf8(help).expect("help should be utf8");
        assert!(help.contains("claw init"));
    }

    #[test]
    fn model_report_uses_sectioned_layout() {
        let report = format_model_report("claude-sonnet", 12, 4);
        assert!(report.contains("Model"));
        assert!(report.contains("Current model    claude-sonnet"));
        assert!(report.contains("Session messages 12"));
        assert!(report.contains("Switch models with /model <name>"));
    }

    #[test]
    fn model_switch_report_preserves_context_summary() {
        let report = format_model_switch_report("claude-sonnet", "claude-opus", 9);
        assert!(report.contains("Model updated"));
        assert!(report.contains("Previous         claude-sonnet"));
        assert!(report.contains("Current          claude-opus"));
        assert!(report.contains("Preserved msgs   9"));
    }

    #[test]
    fn status_line_reports_model_and_token_totals() {
        let status = format_status_report(
            "claude-sonnet",
            StatusUsage {
                message_count: 7,
                turns: 3,
                latest: runtime::TokenUsage {
                    input_tokens: 5,
                    output_tokens: 4,
                    cache_creation_input_tokens: 1,
                    cache_read_input_tokens: 0,
                },
                cumulative: runtime::TokenUsage {
                    input_tokens: 20,
                    output_tokens: 8,
                    cache_creation_input_tokens: 2,
                    cache_read_input_tokens: 1,
                },
                estimated_tokens: 128,
            },
            "workspace-write",
            &super::StatusContext {
                cwd: PathBuf::from("/tmp/project"),
                session_path: Some(PathBuf::from("session.json")),
                loaded_config_files: 2,
                discovered_config_files: 3,
                memory_file_count: 4,
                project_root: Some(PathBuf::from("/tmp")),
                git_branch: Some("main".to_string()),
            },
        );
        assert!(status.contains("Status"));
        assert!(status.contains("Model            claude-sonnet"));
        assert!(status.contains("Permission mode  workspace-write"));
        assert!(status.contains("Messages         7"));
        assert!(status.contains("Latest total     10"));
        assert!(status.contains("Cumulative total 31"));
        assert!(status.contains("Cwd              /tmp/project"));
        assert!(status.contains("Project root     /tmp"));
        assert!(status.contains("Git branch       main"));
        assert!(status.contains("Session          session.json"));
        assert!(status.contains("Config files     loaded 2/3"));
        assert!(status.contains("Memory files     4"));
    }

    #[test]
    fn config_report_supports_section_views() {
        let report = render_config_report(Some("env")).expect("config report should render");
        assert!(report.contains("Merged section: env"));
    }

    #[test]
    fn memory_report_uses_sectioned_layout() {
        let report = render_memory_report().expect("memory report should render");
        assert!(report.contains("Memory"));
        assert!(report.contains("Working directory"));
        assert!(report.contains("Instruction files"));
        assert!(report.contains("Discovered files"));
    }

    #[test]
    fn config_report_uses_sectioned_layout() {
        let report = render_config_report(None).expect("config report should render");
        assert!(report.contains("Config"));
        assert!(report.contains("Discovered files"));
        assert!(report.contains("Merged JSON"));
    }

    #[test]
    fn parses_git_status_metadata() {
        let (root, branch) = parse_git_status_metadata(Some(
            "## rcc/cli...origin/rcc/cli
 M src/main.rs",
        ));
        assert_eq!(branch.as_deref(), Some("rcc/cli"));
        let _ = root;
    }

    #[test]
    fn status_context_reads_real_workspace_metadata() {
        let context = status_context(None).expect("status context should load");
        assert!(context.cwd.is_absolute());
        assert_eq!(context.discovered_config_files, 5);
        assert!(context.loaded_config_files <= context.discovered_config_files);
    }

    #[test]
    fn normalizes_supported_permission_modes() {
        assert_eq!(normalize_permission_mode("read-only"), Some("read-only"));
        assert_eq!(
            normalize_permission_mode("workspace-write"),
            Some("workspace-write")
        );
        assert_eq!(
            normalize_permission_mode("danger-full-access"),
            Some("danger-full-access")
        );
        assert_eq!(normalize_permission_mode("unknown"), None);
    }

    #[test]
    fn clear_command_requires_explicit_confirmation_flag() {
        assert_eq!(
            SlashCommand::parse("/clear"),
            Some(SlashCommand::Clear { confirm: false })
        );
        assert_eq!(
            SlashCommand::parse("/clear --confirm"),
            Some(SlashCommand::Clear { confirm: true })
        );
    }

    #[test]
    fn parses_resume_and_config_slash_commands() {
        assert_eq!(
            SlashCommand::parse("/resume saved-session.json"),
            Some(SlashCommand::Resume {
                session_path: Some("saved-session.json".to_string())
            })
        );
        assert_eq!(
            SlashCommand::parse("/clear --confirm"),
            Some(SlashCommand::Clear { confirm: true })
        );
        assert_eq!(
            SlashCommand::parse("/config"),
            Some(SlashCommand::Config { section: None, set_key: None, set_value: None })
        );
        assert_eq!(
            SlashCommand::parse("/config env"),
            Some(SlashCommand::Config {
                section: Some("env".to_string()),
                set_key: None,
                set_value: None,
            })
        );
        assert_eq!(
            SlashCommand::parse("/config set model claude-opus-4-6"),
            Some(SlashCommand::Config {
                section: None,
                set_key: Some("model".to_string()),
                set_value: Some("claude-opus-4-6".to_string()),
            })
        );
        assert_eq!(SlashCommand::parse("/memory"), Some(SlashCommand::Memory));
        assert_eq!(SlashCommand::parse("/init"), Some(SlashCommand::Init));
    }

    #[test]
    fn init_template_mentions_detected_rust_workspace() {
        let rendered = crate::init::render_init_claude_md(std::path::Path::new("."));
        assert!(rendered.contains("# CLAUDE.md"));
        assert!(rendered.contains("cargo clippy --workspace --all-targets -- -D warnings"));
    }

    #[test]
    fn converts_tool_roundtrip_messages() {
        let messages = vec![
            ConversationMessage::user_text("hello"),
            ConversationMessage::assistant(vec![ContentBlock::ToolUse {
                id: "tool-1".to_string(),
                name: "bash".to_string(),
                input: "{\"command\":\"pwd\"}".to_string(),
            }]),
            ConversationMessage {
                role: MessageRole::Tool,
                blocks: vec![ContentBlock::ToolResult {
                    tool_use_id: "tool-1".to_string(),
                    tool_name: "bash".to_string(),
                    output: "ok".to_string(),
                    is_error: false,
                }],
                usage: None,
            },
        ];

        let converted = super::convert_messages(&messages);
        assert_eq!(converted.len(), 3);
        assert_eq!(converted[1].role, "assistant");
        assert_eq!(converted[2].role, "user");
    }
    #[test]
    fn repl_help_mentions_history_completion_and_multiline() {
        let help = render_repl_help();
        assert!(help.contains("Up/Down"));
        assert!(help.contains("Tab"));
        assert!(help.contains("Shift+Enter/Ctrl+J"));
    }

    #[test]
    fn tool_rendering_helpers_compact_output() {
        let start = format_tool_call_start("read_file", r#"{"path":"src/main.rs"}"#);
        assert!(start.contains("Tool call"));
        assert!(start.contains("src/main.rs"));

        let done = format_tool_result("read_file", r#"{"contents":"hello"}"#, false);
        assert!(done.contains("Tool `read_file`"));
        assert!(done.contains("contents"));
    }

    #[test]
    fn provider_default_model_differs_by_provider() {
        // Regression: run_prompt_json was ignoring provider and always using Anthropic.
        // This test ensures the Provider enum dispatches correctly.
        assert_eq!(Provider::Anthropic.default_model(), DEFAULT_MODEL);
        assert_eq!(Provider::OpenAi.default_model(), "codex-mini-latest");
        assert_eq!(Provider::Ollama.default_model(), "gemma4:31b-it-q4_K_M");
        assert_ne!(Provider::Anthropic.default_model(), Provider::OpenAi.default_model());
        assert_ne!(Provider::Anthropic.default_model(), Provider::Ollama.default_model());
    }

    #[test]
    fn provider_parse_accepts_known_aliases() {
        assert_eq!(Provider::parse("anthropic").unwrap(), Provider::Anthropic);
        assert_eq!(Provider::parse("claude").unwrap(), Provider::Anthropic);
        assert_eq!(Provider::parse("openai").unwrap(), Provider::OpenAi);
        assert_eq!(Provider::parse("codex").unwrap(), Provider::OpenAi);
        assert_eq!(Provider::parse("ollama").unwrap(), Provider::Ollama);
        assert_eq!(Provider::parse("local").unwrap(), Provider::Ollama);
        assert_eq!(Provider::parse("gemma").unwrap(), Provider::Ollama);
        assert!(Provider::parse("unknown").is_err());
    }

    #[test]
    fn provider_flag_selects_correct_default_model() {
        // Regression: --provider openai must use "codex-mini-latest", not the Anthropic default
        let action = parse_args(&["--provider".into(), "openai".into()]).unwrap();
        match action {
            CliAction::Repl { model, provider, .. } => {
                assert_eq!(provider, Provider::OpenAi);
                assert_eq!(model, "codex-mini-latest");
            }
            other => panic!("expected Repl, got {other:?}"),
        }
    }

    #[test]
    fn block_on_new_thread_avoids_nested_runtime_panic() {
        // Regression: calling block_on inside an existing tokio runtime panics.
        // block_on_new_thread must run the closure on a separate thread.
        let rt = tokio::runtime::Runtime::new().expect("outer runtime");
        rt.block_on(async {
            let result = super::block_on_new_thread(|| {
                let inner_rt = tokio::runtime::Runtime::new()
                    .map_err(|e| runtime::ToolError::new(e.to_string()))?;
                inner_rt.block_on(async { Ok(42) })
            });
            assert_eq!(result.unwrap(), 42);
        });
    }

    #[test]
    fn oauth_callback_body_uses_provider_label() {
        let success = super::oauth_callback_body("OpenAI Codex", false);
        assert!(success.contains("OpenAI Codex"), "got: {success}");
        assert!(success.contains("succeeded"), "got: {success}");
        assert!(!success.contains("Claude"), "should not contain Claude: {success}");

        let failure = super::oauth_callback_body("Claude", true);
        assert!(failure.contains("Claude"), "got: {failure}");
        assert!(failure.contains("failed"), "got: {failure}");
    }

    #[test]
    fn prompt_json_dispatches_to_correct_backend() {
        // Regression: run_prompt_json was hardcoded to Anthropic regardless of provider.
        // This tests the dispatch decision function directly.
        assert_eq!(super::prompt_json_backend(super::Provider::Anthropic), "anthropic");
        assert_eq!(super::prompt_json_backend(super::Provider::OpenAi), "openai");
        assert_eq!(super::prompt_json_backend(super::Provider::Ollama), "ollama");
        // Verify exhaustive match — if a new provider is added, this test must be updated
        assert_ne!(
            super::prompt_json_backend(super::Provider::Anthropic),
            super::prompt_json_backend(super::Provider::OpenAi),
        );
        assert_ne!(
            super::prompt_json_backend(super::Provider::Anthropic),
            super::prompt_json_backend(super::Provider::Ollama),
        );
    }
}
