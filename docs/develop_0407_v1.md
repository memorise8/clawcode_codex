# develop_0407_v1

## 목적

이 문서는 현재 프로젝트를 "Codex OAuth 기반 코딩 harness"에서 "완성형 플랫폼" 수준으로 끌어올리기 위해 우선적으로 손봐야 할 3개 축을 구현 관점에서 정리한 작업 메모다.

다루는 범위는 아래 3가지다.

1. 원격 MCP transport 재설계
2. 문서 및 지원 범위 정리
3. live provider + MCP E2E 테스트 체계 구축

---

## 1. 원격 MCP transport 재설계

### 현재 문제

현재 MCP 관련 구현은 [mcp_stdio.rs](/home/ruci/repo/develop/claw-code/rust/crates/runtime/src/mcp_stdio.rs)에 역할이 과도하게 몰려 있다.

- `stdio` 프로세스 실행 관리
- remote bootstrap
- JSON-RPC 요청/응답 처리
- transport별 예외 처리
- unsupported 기능 판정

이 구조에서는 transport가 늘어날수록 manager가 transport 세부 구현까지 알아야 하므로 유지보수가 어렵다. 특히 remote `SSE`, `headersHelper`, `oauth`는 현재 정식 지원 경로가 아니라 unsupported 처리에 가깝다.

### 수정 방향

핵심 방향은 `manager`를 얇게 만들고, transport를 독립 계층으로 분리하는 것이다.

- `McpServerManager`는 서버 정의를 읽고 적절한 transport client를 생성하는 역할만 맡는다.
- 실제 연결, initialize, list/call/read 로직은 transport별 구현체로 이동한다.
- 인증과 헤더 주입은 transport 분기가 아니라 공통 auth/decorator 계층으로 분리한다.

### 권장 구조

새 모듈을 기준으로 보면 아래 구조가 적절하다.

- `runtime/src/mcp_transport.rs`
- `runtime/src/mcp_transport_stdio.rs`
- `runtime/src/mcp_transport_http.rs`
- `runtime/src/mcp_transport_sse.rs`
- `runtime/src/mcp_auth.rs`

### 제안 인터페이스

공통 trait는 다음 정도로 고정하는 것이 좋다.

```rust
pub trait McpTransportClient {
    fn initialize(&mut self) -> Result<McpInitializeResult, McpServerManagerError>;
    fn list_tools(&mut self, cursor: Option<String>) -> Result<McpListToolsResult, McpServerManagerError>;
    fn call_tool(&mut self, params: McpToolCallParams) -> Result<McpToolCallResult, McpServerManagerError>;
    fn list_resources(&mut self, cursor: Option<String>) -> Result<McpListResourcesResult, McpServerManagerError>;
    fn read_resource(&mut self, uri: String) -> Result<McpReadResourceResult, McpServerManagerError>;
}
```

구현체는 아래처럼 나눈다.

- `StdioTransportClient`
- `HttpTransportClient`
- `SseTransportClient`

### 설정 계층 처리 방식

[config.rs](/home/ruci/repo/develop/claw-code/rust/crates/runtime/src/config.rs)에 있는 `McpServerConfig`는 그대로 유지할 수 있다. 대신 실제 연결 생성은 아래와 같은 팩토리로 모은다.

```rust
fn build_transport_client(
    name: &str,
    config: &ScopedMcpServerConfig,
) -> Result<Box<dyn McpTransportClient>, McpServerManagerError>
```

여기서 중요한 점은 `headersHelper`와 `oauth`를 transport 조건문에 묶지 않는 것이다.

- `headersHelper`는 요청 직전 헤더를 계산해 주는 helper 계층으로 분리
- `oauth`는 token refresh와 credential lookup을 담당하는 auth provider 계층으로 분리
- HTTP와 SSE는 같은 auth/decorator를 공유

### manager 책임 축소

`McpServerManager`는 아래 책임만 남기는 것이 맞다.

- 설정에서 MCP 서버 목록 읽기
- transport client 생성
- initialize 후 tool inventory 캐시
- `qualified_name`와 실제 `server/raw_tool_name` 매핑
- 사용자에게 보여줄 unsupported 또는 config error 정리

반대로 아래 책임은 manager에서 빼야 한다.

- 직접 HTTP 요청 조립
- SSE 프로토콜 세부 처리
- auth 흐름 처리
- headers helper 실행

