//! GitLab Duo Agent (`gitlab-duo-agent`): the Duo Workflow Service over an inline `ambient` flow.
//!
//! OMP (`providers/gitlab-duo-workflow.ts`) keeps one workflow alive across a whole tool loop and
//! answers each `runMCPTool` action on the live socket. GreenCode's provider API is one stateless
//! turn, so every `complete` resolves the namespace, creates a FRESH workflow, sends the whole
//! conversation as the goal (bare text for one turn, a ChatML transcript after that), streams the
//! checkpoint text, returns the tool call, and stops the workflow. The next turn replays the
//! transcript, tool results included, so no socket or `actionResponse` has to be kept.

use super::{limit, ws, Provider, Reply, Request, Wire};
use crate::{Block, BoxFut, Message, Role, Usage};
use anyhow::{anyhow, bail, Context, Result};
use fancy_regex::Regex;
use serde_json::{json, Map, Value};
use std::{
    collections::{HashMap, HashSet},
    fmt::Write as _,
    sync::{LazyLock, Mutex},
    time::Duration,
};

pub(crate) const BASE: &str = "https://gitlab.com";
const DEFINITION: &str = "ambient";
const CLIENT_TYPE: &str = "node-websocket";
const LANGUAGE_SERVER_VERSION: &str = "8.104.0";
const AGENT: &str = "greencode_agent";
const PROMPT: &str = "greencode_inline_prompt";
const IDLE_TIMEOUT: Duration = Duration::from_secs(90);
const REST_TIMEOUT: Duration = Duration::from_secs(30);
/// The service's gRPC `MAX_MESSAGE_SIZE` is 4 MB; past ~2 MB a goal basically always fails.
const HARD_GOAL_BYTES: usize = 2_000_000;
const CAPABILITIES: [&str; 5] =
    ["incremental_streaming", "read_file_chunked", "shell_command", "command_timeout", "tool_call_approval"];
const UI_LOG_EVENTS: [&str; 4] =
    ["on_agent_reasoning", "on_agent_final_answer", "on_tool_execution_success", "on_tool_execution_failed"];

const MODELS_QUERY: &str = r#"query greencode_duoAgentAvailableModels($rootNamespaceId: GroupID!) {
  aiChatAvailableModels(rootNamespaceId: $rootNamespaceId) {
    defaultModel { name ref }
    selectableModels { name ref }
    pinnedModel { name ref }
  }
}"#;

const ROOT_NAMESPACE_QUERY: &str = r#"query greencode_duoAgentProjectRootNamespace($fullPath: ID!) {
  project(fullPath: $fullPath) {
    namespace { rootAncestor { id } }
  }
}"#;

/// Tells the model the goal's ChatML/`<ran>` markers are a record, not emittable call grammar.
const CHATML_NOTE: &str = "The task below is a transcript of the conversation so far, written as a plain-text log. \
Turn boundaries (`<|im_start|>role` … `<|im_end|>`) and any `<ran NAME>{…}</ran>` / `<ran:result>` entries inside it \
are a RECORD of what already happened — past tool calls and their results. They are not a syntax for you to emit. \
To call a tool, use your normal structured tool-calling channel; never write `<ran …>`, `<tool_call>`, `<|im_start|>`, \
or similar markers as your own output.";

/// Groups whose Duo settings this process already enabled.
static ENSURED: LazyLock<Mutex<HashSet<(String, String)>>> = LazyLock::new(Default::default);

pub struct Workflow {
    http: reqwest::Client,
    base: String,
    /// The GitLab OAuth access token.
    key: String,
    wire: Wire,
    redactor: Option<crate::redact::Redactor>,
}

impl Workflow {
    pub fn new(http: reqwest::Client, base: String, key: String, wire: Wire) -> Self {
        let base = if base.trim().is_empty() { BASE.to_string() } else { base.trim_end_matches('/').to_string() };
        Self { http, base, key, wire, redactor: crate::redact::Redactor::new(&[]).ok() }
    }

