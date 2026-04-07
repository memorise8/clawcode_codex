mod client;
mod error;
mod openai_auth;
mod openai_client;
pub mod openai_types;
mod sse;
mod types;

pub use client::{
    oauth_token_is_expired, read_base_url, resolve_saved_oauth_token,
    resolve_startup_auth_source, AnthropicClient, AuthSource, MessageStream, OAuthTokenSet,
};
pub use error::ApiError;
pub use sse::{parse_frame, SseParser};
pub use types::{
    ContentBlockDelta, ContentBlockDeltaEvent, ContentBlockStartEvent, ContentBlockStopEvent,
    InputContentBlock, InputMessage, MessageDelta, MessageDeltaEvent, MessageRequest,
    MessageResponse, MessageStartEvent, MessageStopEvent, OutputContentBlock, StreamEvent,
    ToolChoice, ToolDefinition, ToolResultContentBlock, Usage,
};
pub use openai_auth::{resolve_openai_auth, OpenAiCredentials};
pub use openai_client::{read_openai_base_url, OpenAiClient, OpenAiMessageStream, ResponsesStream};
pub use openai_types::{
    ChatCompletionChunk, ChatCompletionRequest, ChatCompletionResponse, ChatChunkChoice,
    ChatChoice, ChatDelta, ChatDeltaFunction, ChatDeltaToolCall, ChatFunction, ChatFunctionCall,
    ChatMessage, ChatTool, ChatToolCall, ChatToolChoice, ChatToolChoiceFunction, ChatUsage,
    StreamOptions,
    ResponsesRequest, ResponsesInput, ResponsesMessage, ResponsesContent, ResponsesContentPart,
    ResponsesTool, ResponsesResponse, ResponsesOutputItem, ResponsesOutputContent, ResponsesUsage,
    ResponsesFunctionCallInput, ResponsesFunctionCallOutputInput,
};