### 테스트 전략

테스트는 unsupported 판정 중심이 아니라 실제 지원 경로 중심으로 바꿔야 한다.

- transport trait mock 테스트
- fake HTTP server 기반 initialize/list_tools/call_tool 테스트
- fake SSE server 기반 initialize/list_tools/call_tool 테스트
- oauth token 주입 테스트
- headers helper 반영 테스트
- discovery와 call이 동일 client state를 공유하는지 검증

### 완료 기준

아래를 만족하면 이 단계는 끝난 것으로 본다.

- `stdio/http/sse`가 동일 trait로 동작한다.
- manager가 transport 세부 구현을 모른다.
- `headersHelper`와 `oauth`가 transport 외부 계층으로 분리된다.
- unsupported 분기가 줄고 실제 지원 경로 테스트가 늘어난다.
- MCP discovery와 tool call이 같은 connection/state 모델을 공유한다.

---

## 2. 문서 및 지원 범위 정리

### 현재 문제

현재 README는 실제 제품 상태와 어긋난다. 특히 [README.md](/home/ruci/repo/develop/claw-code/README.md)에는 아직도 Rust가 진행 중이거나 Python-first 구조라는 설명이 남아 있는데, 실제 실동작 harness의 중심은 Rust 쪽에 가깝다.

이 상태는 사용자 기대치를 잘못 형성한다.

- 무엇이 공식 구현인지 불명확
- provider 지원 범위가 불명확
- MCP 지원 범위가 불명확
- 설정 파일 구조와 우선순위가 불명확

### 수정 방향

문서는 소개문이 아니라 "지원 계약" 역할을 해야 한다.

- 무엇을 공식 지원하는지
- 무엇은 실험적 기능인지
- 어떤 설정이 필요한지
- 어떤 제약이 있는지
- 어떻게 테스트하는지

이 다섯 가지가 문서에서 먼저 보여야 한다.

### 권장 문서 구조

루트 [README.md](/home/ruci/repo/develop/claw-code/README.md)는 짧고 명확하게 재작성한다.

포함해야 할 내용은 아래 정도면 충분하다.

- 프로젝트 소개
- 핵심 기능
- 지원 provider
- 지원 MCP transport
- 빠른 시작
- 자세한 문서 링크

상세 문서는 `docs/` 아래로 분리한다.

- `docs/auth.md`
- `docs/mcp.md`
- `docs/config.md`
- `docs/security.md`
- `docs/testing.md`

### 각 문서의 역할

`docs/auth.md`

- OpenAI OAuth 로그인
- Anthropic OAuth 로그인
- 저장 위치
- refresh 동작
- 실패 시 진단 방법

`docs/mcp.md`

- 지원 transport 매트릭스
- `stdio/http/sse/ws/sdk/claude-ai-proxy` 상태
- 각 transport 예시 설정
- auth/helper 적용 방식

`docs/config.md`

- `settings.json`
- `.claude/settings.json`
- `.claude/settings.local.json`
- 우선순위 규칙
- 주요 필드 설명

`docs/security.md`

- permission mode 설명
- tool allowlist
- child agent 상속 규칙
- secret/redaction 정책

`docs/testing.md`

- 로컬 테스트 방법
- live E2E 테스트 방법
- 필요한 환경변수
- flaky test 처리 규칙

### 코드와 문서의 정합성 유지 방식

문서는 코드보다 낙관적이면 안 된다.

그래서 아래 원칙을 적용하는 것이 좋다.

- 지원 transport 표는 코드 기준으로 맞춘다.
- README 예시는 실제 테스트 가능한 예시만 남긴다.
- 설정 예시는 `ConfigLoader` 동작과 일치해야 한다.
- 가능하면 문서 예시를 테스트로 검증한다.

예를 들면 아래 종류의 검증이 유효하다.

- README에 있는 CLI 예시가 smoke test로 실행됨
- 설정 파일 예시가 파싱 테스트로 검증됨
- 지원 transport 표가 enum과 실제 client 구현 상태와 일치함

### 완료 기준

아래를 만족하면 문서 정리는 끝난 것으로 본다.

- README가 현재 제품 상태를 정확히 반영한다.
- 새 사용자가 docs만 읽고 설치, 로그인, 설정, MCP 연결, 테스트 실행이 가능하다.
- 문서 예시와 코드 동작이 회귀 테스트로 연결된다.
- Python-first 설명과 Rust-in-progress 같은 오래된 서술이 제거된다.