    fn api(&self) -> Api<'_> {
        Api { http: &self.http, base: &self.base, token: &self.key }
    }

    fn redacted(&self, text: &str) -> String {
        self.redactor.as_ref().map_or_else(|| text.to_string(), |redactor| redactor.text(text))
    }

    fn idle(&self) -> Duration {
        self.wire.timeouts.idle.unwrap_or(IDLE_TIMEOUT)
    }

    async fn turn(&self, req: Request<'_>, on_text: &(dyn Fn(&str) + Send + Sync)) -> Result<Reply> {
        let api = self.api();
        let scope = api.scope().await?;
        api.settings(&scope).await;
        let turns = conversation(req.messages);
        // The flow's system slot carries our own prompt; the goal carries only the conversation.
        let system = if turns.len() > 1 { format!("{}\n\n{CHATML_NOTE}", req.system) } else { req.system.to_string() };
        let system = sanitize_prompt(&self.redacted(&system))?;
        let goal = self.redacted(&goal(&turns, req.messages));
        if goal.len() >= HARD_GOAL_BYTES {
            bail!(
                "GitLab Duo Agent の goal が大きすぎます（{} バイト、上限 {}）。会話を圧縮してから再試行してください",
                goal.len(),
                HARD_GOAL_BYTES
            )
        }
        let connection = api.direct_access(&scope).await?;
        let workflow = api.create(&scope).await?;
        let result = self.run(&api, &scope, &connection, &workflow, &req, &goal, &system, on_text).await;
        api.stop(&workflow).await;
        result
    }

    #[allow(clippy::too_many_arguments)]
    async fn run(
        &self,
        api: &Api<'_>,
        scope: &Scope,
        connection: &Connection,
        workflow: &str,
        req: &Request<'_>,
        goal: &str,
        system: &str,
        on_text: &(dyn Fn(&str) + Send + Sync),
    ) -> Result<Reply> {
        let available = api.available_models(scope).await;
        let model = text(&available["pinnedModel"]["ref"]).unwrap_or_else(|| req.model.to_string());
        let mut payload = start_request(scope, &model, workflow, req, goal, system);
        let url = self.socket_url(connection, scope, &model);
        let owned = self.headers(connection, scope);
        let headers: Vec<(&str, String)> = owned.iter().map(|(name, value)| (name.as_str(), value.clone())).collect();
        let idle = self.idle();
        let mut acc = Acc::default();
        let mut approved = false;
        loop {
            let mut socket = ws::Ws::connect(&url, &headers).await?;
            socket.send_json(&json!({ "startRequest": payload })).await?;
            loop {
                let frame = match socket.next_text(Some(idle)).await {
                    Ok(Some(frame)) => frame,
                    Ok(None) => bail!("GitLab Duo Agent の接続が閉じられました"),
                    Err(error) => bail!("GitLab Duo Agent の応答が {} 秒途切れました: {error}", idle.as_secs()),
                };
                let Ok(event) = serde_json::from_str::<Value>(&frame) else { continue };
                let Some(event) = event.as_object() else { continue };
                match advance(event, &mut acc, on_text)? {
                    Frame::Ignore => {}
                    // OMP answers an approval by reconnecting with a startRequest that carries it.
                    Frame::Approval => {
                        if approved {
                            bail!("GitLab Duo Agent が承認を繰り返し要求しています。GitLab でワークフローを確認してください")
                        }
                        approved = true;
                        payload["goal"] = json!("");
                        payload["additional_context"] = json!([]);
                        payload["approval"] = json!({ "approval": {} });
                        break
                    }
                    Frame::Finished => return Ok(acc.reply("end_turn")),
                    Frame::Tool(block) => {
                        acc.blocks.push(block);
                        return Ok(acc.reply("tool_use"))
                    }
                }
            }
        }
    }

    fn socket_url(&self, connection: &Connection, scope: &Scope, model: &str) -> String {
        let service = connection.base.is_some();
        let root = match &connection.base {
            Some(base) => format!("{}/", base.trim_end_matches('/')),
            None => api_url(&self.base, "/api/v4/ai/duo_workflows/ws"),
        };
        let mut params = Vec::new();
        if let Some(project) = &scope.project {
            params.push(format!("project_id={}", encode(project)));
        }
        if !service {
            params.push(format!("namespace_id={}", encode(&scope.rest())));
        }
        params.push(format!("root_namespace_id={}", encode(&scope.rest())));
        params.push(format!("user_selected_model_identifier={}", encode(model)));
        params.push(format!("workflow_definition={DEFINITION}"));
        ws::socket_url(&format!("{root}?{}", params.join("&")))
    }

    fn headers(&self, connection: &Connection, scope: &Scope) -> Vec<(String, String)> {
        let mut headers = connection.headers.clone();
        let push = |headers: &mut Vec<(String, String)>, name: &str, value: String| headers.push((name.to_string(), value));
        push(&mut headers, "authorization", format!("Bearer {}", connection.token));
        push(&mut headers, "x-gitlab-client-type", CLIENT_TYPE.to_string());
        push(&mut headers, "x-gitlab-language-server-version", LANGUAGE_SERVER_VERSION.to_string());
        push(
            &mut headers,
            "user-agent",
            format!("unknown/unknown unknown/unknown gitlab-language-server/{LANGUAGE_SERVER_VERSION}"),
        );
        if let Some(origin) = reqwest::Url::parse(&self.base).ok().map(|url| {
            let port = url.port().map(|port| format!(":{port}")).unwrap_or_default();
            format!("{}://{}{port}", url.scheme(), url.host_str().unwrap_or_default())
        }) {
            push(&mut headers, "origin", origin);
        }
        if let Some(project) = &scope.project {
            push(&mut headers, "x-gitlab-project-id", project.clone());
        }
        push(&mut headers, "x-gitlab-namespace-id", scope.rest());
        push(&mut headers, "x-gitlab-root-namespace-id", scope.rest());
        headers
    }
}

impl Provider for Workflow {
    fn complete<'a>(
        &'a self,
        req: Request<'a>,
        on_text: &'a (dyn Fn(&str) + Send + Sync),
    ) -> BoxFut<'a, Result<Reply>> {
        Box::pin(async move { self.turn(req, on_text).await })
    }
}

// ---- REST / GraphQL ----

struct Api<'a> {
    http: &'a reqwest::Client,
    base: &'a str,
    token: &'a str,
}

impl Api<'_> {
    async fn get(&self, path: &str) -> Result<Value> {
        let response = self
            .http
            .get(self.url(path))
            .bearer_auth(self.token)
            .timeout(REST_TIMEOUT)
            .send()
            .await
            .with_context(|| format!("GitLab: GET {path} に接続できませんでした"))?;
        if !response.status().is_success() {
            bail!("GitLab: GET {path} が HTTP {} を返しました", response.status())
        }
        Ok(response.json().await?)
    }

    async fn post(&self, path: &str, body: &Value) -> Result<Value> {
        let response = self
            .http
            .post(self.url(path))
            .bearer_auth(self.token)
            .timeout(REST_TIMEOUT)
            .json(body)
            .send()
            .await
            .with_context(|| format!("GitLab: POST {path} に接続できませんでした"))?;
        let status = response.status();
        if !status.is_success() {
            let headers = response.headers().clone();
            let text = response.text().await.unwrap_or_default();
            if status == reqwest::StatusCode::FORBIDDEN {
                bail!("GitLab で Duo Agent Platform が有効になっていません（HTTP 403）。グループの設定を確認してください")
            }
            return Err(limit::http_error(status, &headers, &text));
        }
        Ok(response.json().await?)
    }

    async fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        let response = self
            .http
            .post(self.url("/api/graphql"))
            .bearer_auth(self.token)
            .timeout(REST_TIMEOUT)
            .json(&json!({ "query": query, "variables": variables }))
            .send()
            .await
            .context("GitLab GraphQL に接続できませんでした")?;
        if !response.status().is_success() {
            bail!("GitLab GraphQL が HTTP {} を返しました", response.status())
        }
        Ok(response.json().await?)
    }

    fn url(&self, path: &str) -> String {
        api_url(self.base, path)
    }

    /// Root namespace and, when one resolves, the project the ambient flow needs.
    async fn scope(&self) -> Result<Scope> {
        let project_reference = env("GITLAB_DUO_PROJECT_ID")
            .or_else(|| env("GITLAB_DUO_PROJECT_PATH"))
            .or_else(|| remote_project(self.base));
        let explicit = env("GITLAB_DUO_NAMESPACE_ID");
        if let Some(project) = match &project_reference {
            Some(reference) => self.project(reference).await.ok(),
            None => None,
        } {
            let root = self.root_of(&project).await;
            return Ok(Scope {
                group: project.namespace_path.clone().unwrap_or_else(|| project.namespace_id.clone()),
                root,
                project: Some(project.id),
                project_ref: Some(project.path),
            })
        }
        let root = match explicit {
            Some(namespace) => rest_id(&namespace),
            None => self.first_group().await?,
        };
        let project = self.group_project(&root).await;
        Ok(Scope { group: root.clone(), root, project: project.as_ref().map(|p| p.id.clone()), project_ref: project.map(|p| p.path) })
    }

    async fn project(&self, reference: &str) -> Result<Project> {
        let payload = self.get(&format!("/api/v4/projects/{}", encode(reference))).await?;
        project_from(&payload).ok_or_else(|| anyhow!("GitLab のプロジェクト `{reference}` を解決できませんでした"))
    }

    async fn root_of(&self, project: &Project) -> String {
        let found = self
            .graphql(ROOT_NAMESPACE_QUERY, json!({ "fullPath": project.path }))
            .await
            .ok()
            .and_then(|payload| text(&payload["data"]["project"]["namespace"]["rootAncestor"]["id"]));
        found.map(|id| rest_id(&id)).unwrap_or_else(|| project.namespace_id.clone())
    }

    async fn first_group(&self) -> Result<String> {
        let payload = self.get("/api/v4/groups?top_level_only=true&per_page=100&order_by=name&sort=asc").await?;
        let groups = payload.as_array().ok_or_else(|| anyhow!("GitLab のグループ一覧が想定外の応答でした"))?;
        let preferred = groups
            .iter()
            .find(|group| group["duo_features_enabled"] == true || group["duo_core_features_enabled"] == true)
            .or_else(|| groups.first());
        preferred
            .and_then(|group| text(&group["id"]))
            .ok_or_else(|| anyhow!("GitLab Duo Agent の名前空間が見つかりません。GITLAB_DUO_NAMESPACE_ID を設定してください"))
    }

    /// The ambient flow fails server-side without a project; pick one the token can reach.
    async fn group_project(&self, namespace: &str) -> Option<Project> {
        let query = "per_page=1&min_access_level=30&order_by=last_activity_at&sort=desc";
        let endpoints = [
            format!("/api/v4/groups/{}/projects?include_subgroups=true&{query}", encode(namespace)),
            format!("/api/v4/projects?membership=true&{query}"),
        ];
        for endpoint in endpoints {
            let Ok(payload) = self.get(&endpoint).await else { continue };
            if let Some(project) = payload.as_array().and_then(|projects| projects.first()).and_then(project_from) {
                return Some(project)
            }
        }
        None
    }

    /// Best effort, once per group: the inline flow needs the agent-platform + MCP flags on.
    async fn settings(&self, scope: &Scope) {
        let key = (self.base.to_string(), scope.rest());
        if ENSURED.lock().unwrap().contains(&key) {
            return
        }
        let body = json!({
            "experiment_features_enabled": true,
            "ai_settings_attributes": { "duo_agent_platform_enabled": true, "duo_workflow_mcp_enabled": true },
        });
        let response = self
            .http
            .put(self.url(&format!("/api/v4/groups/{}", encode(&scope.rest()))))
            .bearer_auth(self.token)
            .timeout(REST_TIMEOUT)
            .json(&body)
            .send()
            .await;
        // Any HTTP answer below 500 is definitive: a 4xx is missing rights, which retrying never fixes.
        if response.as_ref().is_ok_and(|response| response.status().as_u16() < 500) {
            ENSURED.lock().unwrap().insert(key);
        }
    }

    async fn direct_access(&self, scope: &Scope) -> Result<Connection> {
        let mut body = json!({ "workflow_definition": DEFINITION, "root_namespace_id": gid(&scope.root) });
        if let Some(project) = &scope.project_ref {
            body["project_id"] = json!(project);
        }
        let payload = self.post("/api/v4/ai/duo_workflows/direct_access", &body).await?;
        let token = ["gitlab_rails", "duo_workflow_service"]
            .iter()
            .find_map(|key| text(&payload[key]["token"]))
            .or_else(|| {
                ["duo_workflow_access_token", "workflow_token", "token", "access_token", "jwt"]
                    .iter()
                    .find_map(|key| text(&payload[key]))
            })
            .ok_or_else(|| anyhow!("GitLab Duo Agent の direct_access がトークンを返しませんでした"))?;
        // A service endpoint (runway host) replaces the GitLab Rails socket path, headers included.
        let service = payload["gitlab_rails"]["token"].is_null()
            && text(&payload["duo_workflow_service"]["base_url"]).is_some();
        let (base, headers) = if service {
            let base = text(&payload["duo_workflow_service"]["base_url"])
                .map(|base| normalize_service_base(&base))
                .unwrap_or_default();
            let headers = payload["duo_workflow_service"]["headers"]
                .as_object()
                .map(|headers| headers.iter().filter_map(|(k, v)| text(v).map(|v| (k.clone(), v))).collect())
                .unwrap_or_default();
            (Some(base), headers)
        } else {
            (None, Vec::new())
        };
        Ok(Connection { token, base, headers })
    }

    async fn create(&self, scope: &Scope) -> Result<String> {
        let mut body = json!({
            "workflow_definition": DEFINITION,
            "environment": "ide",
            "allow_agent_to_request_user": false,
            "agent_privileges": [6],
            "pre_approved_agent_privileges": [6],
            "requires_duo_cli_enabled": false,
            // The inline flow carries the conversation in `startRequest.goal`, never in the workflow.
            "goal": "",
        });
        match &scope.project_ref {
            Some(project) => body["project_id"] = json!(project),
            None => body["namespace_id"] = json!(scope.group),
        }
        let payload = self.post("/api/v4/ai/duo_workflows/workflows", &body).await?;
        ["id", "workflow_id", "workflowId"]
            .iter()
            .find_map(|key| text(&payload[key]))
            .ok_or_else(|| anyhow!("GitLab Duo Agent のワークフロー作成が ID を返しませんでした"))
    }

    /// Best effort: the turn is already finished, so a stop that fails must not surface.
    async fn stop(&self, workflow: &str) {
        let _ = self
            .http
            .patch(self.url(&format!("/api/v4/ai/duo_workflows/workflows/{}", encode(workflow))))
            .bearer_auth(self.token)
            .timeout(REST_TIMEOUT)
            .json(&json!({ "status_event": "stop" }))
            .send()
            .await;
    }

    async fn available_models(&self, scope: &Scope) -> Value {
        self.graphql(MODELS_QUERY, json!({ "rootNamespaceId": gid(&scope.root) }))
            .await
            .ok()
            .and_then(|payload| payload.get("data").and_then(|data| data.get("aiChatAvailableModels")).cloned())
            .unwrap_or(Value::Null)
    }
}

#[derive(Clone, Debug)]
struct Scope {
    root: String,
    group: String,
    /// Numeric project id, for WebSocket routing.
    project: Option<String>,
    /// Project path (or id) the REST calls carry.
    project_ref: Option<String>,
}

impl Scope {
    fn rest(&self) -> String {
        rest_id(&self.root)
    }
}

#[derive(Clone, Debug)]
struct Project {
    id: String,
    path: String,
    namespace_id: String,
    namespace_path: Option<String>,
}

fn project_from(value: &Value) -> Option<Project> {
    Some(Project {
        id: text(&value["id"])?,
        path: text(&value["path_with_namespace"])?,
        namespace_id: text(&value["namespace"]["id"]).unwrap_or_default(),
        namespace_path: text(&value["namespace"]["full_path"]),
    })
}

struct Connection {
    token: String,
    /// The Duo Workflow Service host, when direct_access hands one out.
    base: Option<String>,
    headers: Vec<(String, String)>,
}