---

## 3. live provider + MCP E2E 테스트 체계 구축

### 현재 문제

현재 테스트는 로컬 단위 테스트와 파이썬 검증은 괜찮지만, live provider 경계 검증은 약하다. 특히 provider integration 일부는 `ignore` 상태이며, 실제 서비스 경계에서 정기적으로 검증되는 구조가 아니다.

이 상태에서는 로컬 테스트가 통과해도 아래 리스크가 남는다.

- provider API 변경 감지 지연
- OAuth refresh 회귀 미탐지
- streaming 파손 미탐지
- MCP remote transport 회귀 미탐지

### 수정 방향

테스트 체계는 아래 2층으로 나누는 것이 좋다.

1. PR마다 도는 빠른 검증
2. 주기적으로 도는 live E2E 검증

### 빠른 검증 구성

빠른 검증은 mock/fake 기반으로 유지한다.

- `cargo fmt --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test --workspace`
- `python3 -m unittest discover -s tests -v`

이 단계는 PR마다 반드시 돌아야 한다.

### live E2E 구성

live E2E는 GitHub Actions의 nightly 또는 manual dispatch로 분리한다.

현재 repo에는 workflow가 사실상 없으므로 `.github/workflows/`를 새로 잡는 것이 맞다.

권장 파일 구성은 아래와 같다.

- `.github/workflows/ci.yml`
- `.github/workflows/nightly-provider-live.yml`
- `.github/workflows/nightly-mcp-live.yml`

### provider live 테스트 범위

provider 관련 live 테스트는 crate를 분리하는 편이 관리가 쉽다.

- `rust/crates/api/tests/live_openai.rs`
- `rust/crates/api/tests/live_anthropic.rs`

다뤄야 할 시나리오는 아래다.

- 저장된 credential 해석
- access token 만료 후 refresh
- 기본 streaming 응답
- tool call이 포함된 응답
- second turn 또는 session continuity

### MCP live 테스트 범위

MCP는 runtime 기준으로 live/fake hybrid 구성이 좋다.

- `rust/crates/runtime/tests/live_mcp_http.rs`
- `rust/crates/runtime/tests/live_mcp_sse.rs`

다뤄야 할 시나리오는 아래다.

- initialize
- list_tools
- call_tool
- list_resources
- read_resource
- auth 적용
- headers helper 적용

### 실행 정책

live 테스트는 PR blocking으로 두지 않는 것이 좋다.

- PR에서는 빠른 검증만 수행
- main branch nightly에서 live 수행
- manual dispatch로 운영자가 직접 재실행 가능
- secret이 없으면 graceful skip

### 실패 시 운영 원칙

live 테스트는 실패했을 때 정보가 남아야 의미가 있다.

- redacted log artifact 업로드
- provider 이름
- transport 종류
- 실패 단계
- request id 또는 trace id

flaky test는 무조건 `ignore`로 돌리지 말고 아래 순서로 처리한다.

1. retry 정책 적용
2. 환경 의존성 분리
3. 원인 분석 후 test tag 재조정

### 완료 기준

아래를 만족하면 테스트 체계는 플랫폼 수준에 근접한다.

- PR 검증과 live 검증이 분리되어 있다.
- provider 핵심 시나리오가 정기적으로 실제 네트워크에서 돈다.
- MCP HTTP/SSE 경로가 정기적으로 검증된다.
- 실패 시 원인 추적에 필요한 로그가 남는다.
- live test가 `ignore` 상태로 방치되지 않는다.

---

## 권장 실행 순서

이 세 가지는 아래 순서로 진행하는 것이 가장 안전하다.

1. 원격 MCP transport 재설계
2. 문서 및 지원 범위 정리
3. live provider + MCP E2E 테스트 체계 구축

이 순서를 권장하는 이유는 문서와 E2E 테스트가 결국 transport 구조가 고정된 뒤에야 안정되기 때문이다.

---

## 한 줄 결론

현재 프로젝트는 핵심 harness는 이미 충분히 성립해 있다. 완성형 플랫폼으로 가려면 기능을 더 붙이는 것보다, MCP transport를 계층화하고, 문서를 지원 계약으로 재작성하고, live E2E 검증을 운영 체계로 끌어올리는 것이 우선이다.