/// Model refs the GraphQL catalog offers, as bare ids (for the picker).
pub async fn models(base: &str, token: &str) -> Result<Vec<String>> {
    let http = reqwest::Client::new();
    let api = Api { http: &http, base, token };
    let scope = api.scope().await?;
    Ok(model_refs(&api.available_models(&scope).await))
}

fn model_refs(available: &Value) -> Vec<String> {
    if let Some(pinned) = text(&available["pinnedModel"]["ref"]) {
        return vec![pinned]
    }
    let selectable: Vec<String> = available["selectableModels"]
        .as_array()
        .map(|models| models.iter().filter_map(|model| text(&model["ref"])).collect())
        .unwrap_or_default();
    if !selectable.is_empty() {
        return selectable
    }
    text(&available["defaultModel"]["ref"]).into_iter().collect()
}

// ---- startRequest / flow config ----

fn start_request(scope: &Scope, model: &str, workflow: &str, request: &Request<'_>, goal: &str, system: &str) -> Value {
    let mcp_tools: Vec<Value> = request
        .tools
        .iter()
        .map(|tool| {
            let schema = if tool.schema.is_object() {
                tool.schema.clone()
            } else {
                json!({ "type": "object", "properties": {}, "required": [] })
            };
            json!({
                "name": tool.name,
                "originalToolName": tool.name,
                "serverName": "greencode",
                "description": tool.description,
                "inputSchema": schema.to_string(),
                "isApproved": true,
            })
        })
        .collect();
    let preapproved: Vec<&str> = request.tools.iter().map(|tool| tool.name.as_str()).collect();
    let mut metadata = json!({
        "environment": "ide",
        "client_type": CLIENT_TYPE,
        "namespaceId": scope.rest(),
        "rootNamespaceId": scope.rest(),
        "selectedModelIdentifier": model,
    });
    if let Some(project) = &scope.project {
        metadata["projectId"] = json!(project);
    }
    json!({
        "workflowID": workflow,
        "clientVersion": "1.0",
        "workflowDefinition": DEFINITION,
        "goal": goal,
        "workflowMetadata": metadata.to_string(),
        "additional_context": [],
        "clientCapabilities": CAPABILITIES,
        "mcpTools": mcp_tools,
        "preapproved_tools": preapproved,
        "flowConfigSchemaVersion": "v1",
        "flowConfig": flow_config(system),
    })
}

fn flow_config(system: &str) -> Value {
    json!({
        "version": "v1",
        "environment": "ambient",
        "flow": { "entry_point": AGENT },
        "components": [{
            "name": AGENT,
            "type": "AgentComponent",
            "prompt_id": PROMPT,
            "toolset": [],
            "inputs": [{ "from": "context:goal", "as": "goal" }],
            "ui_log_events": UI_LOG_EVENTS,
        }],
        "routers": [{ "from": AGENT, "to": "end" }],
        "prompts": [{
            "name": PROMPT,
            "prompt_id": PROMPT,
            "unit_primitives": ["duo_agent_platform"],
            "prompt_template": { "system": system, "user": "{{goal}}", "placeholder": "history" },
        }],
    })
}

/// The service rejects (WebSocket 4400) any inline-flow prompt its own sanitizer would change, so
/// apply its rewrites first: strip Unicode tag characters and HTML comments, escape the prompt tags
/// it rewrites, and neutralize Markdown comment markers.
fn sanitize_prompt(prompt: &str) -> Result<String> {
    let mut text = prompt.to_string();
    // Every rewrite is either a removal or idempotent escaping, so a couple of passes converge.
    for _ in 0..16 {
        let mut next = UNICODE_TAG_ESCAPES.replace_all(&text, "").into_owned();
        next = UNICODE_TAGS.replace_all(&next, "").into_owned();
        next = HTML_COMMENT.replace_all(&next, "").into_owned();
        next = COMMENT_ESCAPE.replace_all(&next, "&lt;!--").into_owned();
        next = MARKDOWN_MARKER.replace_all(&next, "[${1}]\u{200b}:").into_owned();
        for (plain, escaped, tag) in TAG_PATTERNS.iter() {
            let replacement = format!("&lt;${{1}}{tag}&gt;");
            next = plain.replace_all(&next, replacement.as_str()).into_owned();
            next = escaped.replace_all(&next, replacement.as_str()).into_owned();
        }
        if next == text {
            return Ok(text)
        }
        text = next;
    }
    bail!("GitLab Duo のシステムプロンプトを安全な形に変換できませんでした")
}

const TAG_WS: &str = r"[\s\x{1c}-\x{1f}\x{85}]";
static UNICODE_TAGS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[\x{E0000}-\x{E007F}\x{E0100}-\x{E01EF}]").expect("unicode tag pattern"));
static UNICODE_TAG_ESCAPES: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\\udb40\\ud[c-f][0-9a-f]{2}").expect("escaped unicode tag pattern"));
static HTML_COMMENT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)<!--.*?-->").expect("html comment pattern"));
static COMMENT_ESCAPE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\\+u003c!--").expect("escaped html comment pattern"));
static MARKDOWN_MARKER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\[(//|comment)\]:").expect("markdown comment pattern"));
static TAG_PATTERNS: LazyLock<Vec<(Regex, Regex, &'static str)>> = LazyLock::new(|| {
    ["goal", "system", "truncated_tool_output", "truncation_notice"]
        .into_iter()
        .map(|tag| {
            // Python's `re.IGNORECASE` folds ſ→s and ı/İ→i; spelling the classes out keeps that.
            let name: String = tag
                .chars()
                .map(|c| match c {
                    's' => "[sſ]".to_string(),
                    'i' => "[iıİ]".to_string(),
                    c => c.to_string(),
                })
                .collect();
            let plain = Regex::new(&format!(r"(?i)<{TAG_WS}*(/?){TAG_WS}*{name}{TAG_WS}*>")).expect("tag pattern");
            let escaped = Regex::new(&format!(
                r"(?i)\\\\{{1,2}}u003c{TAG_WS}*(/?){TAG_WS}*{name}{TAG_WS}*\\\\{{1,2}}u003e"
            ))
            .expect("escaped tag pattern");
            (plain, escaped, tag)
        })
        .collect()
});

// ---- goal rendering ----

enum Turn {
    User(String),
    Assistant(String, Vec<(String, Value)>),
    Tool(String, bool),
}

impl Turn {
    fn role(&self) -> &'static str {
        match self {
            Turn::User(_) => "user",
            Turn::Assistant(..) => "assistant",
            Turn::Tool(..) => "tool",
        }
    }
}

fn conversation(messages: &[Message]) -> Vec<Turn> {
    let mut turns = Vec::new();
    for message in messages {
        match message.role {
            Role::User => {
                let mut texts: Vec<&str> = Vec::new();
                for block in &message.content {
                    match block {
                        Block::Text { text } => texts.push(text),
                        Block::Other(value) if value["type"] == "image" => texts.push("[image]"),
                        Block::ToolResult { content, is_error, .. } => {
                            flush_user(&mut turns, &mut texts);
                            turns.push(Turn::Tool(content.clone(), *is_error));
                        }
                        _ => {}
                    }
                }
                flush_user(&mut turns, &mut texts);
            }
            Role::Assistant => {
                let mut texts: Vec<&str> = Vec::new();
                let mut calls = Vec::new();
                for block in &message.content {
                    match block {
                        Block::Text { text } => texts.push(text),
                        Block::ToolUse { name, input, .. } => calls.push((name.clone(), input.clone())),
                        _ => {}
                    }
                }
                let text = texts.join("\n");
                if !text.is_empty() || !calls.is_empty() {
                    turns.push(Turn::Assistant(text, calls))
                }
            }
        }
    }
    turns
}

fn flush_user(turns: &mut Vec<Turn>, texts: &mut Vec<&str>) {
    if texts.is_empty() {
        return
    }
    turns.push(Turn::User(texts.join("\n")));
    texts.clear();
}

/// One turn rides as bare text; a real conversation becomes the flat ChatML transcript.
fn goal(turns: &[Turn], messages: &[Message]) -> String {
    if turns.len() > 1 {
        return turns.iter().map(render_turn).collect::<Vec<_>>().join("\n")
    }
    latest_user_text(messages).unwrap_or_default()
}

fn latest_user_text(messages: &[Message]) -> Option<String> {
    for message in messages.iter().rev() {
        if message.role != Role::User {
            continue
        }
        let text = message
            .content
            .iter()
            .filter_map(|block| match block {
                Block::Text { text } => Some(text.as_str()),
                Block::Other(value) if value["type"] == "image" => Some("[image]"),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        if !text.is_empty() {
            return Some(text)
        }
    }
    None
}

fn render_turn(turn: &Turn) -> String {
    let body = match turn {
        Turn::User(text) => format!("{text}\n"),
        Turn::Assistant(text, calls) => {
            let mut parts: Vec<String> = Vec::new();
            if !text.is_empty() {
                parts.push(text.clone());
            }
            for (name, input) in calls {
                // Past-tense record: deliberately not the `{name, arguments}` shape of a live call.
                parts.push(format!("<ran {name}>{input}</ran>"));
            }
            format!("{}\n", parts.join("\n"))
        }
        Turn::Tool(content, is_error) => {
            format!("<ran:result{}>\n{content}\n", if *is_error { " status=error" } else { "" })
        }
    };
    format!("<|im_start|>{}\n{body}<|im_end|>", turn.role())
}

// ---- inbound frames ----

enum Frame {
    Ignore,
    Approval,
    Finished,
    Tool(Block),
}

#[derive(Default)]
struct Acc {
    blocks: Vec<Block>,
    open_text: Option<usize>,
    open_thinking: Option<usize>,
    /// Full content already emitted per checkpoint message, so a replayed snapshot adds no delta.
    seen: HashMap<String, String>,
    usage: Usage,
}

impl Acc {
    fn reply(self, stop: &str) -> Reply {
        Reply { blocks: self.blocks, usage: self.usage, stop: stop.into() }
    }

    fn apply(&mut self, entries: Vec<Entry>, on_text: &(dyn Fn(&str) + Send + Sync)) {
        for entry in entries {
            if entry.kind == Kind::Boundary {
                self.open_text = None;
                self.open_thinking = None;
                continue
            }
            let delta = match self.seen.get(&entry.key).map(String::as_str) {
                None => entry.content.clone(),
                Some(previous) if previous == entry.content => String::new(),
                // A server-side rewrite of an already emitted message is dropped, not replayed.
                Some(previous) => entry.content.strip_prefix(previous).map(str::to_string).unwrap_or_default(),
            };
            let kind = entry.kind;
            self.seen.insert(entry.key, entry.content);
            match kind {
                Kind::Text => self.text(&delta, on_text),
                Kind::Thinking => self.thinking(&delta),
                Kind::Boundary => {}
            }
        }
    }

    fn text(&mut self, delta: &str, on_text: &(dyn Fn(&str) + Send + Sync)) {
        if delta.is_empty() {
            return
        }
        self.open_thinking = None;
        let index = match self.open_text {
            Some(index) => index,
            None => {
                self.blocks.push(Block::Text { text: String::new() });
                self.open_text = Some(self.blocks.len() - 1);
                self.blocks.len() - 1
            }
        };
        if let Block::Text { text } = &mut self.blocks[index] {
            text.push_str(delta);
        }
        on_text(delta);
    }

    fn thinking(&mut self, delta: &str) {
        if delta.is_empty() {
            return
        }
        self.open_text = None;
        let index = match self.open_thinking {
            Some(index) => index,
            None => {
                self.blocks.push(Block::Other(json!({ "type": "thinking", "thinking": "" })));
                self.open_thinking = Some(self.blocks.len() - 1);
                self.blocks.len() - 1
            }
        };
        if let Block::Other(value) = &mut self.blocks[index] {
            let combined = format!("{}{delta}", value["thinking"].as_str().unwrap_or(""));
            value["thinking"] = json!(combined);
        }
    }
}

fn advance(event: &Map<String, Value>, acc: &mut Acc, on_text: &(dyn Fn(&str) + Send + Sync)) -> Result<Frame> {
    let status = status(event);
    if let Some(used) = context_usage(event) {
        acc.usage.input = used;
    }
    if let Some(entries) = checkpoint(event) {
        acc.apply(entries, on_text);
    }
    match status.as_deref() {
        Some("PLAN_APPROVAL_REQUIRED") | Some("TOOL_CALL_APPROVAL_REQUIRED") => Ok(Frame::Approval),
        Some("INPUT_REQUIRED") | Some("FINISHED") => Ok(Frame::Finished),
        Some("FAILED") | Some("STOPPED") => {
            let message = event.get("error").and_then(text)
                .or_else(|| event.get("message").and_then(text))
                .unwrap_or_else(|| status.clone().unwrap_or_default());
            Err(anyhow!("GitLab Duo Agent のワークフローが {} で終了しました: {message}", status.unwrap_or_default()))
        }
        _ => match action(event)? {
            Some(action) => Ok(Frame::Tool(tool_block(action))),
            None => Ok(Frame::Ignore),
        },
    }
}

fn status(event: &Map<String, Value>) -> Option<String> {
    let field = |key: &str, inner: &str| event.get(key).and_then(|value| text(&value[inner]));
    event.get("status").and_then(text)
        .or_else(|| field("workflowStatus", "status"))
        .or_else(|| field("newCheckpoint", "status"))
}

fn context_usage(event: &Map<String, Value>) -> Option<u64> {
    let action = event.get("action").and_then(Value::as_object);
    let checkpoint = event
        .get("newCheckpoint")
        .and_then(Value::as_object)
        .or_else(|| event.get("checkpoint").and_then(Value::as_object));
    for source in [Some(event), action, checkpoint].into_iter().flatten() {
        let Some(usage) = source.get("agent_context_usage").and_then(Value::as_object) else { continue };
        let agent = ["Chat Agent", "context_builder"]
            .iter()
            .find_map(|name| usage.get(*name))
            .or_else(|| usage.values().next());
        if let Some(tokens) = agent.and_then(|agent| agent.get("total_tokens")).and_then(Value::as_u64) {
            return Some(tokens)
        }
    }
    None
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Text,
    Thinking,
    Boundary,
}

struct Entry {
    key: String,
    content: String,
    kind: Kind,
}

/// GitLab checkpoints are full `ui_chat_log` snapshots; boundaries start a new turn.
fn checkpoint(event: &Map<String, Value>) -> Option<Vec<Entry>> {
    let action = event.get("action").and_then(Value::as_object);
    let checkpoint = action
        .and_then(|action| action.get("newCheckpoint"))
        .and_then(Value::as_object)
        .or_else(|| event.get("newCheckpoint").and_then(Value::as_object))
        .or_else(|| event.get("checkpoint").and_then(Value::as_object))?;
    let direct = ["message", "text", "content"]
        .iter()
        .find_map(|key| checkpoint.get(*key).and_then(text))
        .or_else(|| {
            let nested = checkpoint.get("checkpoint").and_then(Value::as_object)?;
            ["message", "text"].iter().find_map(|key| nested.get(*key).and_then(text))
        });
    if let Some(content) = direct {
        return Some(vec![Entry { key: "direct:text".into(), content, kind: Kind::Text }])
    }
    let log = checkpoint.get("checkpoint")?.as_str()?;
    let parsed: Value = serde_json::from_str(log).ok()?;
    let mut entries = Vec::new();
    for (index, row) in parsed.get("channel_values")?.get("ui_chat_log")?.as_array()?.iter().enumerate() {
        let Some(row) = row.as_object() else { continue };
        match row.get("message_type").and_then(text).as_deref() {
            Some("agent") => {
                let Some(content) = row.get("content").and_then(text) else { continue };
                // `on_agent_reasoning` tags pre-tool commentary; the rest is the answer.
                let thinking = row.get("message_sub_type").and_then(text).as_deref() == Some("reasoning");
                let key = row.get("message_id").and_then(text)
                    .map(|id| format!("agent:{id}"))
                    .unwrap_or_else(|| format!("{}:{index}", if thinking { "reasoning" } else { "agent" }));
                entries.push(Entry { key, content, kind: if thinking { Kind::Thinking } else { Kind::Text } });
            }
            Some("request") | Some("tool") => {
                entries.push(Entry { key: String::new(), content: String::new(), kind: Kind::Boundary })
            }
            _ => {}
        }
    }
    Some(entries)
}

struct Action {
    request_id: String,
    name: String,
    args: Map<String, Value>,
}

fn action(event: &Map<String, Value>) -> Result<Option<Action>> {
    let wrapped = ["action", "workflowAction", "toolCall"]
        .iter()
        .find_map(|key| event.get(*key).and_then(Value::as_object));
    if let Some(wrapped) = wrapped {
        if wrapped.contains_key("newCheckpoint") {
            return Ok(None)
        }
        let Some(name) = ["name", "action", "type"]
            .iter()
            .find_map(|key| wrapped.get(*key).and_then(text))
            .or_else(|| event.get("actionName").and_then(text))
        else {
            return Ok(None)
        };
        let request_id = ["requestID", "requestId", "id"]
            .iter()
            .find_map(|key| wrapped.get(*key).and_then(text))
            .or_else(|| event.get("requestID").and_then(text))
            .or_else(|| event.get("requestId").and_then(text));
        let request_id = require_request_id(request_id, &name, wrapped)?;
        let args = wrapped
            .get("args")
            .or_else(|| wrapped.get("arguments"))
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_else(|| wrapped.clone());
        return Ok(Some(Action { request_id, name, args }))
    }
    for name in ["runMCPTool", "run_mcp_tool"] {
        let Some(args) = event.get(name).and_then(Value::as_object) else { continue };
        let request_id = event.get("requestID").and_then(text).or_else(|| event.get("requestId").and_then(text));
        let request_id = require_request_id(request_id, name, event)?;
        return Ok(Some(Action { request_id, name: name.to_string(), args: args.clone() }))
    }
    Ok(None)
}

/// The server discards an `actionResponse` whose id misses its awaiting future, so a synthesized
/// id is never correct: a missing one is protocol drift.
fn require_request_id(id: Option<String>, name: &str, source: &Map<String, Value>) -> Result<String> {
    id.ok_or_else(|| {
        let keys: Vec<&str> = source.keys().take(20).map(String::as_str).collect();
        anyhow!("GitLab Duo Agent のアクション `{name}` に requestID がありません（キー: {}）", keys.join(", "))
    })
}

fn tool_block(action: Action) -> Block {
    let Action { request_id, name, args } = action;
    let (name, input) = if name == "runMCPTool" || name == "run_mcp_tool" {
        let raw = ["toolName", "tool_name", "name"]
            .iter()
            .find_map(|key| args.get(*key).and_then(text))
            .unwrap_or_default();
        // The registration name is bare; a prefixed wire name only confuses everyone.
        let bare = raw.strip_prefix("mcp__").and_then(|rest| rest.split_once("__")).map_or(raw.as_str(), |(_, name)| name);
        (bare.to_string(), parse_arguments(args.get("args").or_else(|| args.get("arguments"))))
    } else {
        (name, Value::Object(args))
    };
    Block::ToolUse { id: request_id, name, input }
}

fn parse_arguments(value: Option<&Value>) -> Value {
    match value {
        Some(Value::String(raw)) => serde_json::from_str::<Value>(raw).ok().filter(Value::is_object).unwrap_or_else(|| json!({})),
        Some(other) if other.is_object() => other.clone(),
        _ => json!({}),
    }
}

// ---- small helpers ----

fn api_url(base: &str, path: &str) -> String {
    format!("{}{}", base.trim_end_matches('/'), path)
}

fn normalize_service_base(base: &str) -> String {
    let trimmed = base.trim().trim_end_matches('/');
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    }
}

fn gid(namespace: &str) -> String {
    if namespace.chars().all(|c| c.is_ascii_digit()) && !namespace.is_empty() {
        format!("gid://gitlab/Group/{namespace}")
    } else {
        namespace.to_string()
    }
}

fn rest_id(namespace: &str) -> String {
    namespace.rsplit('/').next().unwrap_or(namespace).to_string()
}

fn encode(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => out.push(byte as char),
            _ => {
                let _ = write!(out, "%{byte:02X}");
            }
        }
    }
    out
}

fn text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) if !text.is_empty() => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().map(|value| value.trim().to_string()).filter(|value| !value.is_empty())
}

/// `group/project` from the nearest `.git/config` remote on this host.
///
/// shortcut: reads the plain `.git/config` only (no linked-worktree `commondir`); upgrade when a
/// worktree checkout must resolve its project without `GITLAB_DUO_PROJECT_PATH`.
fn remote_project(base: &str) -> Option<String> {
    let base = reqwest::Url::parse(base).ok()?;
    let host = base.host_str()?.to_ascii_lowercase();
    let prefix = base.path().trim_end_matches('/').to_string();
    let mut directory = std::env::current_dir().ok()?;
    loop {
        if let Ok(config) = std::fs::read_to_string(directory.join(".git").join("config")) {
            return config.lines().find_map(|line| {
                let (key, value) = line.split_once('=')?;
                key.trim().eq_ignore_ascii_case("url").then(|| gitlab_path(value.trim(), &host, &prefix)).flatten()
            })
        }
        if !directory.pop() {
            return None
        }
    }
}

fn gitlab_path(url: &str, host: &str, prefix: &str) -> Option<String> {
    let path = match url.split_once("://") {
        Some((scheme, rest)) => {
            let http = scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https");
            if !http && !scheme.eq_ignore_ascii_case("ssh") {
                return None
            }
            let (authority, path) = rest.split_once('/')?;
            if authority.rsplit('@').next()?.split(':').next()? != host {
                return None
            }
            path
        }
        // scp-like `git@host:group/project.git`.
        None => {
            let (authority, path) = url.split_once(':')?;
            if authority.rsplit('@').next()? != host {
                return None
            }
            path
        }
    };
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let path = path.strip_prefix(prefix.trim_matches('/')).unwrap_or(path).trim_matches('/');
    (path.split('/').count() >= 2).then(|| path.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    #[derive(Debug)]
    struct Recorded {
        method: String,
        path: String,
        headers: Vec<(String, String)>,
        body: String,
    }

    /// One HTTP request off the wire: request line and headers, then `content-length` bytes.
    async fn read_request(socket: &mut TcpStream) -> Option<Recorded> {
        let mut received = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = socket.read(&mut buf).await.ok()?;
            if n == 0 {
                return None
            }
            received.extend_from_slice(&buf[..n]);
            let Some(split) = received.windows(4).position(|w| w == b"\r\n\r\n") else { continue };
            let head = String::from_utf8_lossy(&received[..split]).to_string();
            let length = head
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .and_then(|value| value.trim().parse::<usize>().ok())
                })
                .unwrap_or(0);
            if received.len() < split + 4 + length {
                continue
            }
            let body = String::from_utf8_lossy(&received[split + 4..split + 4 + length]).to_string();
            let mut lines = head.lines();
            let mut request_line = lines.next().unwrap_or_default().split_whitespace();
            let method = request_line.next().unwrap_or_default().to_string();
            let path = request_line.next().unwrap_or_default().to_string();
            let headers = lines
                .filter_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    Some((name.trim().to_ascii_lowercase(), value.trim().to_string()))
                })
                .collect();
            return Some(Recorded { method, path, headers, body })
        }
    }

    async fn respond(socket: &mut TcpStream, status: &str, body: &Value) {
        let body = body.to_string();
        let head = format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n",
            body.len()
        );
        let _ = socket.write_all(format!("{head}\r\n{body}").as_bytes()).await;
        let _ = socket.shutdown().await;
    }

    /// True when this connection starts with the workflow socket's upgrade request.
    ///
    /// `shortcut`: peeks before the HTTP reader consumes the head; a client that splits the request
    /// line across TCP segments without more bytes still lands in one read on loopback.
    async fn is_socket_upgrade(socket: &mut TcpStream) -> bool {
        let mut buf = [0u8; 512];
        for _ in 0..50 {
            if socket.peek(&mut buf).await.is_ok_and(|n| n > 0) {
                return String::from_utf8_lossy(&buf).starts_with("GET /api/v4/ai/duo_workflows/ws")
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        false
    }

    fn reply_body(payload: &str) -> Value {
        json!({
            "newCheckpoint": {
                "status": "RUNNING",
                "checkpoint": json!({ "channel_values": { "ui_chat_log": serde_json::from_str::<Value>(payload).unwrap() } })
                    .to_string(),
                "agent_context_usage": { "Chat Agent": { "total_tokens": 123, "max_tokens": 1000 } },
            }
        })
    }

    /// One `complete`: REST setup, a workflow socket, a checkpoint with text and a tool action, then stop.
    #[tokio::test]
    async fn complete_turns_a_checkpoint_into_a_reply_and_stops_the_workflow() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let log = Arc::new(Mutex::new(Vec::<Recorded>::new()));
        let server = {
            let log = log.clone();
            tokio::spawn(async move {
                while let Ok((mut socket, _)) = listener.accept().await {
                    let log = log.clone();
                    tokio::spawn(async move {
                        if is_socket_upgrade(&mut socket).await {
                            let seen = log.clone();
                            let callback = move |request: &tokio_tungstenite::tungstenite::handshake::server::Request,
                                                 response: tokio_tungstenite::tungstenite::handshake::server::Response|
                                  -> std::result::Result<_, tokio_tungstenite::tungstenite::handshake::server::ErrorResponse> {
                                seen.lock().unwrap().push(Recorded {
                                    method: "GET".into(),
                                    path: request.uri().to_string(),
                                    headers: request
                                        .headers()
                                        .iter()
                                        .map(|(name, value)| {
                                            (name.as_str().to_string(), value.to_str().unwrap_or_default().to_string())
                                        })
                                        .collect(),
                                    body: String::new(),
                                });
                                Ok(response)
                            };
                            let mut ws = tokio_tungstenite::accept_hdr_async(socket, callback).await.unwrap();
                            let frame = ws.next().await.unwrap().unwrap();
                            log.lock().unwrap().push(Recorded {
                                method: "WS".into(),
                                path: String::new(),
                                headers: Vec::new(),
                                body: frame.to_text().unwrap().to_string(),
                            });
                            for event in [
                                reply_body(r#"[{"message_type":"agent","message_id":"m1","content":"Checking "}]"#),
                                reply_body(
                                    r#"[{"message_type":"agent","message_id":"m1","content":"Checking files"},
                                       {"message_type":"request","content":"read a.txt"},
                                       {"message_type":"tool","content":"file text"}]"#,
                                ),
                                json!({"requestID": "req-1", "runMCPTool": {"name": "mcp__greencode__read", "args": "{\"path\":\"a.txt\"}"}}),
                            ] {
                                let _ = ws.send(tokio_tungstenite::tungstenite::Message::text(event.to_string())).await;
                            }
                            return
                        }
                        let Some(recorded) = read_request(&mut socket).await else { return };
                        let reply = match (recorded.path.as_str(), recorded.method.as_str()) {
                            (path, "GET") if path.starts_with("/api/v4/groups/42/projects") => json!([{
                                "id": 7,
                                "path_with_namespace": "grp/proj",
                                "namespace": { "id": 42, "full_path": "grp" }
                            }]),
                            (path, "GET") if path.starts_with("/api/v4/groups?") => {
                                json!([{ "id": 42, "duo_features_enabled": true }])
                            }
                            (_, "PUT") => json!({ "id": 42 }),
                            ("/api/v4/ai/duo_workflows/direct_access", _) => {
                                json!({ "gitlab_rails": { "token": "wf-token" } })
                            }
                            ("/api/v4/ai/duo_workflows/workflows", _) => json!({ "id": 99 }),
                            ("/api/graphql", _) => json!({ "data": { "aiChatAvailableModels": {
                                "defaultModel": { "name": "Claude Sonnet 4.6", "ref": "claude_sonnet_4_6" },
                                "selectableModels": [],
                                "pinnedModel": null,
                            } } }),
                            _ => json!({}),
                        };
                        log.lock().unwrap().push(recorded);
                        respond(&mut socket, "200 OK", &reply).await;
                    });
                }
            })
        };
        let base = format!("http://{addr}");
        let workflow = Workflow::new(reqwest::Client::new(), base.clone(), "glpat-test".into(), Wire::default());
        let messages = vec![Message::user("hello")];
        let tools = vec![crate::ToolDef {
            name: "read".into(),
            description: "Read a file".into(),
            schema: json!({ "type": "object", "properties": { "path": { "type": "string" } } }),
        }];
        let request = Request {
            model: "claude_sonnet_4_6",
            system: "You are helpful.",
            messages: &messages,
            tools: &tools,
            max_tokens: 1024,
            live: None,
        };
        let streamed = Mutex::new(String::new());
        let reply = tokio::time::timeout(
            Duration::from_secs(20),
            workflow.complete(request, &|delta| streamed.lock().unwrap().push_str(delta)),
        )
        .await
        .expect("complete timed out")
        .unwrap();
        assert_eq!(streamed.lock().unwrap().as_str(), "Checking files");
        assert_eq!(reply.stop, "tool_use");
        assert_eq!(reply.usage.input, 123);
        assert!(matches!(&reply.blocks[0], Block::Text { text } if text == "Checking files"));
        assert!(
            matches!(&reply.blocks[1], Block::ToolUse { id, name, input }
                if id == "req-1" && name == "read" && input["path"] == "a.txt")
        );

        let log = log.lock().unwrap();
        let find = |method: &str, path: &str| log.iter().find(|entry| entry.method == method && entry.path == path);
        // Namespace and project discovery, then the settings PUT and the workflow's REST lifecycle.
        assert!(find("GET", "/api/v4/groups?top_level_only=true&per_page=100&order_by=name&sort=asc").is_some());
        let settings = log.iter().find(|entry| entry.method == "PUT").expect("group settings PUT");
        assert!(settings.body.contains("\"duo_agent_platform_enabled\":true"));
        let direct = find("POST", "/api/v4/ai/duo_workflows/direct_access").expect("direct_access");
        assert!(direct.body.contains("\"root_namespace_id\":\"gid://gitlab/Group/42\""));
        assert!(direct.body.contains("\"project_id\":\"grp/proj\""));
        let create = find("POST", "/api/v4/ai/duo_workflows/workflows").expect("workflow create");
        assert!(create.body.contains("\"workflow_definition\":\"ambient\""));
        // The socket: query routing and the headers the service checks.
        let socket = log
            .iter()
            .find(|entry| entry.method == "GET" && entry.path.contains("project_id="))
            .expect("workflow socket request");
        assert!(socket.path.contains("project_id=7"), "{}", socket.path);
        assert!(socket.path.contains("root_namespace_id=42"), "{}", socket.path);
        assert!(socket.path.contains("workflow_definition=ambient"));
        assert!(socket.path.contains("user_selected_model_identifier=claude_sonnet_4_6"));
        let header = |name: &str| {
            socket.headers.iter().find(|(key, _)| key == name).map(|(_, value)| value.as_str()).unwrap_or_default()
        };
        assert_eq!(header("authorization"), "Bearer wf-token");
        assert_eq!(header("x-gitlab-client-type"), "node-websocket");
        assert_eq!(header("x-gitlab-project-id"), "7");
        assert_eq!(header("x-gitlab-root-namespace-id"), "42");
        assert_eq!(header("origin"), base);
        // The start request: one turn rides as bare text with the sanitized flow prompt.
        let start: Value = serde_json::from_str(&log.iter().find(|entry| entry.method == "WS").unwrap().body).unwrap();
        let start = &start["startRequest"];
        assert_eq!(start["goal"], "hello");
        assert_eq!(start["workflowID"], "99");
        assert_eq!(start["workflowDefinition"], "ambient");
        assert_eq!(start["preapproved_tools"], json!(["read"]));
        assert_eq!(start["mcpTools"][0]["name"], "read");
        assert_eq!(start["flowConfig"]["prompts"][0]["prompt_template"]["system"], "You are helpful.");
        let metadata: Value = serde_json::from_str(start["workflowMetadata"].as_str().unwrap()).unwrap();
        assert_eq!(metadata["projectId"], "7");
        assert_eq!(metadata["selectedModelIdentifier"], "claude_sonnet_4_6");
        // The turn stops its fresh workflow once the reply is in hand.
        assert!(log.iter().any(|entry| entry.method == "PATCH"
            && entry.path == "/api/v4/ai/duo_workflows/workflows/99"
            && entry.body.contains("\"status_event\":\"stop\"")));
        server.abort();
    }

    #[test]
    fn goal_renders_a_tool_history_as_chatml() {
        let messages = vec![
            Message::user("hi"),
            Message {
                role: Role::Assistant,
                content: vec![
                    Block::Text { text: "looking".into() },
                    Block::ToolUse { id: "t1".into(), name: "read".into(), input: json!({ "path": "a.txt" }) },
                ],
            },
            Message {
                role: Role::User,
                content: vec![Block::ToolResult {
                    tool_use_id: "t1".into(),
                    content: "file text".into(),
                    is_error: false,
                }],
            },
        ];
        let goal = goal(&conversation(&messages), &messages);
        assert!(goal.contains("<|im_start|>user\nhi\n<|im_end|>"), "{goal}");
        assert!(goal.contains("<|im_start|>assistant\nlooking\n<ran read>{\"path\":\"a.txt\"}</ran>\n<|im_end|>"), "{goal}");
        assert!(goal.contains("<|im_start|>tool\n<ran:result>\nfile text\n<|im_end|>"), "{goal}");
    }
}
