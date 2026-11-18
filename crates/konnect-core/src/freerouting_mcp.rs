//! Local Freerouting MCP orchestration.
//!
//! Konnect deliberately does not reimplement the router. It starts the local
//! Freerouting JAR in its documented headless stdio MCP mode, follows the
//! server's routing state machine, and returns only compact job evidence.

use anyhow::{bail, Context, Result};
use serde::Serialize;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::task::JoinHandle;
use tokio::time::{timeout, Instant};

#[cfg(test)]
use std::sync::atomic::{AtomicU32, Ordering};

#[cfg(test)]
static LAST_CHILD_PID: AtomicU32 = AtomicU32::new(0);

const MCP_PROTOCOL_VERSION: &str = "2024-11-05";
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);
// Freerouting's whole log is kept: it is the only place the router says how
// many connections it left unrouted (see `RoutingSummary`). Under MCP its
// stderr also carries JSON-RPC traffic, which overflowed the old 64 KB cap
// before the final score lines were written.
const MAX_STDERR_BYTES: usize = 16 * 1024 * 1024;
const MAX_SES_BYTES: u64 = 512 * 1024 * 1024;

/// Launch arguments for one run, with the API and MCP HTTP servers on the
/// given loopback ports. Konnect talks to Freerouting over stdio; the HTTP
/// servers only have to exist (the MCP tools call the API), so each run gets
/// fresh ports: two jobs at once do not collide, and a process already
/// holding a well-known port never receives the board.
fn server_arguments(api_port: u16, mcp_port: u16) -> Vec<String> {
    vec![
        "--api_server.enabled=true".to_string(),
        "--api_server.authentication.enabled=false".to_string(),
        format!("--api_server-endpoints=http://127.0.0.1:{api_port}"),
        "--mcp_server.enabled=true".to_string(),
        "--mcp_server.authentication.enabled=false".to_string(),
        format!("--mcp_server-endpoints=http://127.0.0.1:{mcp_port}"),
        "--mcp_server.stdio=true".to_string(),
        // Freerouting 2.3.0 (build 2026-08-07) refuses a plain-HTTP endpoint
        // unless allowed ("Can't use the endpoint … because HTTP is not
        // allowed"); its MCP tools then fail with "Internal error" because
        // they call that API. Both endpoints are loopback-only (see test).
        "--api_server.http_allowed=true".to_string(),
        "--mcp_server.http_allowed=true".to_string(),
        // It also reports usage to api.freerouting.app by default. This tool
        // promises never to send anything to a cloud service.
        "--usage_and_diagnostic_data.disable_analytics=true".to_string(),
        "--gui.enabled=false".to_string(),
    ]
}

/// A loopback port free right now (the OS picks it).
fn free_loopback_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).context("find a free loopback port")?;
    Ok(listener.local_addr()?.port())
}

const REQUIRED_TOOLS: &[&str] = &[
    "create_session",
    "enqueue_job",
    "upload_job_input_from_local_file",
    "start_job",
    "get_job_details",
    "download_job_output_to_local_file",
];

#[derive(Debug, Clone)]
pub(crate) struct RouteSettings {
    pub max_passes: Option<u32>,
    pub optimizer_enabled: Option<bool>,
    pub job_timeout_seconds: Option<u64>,
    pub poll_interval: Duration,
    pub overall_timeout: Duration,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct RouteEvidence {
    pub session_id: String,
    pub job_id: String,
    pub final_state: String,
    pub poll_count: u32,
    pub elapsed_seconds: f64,
    pub ses_bytes: u64,
    pub server_protocol_version: String,
    pub diagnostics_path: Option<String>,
    /// The router's own account of the result, parsed from its log. `None`
    /// when the log holds no score line (an older or changed Freerouting).
    pub routing: Option<RoutingSummary>,
}

/// One score as Freerouting 2.x logs it: "660.59 (12 unrouted and 52 violations)".
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub(crate) struct RouterScore {
    pub score: f64,
    pub unrouted: u32,
    pub violations: u32,
}

/// What Freerouting reported about a job. The job state says only that the
/// job ended ("COMPLETED" even with nets left open); these numbers are the
/// router's own count of what it could not do.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct RoutingSummary {
    /// "Auto-routing stage started on board … for N unrouted items."
    pub initial_unrouted: Option<u32>,
    /// One score per auto-routing pass, in order.
    pub autoroute_passes: Vec<RouterScore>,
    /// The last final score the log states (optimizer stage if it ran,
    /// otherwise the auto-routing stage, otherwise the last pass).
    pub final_score: Option<RouterScore>,
}

impl RoutingSummary {
    /// Complete only when the router itself reports nothing unrouted and no
    /// violations. `None` when the log said nothing either way.
    pub fn complete(&self) -> Option<bool> {
        self.final_score.map(|s| s.unrouted == 0 && s.violations == 0)
    }
}

/// Parse "S (U unrouted and V violations)" at the start of `text`.
fn parse_score(text: &str) -> Option<RouterScore> {
    let (score, rest) = text.trim_start().split_once(" (")?;
    let (unrouted, rest) = rest.split_once(" unrouted and ")?;
    let (violations, _) = rest.split_once(" violations)")?;
    Some(RouterScore {
        score: score.trim().parse().ok()?,
        unrouted: unrouted.trim().parse().ok()?,
        violations: violations.trim().parse().ok()?,
    })
}

/// Read Freerouting 2.x's log (format confirmed against 2.3.0's
/// `BatchAutorouter`, `BatchOptimizer` and `RoutingJobSchedulerActionThread`
/// messages; fixture `tests/fixtures/freerouting_2_3_0_run.log`). Lines that
/// are not log messages (MCP JSON on the same stream) are ignored.
pub(crate) fn parse_routing_log(log: &str) -> Option<RoutingSummary> {
    let mut summary = RoutingSummary { initial_unrouted: None, autoroute_passes: Vec::new(), final_score: None };
    let mut stage_final = None;
    let mut optimizer_final = None;
    for line in log.lines() {
        if let Some(i) = line.find("Auto-routing stage started on board") {
            if let Some(n) = line[i..].split(" for ").nth(1).and_then(|t| t.split_whitespace().next()) {
                summary.initial_unrouted = n.parse().ok().or(summary.initial_unrouted);
            }
        } else if line.contains("Auto-routing pass #") {
            if let Some(score) = line.split("with the score of ").nth(1).and_then(parse_score) {
                summary.autoroute_passes.push(score);
            }
        } else if line.contains("Auto-routing stage completed") {
            if summary.initial_unrouted.is_none() {
                summary.initial_unrouted = line
                    .split("started with ")
                    .nth(1)
                    .and_then(|t| t.split_whitespace().next())
                    .and_then(|n| n.parse().ok());
            }
            stage_final = line.split("final score: ").nth(1).and_then(parse_score).or(stage_final);
        } else if line.contains("Optimization stage completed") {
            optimizer_final = line.split("final score: ").nth(1).and_then(parse_score).or(optimizer_final);
        }
    }
    summary.final_score = optimizer_final.or(stage_final).or(summary.autoroute_passes.last().copied());
    (summary.initial_unrouted.is_some() || summary.final_score.is_some()).then_some(summary)
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct BridgeProbe {
    pub native_mcp_available: bool,
    pub bridge_available: bool,
    pub server_protocol_version: Option<String>,
    pub tool_count: usize,
    pub diagnostics: Option<String>,
    pub error: Option<String>,
}

pub(crate) async fn probe_local(jar: &Path) -> BridgeProbe {
    let mut client = match LocalMcpClient::start(jar).await {
        Ok(client) => client,
        Err(error) => {
            return BridgeProbe {
                native_mcp_available: false,
                bridge_available: false,
                server_protocol_version: None,
                tool_count: 0,
                diagnostics: None,
                error: Some(format!("{error:#}")),
            };
        }
    };
    let server_protocol_version = Some(client.server_protocol_version.clone());
    let contract = async {
        let tools = client.list_tools().await?;
        validate_tool_contracts(&tools)?;
        let missing = REQUIRED_TOOLS
            .iter()
            .filter(|name| !tools.contains_key(**name))
            .copied()
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            bail!(
                "Freerouting MCP is missing required tool(s): {}",
                missing.join(", ")
            );
        }
        Ok::<usize, anyhow::Error>(tools.len())
    }
    .await;
    let diagnostics = client.close().await;
    match contract {
        Ok(tool_count) => BridgeProbe {
            native_mcp_available: true,
            bridge_available: true,
            server_protocol_version,
            tool_count,
            diagnostics: (!diagnostics.is_empty()).then_some(diagnostics),
            error: None,
        },
        Err(error) => BridgeProbe {
            native_mcp_available: true,
            bridge_available: false,
            server_protocol_version,
            tool_count: 0,
            diagnostics: (!diagnostics.is_empty()).then_some(diagnostics),
            error: Some(format!("{error:#}")),
        },
    }
}

pub(crate) async fn route_local(
    jar: &Path,
    dsn: &Path,
    ses_output: &Path,
    settings: &RouteSettings,
) -> Result<RouteEvidence> {
    validate_inputs(jar, dsn, ses_output, settings)?;
    if let Some(parent) = ses_output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("create SES output directory {}", parent.display()))?;
    }
    let temporary = temporary_output_path(ses_output)?;
    if temporary.exists() {
        bail!(
            "temporary SES output already exists: {}",
            temporary.display()
        );
    }

    let mut client = LocalMcpClient::start(jar).await?;
    let started = Instant::now();
    let route_result = match timeout(
        settings.overall_timeout,
        run_state_machine(&mut client, dsn, &temporary, settings, started),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => Err(anyhow::anyhow!(
            "Freerouting MCP job exceeded the overall timeout of {} seconds",
            settings.overall_timeout.as_secs()
        )),
    };
    let diagnostics = client.close().await;
    let diagnostics_path = write_diagnostics(ses_output, &diagnostics).await?;

    let result = match route_result {
        Ok(evidence) => Ok(evidence),
        Err(error) => {
            if diagnostics.is_empty() {
                Err(error)
            } else {
                Err(error).context(format!("Freerouting stderr: {diagnostics}"))
            }
        }
    };
    let evidence = match result {
        Ok(evidence) => evidence,
        Err(error) => {
            let _ = tokio::fs::remove_file(&temporary).await;
            return Err(error);
        }
    };

    let metadata = tokio::fs::metadata(&temporary)
        .await
        .with_context(|| format!("Freerouting did not create {}", temporary.display()))?;
    if metadata.len() == 0 || metadata.len() > MAX_SES_BYTES {
        let _ = tokio::fs::remove_file(&temporary).await;
        bail!(
            "Freerouting SES size {} is outside the supported range 1..={MAX_SES_BYTES}",
            metadata.len()
        );
    }
    let ses_source = tokio::fs::read_to_string(&temporary)
        .await
        .with_context(|| format!("read Freerouting SES {}", temporary.display()))?;
    let tree = konnect_sexp::parse_sexp(&ses_source).context("parse Freerouting SES output")?;
    if tree.head() != Some("session") {
        let _ = tokio::fs::remove_file(&temporary).await;
        bail!("Freerouting output is not a Specctra SES session");
    }
    konnect_sexp::write_new_atomic(ses_output, &ses_source)
        .with_context(|| format!("create SES output {}", ses_output.display()))?;
    tokio::fs::remove_file(&temporary)
        .await
        .with_context(|| format!("remove temporary SES {}", temporary.display()))?;
    Ok(RouteEvidence {
        ses_bytes: metadata.len(),
        diagnostics_path,
        routing: parse_routing_log(&diagnostics),
        ..evidence
    })
}

async fn write_diagnostics(output: &Path, diagnostics: &str) -> Result<Option<String>> {
    if diagnostics.is_empty() {
        return Ok(None);
    }
    let path = output.with_extension("freerouting.log");
    if path.exists() {
        bail!(
            "Freerouting diagnostics output already exists: {}",
            path.display()
        );
    }
    konnect_sexp::write_new_atomic(&path, diagnostics)
        .with_context(|| format!("create Freerouting diagnostics {}", path.display()))?;
    Ok(Some(path.display().to_string()))
}

fn validate_inputs(
    jar: &Path,
    dsn: &Path,
    ses_output: &Path,
    settings: &RouteSettings,
) -> Result<()> {
    if !jar.is_file() {
        bail!("Freerouting JAR does not exist: {}", jar.display());
    }
    if !dsn.is_file() || !extension_is(dsn, "dsn") {
        bail!("DSN input must be an existing .dsn file: {}", dsn.display());
    }
    if !extension_is(ses_output, "ses") {
        bail!("SES output must have the .ses extension");
    }
    if ses_output.exists() {
        bail!("SES output already exists: {}", ses_output.display());
    }
    if let Some(max_passes) = settings.max_passes {
        if !(1..=100).contains(&max_passes) {
            bail!("max_passes must be between 1 and 100");
        }
    }
    if settings.poll_interval < Duration::from_secs(2)
        || settings.poll_interval > Duration::from_secs(5)
    {
        bail!("poll interval must be between 2 and 5 seconds");
    }
    let minimum_overall_timeout = if cfg!(test) {
        Duration::from_millis(1)
    } else {
        Duration::from_secs(10)
    };
    if settings.overall_timeout < minimum_overall_timeout
        || settings.overall_timeout > Duration::from_secs(86_400)
    {
        bail!("overall timeout must be between 10 and 86400 seconds");
    }
    if settings
        .job_timeout_seconds
        .is_some_and(|seconds| seconds == 0 || seconds > 86_400)
    {
        bail!("job_timeout_seconds must be between 1 and 86400");
    }
    Ok(())
}

async fn run_state_machine(
    client: &mut LocalMcpClient,
    dsn: &Path,
    temporary: &Path,
    settings: &RouteSettings,
    started: Instant,
) -> Result<RouteEvidence> {
    let tools = client.list_tools().await?;
    validate_tool_contracts(&tools)?;
    let missing = REQUIRED_TOOLS
        .iter()
        .filter(|name| !tools.contains_key(**name))
        .copied()
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        bail!(
            "Freerouting MCP is missing required tool(s): {}",
            missing.join(", ")
        );
    }

    let session = client.call_tool("create_session", json!({})).await?;
    let session_id = required_string(&session, &["sessionId", "session_id", "id"], "session id")?;
    let job = client
        .call_tool(
            "enqueue_job",
            json!({ "body": { "session_id": session_id } }),
        )
        .await?;
    let job_id = required_string(&job, &["jobId", "job_id", "id"], "job id")?;
    client
        .call_tool(
            "upload_job_input_from_local_file",
            json!({ "jobId": job_id, "filePath": absolute_utf8(dsn)? }),
        )
        .await?;

    let mut job_settings = Map::new();
    if let Some(max_passes) = settings.max_passes {
        job_settings.insert("maxPasses".to_string(), json!(max_passes));
    }
    if let Some(enabled) = settings.optimizer_enabled {
        job_settings.insert("optimizer".to_string(), json!({ "enabled": enabled }));
    }
    if let Some(seconds) = settings.job_timeout_seconds {
        job_settings.insert(
            "jobTimeoutString".to_string(),
            Value::String(duration_string(seconds)),
        );
    }
    if !job_settings.is_empty() {
        if !tools.contains_key("update_job_settings") {
            bail!("Freerouting MCP does not expose update_job_settings");
        }
        client
            .call_tool(
                "update_job_settings",
                json!({ "path": { "jobId": job_id }, "body": job_settings }),
            )
            .await?;
    }
    client
        .call_tool("start_job", json!({ "path": { "jobId": job_id } }))
        .await?;

    let mut poll_count = 0u32;
    let final_state = loop {
        let details = client
            .call_tool("get_job_details", json!({ "path": { "jobId": job_id } }))
            .await?;
        poll_count = poll_count.saturating_add(1);
        let state = required_string(&details, &["state", "status"], "job state")?;
        match state.to_ascii_uppercase().as_str() {
            "COMPLETED" => break "COMPLETED".to_string(),
            "FAILED" | "CANCELLED" | "CANCELED" => {
                bail!("Freerouting job {job_id} ended in state {state}")
            }
            _ => tokio::time::sleep(settings.poll_interval).await,
        }
    };

    client
        .call_tool(
            "download_job_output_to_local_file",
            json!({ "jobId": job_id, "filePath": absolute_utf8(temporary)? }),
        )
        .await?;
    Ok(RouteEvidence {
        session_id,
        job_id,
        final_state,
        poll_count,
        elapsed_seconds: started.elapsed().as_secs_f64(),
        ses_bytes: 0,
        server_protocol_version: client.server_protocol_version.clone(),
        diagnostics_path: None,
        routing: None,
    })
}

fn validate_tool_contracts(tools: &BTreeMap<String, Value>) -> Result<()> {
    for (tool, paths) in [
        ("enqueue_job", &[(&["body", "session_id"][..], false)][..]),
        (
            "upload_job_input_from_local_file",
            &[(&["jobId"][..], true), (&["filePath"][..], true)][..],
        ),
        ("start_job", &[(&["path", "jobId"][..], true)][..]),
        ("get_job_details", &[(&["path", "jobId"][..], true)][..]),
        (
            "download_job_output_to_local_file",
            &[(&["jobId"][..], true), (&["filePath"][..], true)][..],
        ),
    ] {
        let schema = tools
            .get(tool)
            .with_context(|| format!("Freerouting MCP is missing required tool '{tool}'"))?;
        for (path, leaf_must_be_required) in paths {
            require_schema_path(tool, schema, path, *leaf_must_be_required)?;
        }
    }
    Ok(())
}

fn require_schema_path(
    tool: &str,
    schema: &Value,
    path: &[&str],
    leaf_must_be_required: bool,
) -> Result<()> {
    let mut current = schema;
    for (index, key) in path.iter().enumerate() {
        if current.get("type").and_then(Value::as_str) != Some("object") {
            bail!(
                "Freerouting MCP tool '{tool}' has an incompatible input schema at '{}'",
                path.join(".")
            );
        }
        let required = current
            .get("required")
            .and_then(Value::as_array)
            .is_some_and(|values| values.iter().any(|value| value.as_str() == Some(key)));
        if !required && (index + 1 < path.len() || leaf_must_be_required) {
            bail!(
                "Freerouting MCP tool '{tool}' does not require argument '{}'",
                path.join(".")
            );
        }
        current = current
            .get("properties")
            .and_then(|properties| properties.get(*key))
            .with_context(|| {
                format!(
                    "Freerouting MCP tool '{tool}' has no argument '{}'",
                    path.join(".")
                )
            })?;
    }
    Ok(())
}

struct LocalMcpClient {
    child: Option<Child>,
    stdin: ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
    stderr_task: Option<JoinHandle<String>>,
    next_id: u64,
    server_protocol_version: String,
}

impl LocalMcpClient {
    async fn start(jar: &Path) -> Result<Self> {
        let mut client = Self::spawn(jar)?;
        if let Err(error) = client.initialize().await {
            let diagnostics = client.close().await;
            return if diagnostics.is_empty() {
                Err(error)
            } else {
                Err(error).context(format!("Freerouting stderr: {diagnostics}"))
            };
        }
        Ok(client)
    }

    fn spawn(jar: &Path) -> Result<Self> {
        let mut command = Command::new("java");
        command
            .arg("-jar")
            .arg(jar)
            .args(server_arguments(free_loopback_port()?, free_loopback_port()?))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .context("start local Freerouting MCP server")?;
        #[cfg(test)]
        if let Some(pid) = child.id() {
            LAST_CHILD_PID.store(pid, Ordering::SeqCst);
        }
        let stdin = child.stdin.take().context("Freerouting MCP has no stdin")?;
        let stdout = child
            .stdout
            .take()
            .context("Freerouting MCP has no stdout")?;
        let stderr = child
            .stderr
            .take()
            .context("Freerouting MCP has no stderr")?;
        let stderr_task = tokio::spawn(async move {
            let reader = BufReader::new(stderr);
            let mut bytes = Vec::new();
            let _ = reader
                .take(MAX_STDERR_BYTES as u64)
                .read_to_end(&mut bytes)
                .await;
            String::from_utf8_lossy(&bytes).trim().to_string()
        });
        Ok(Self {
            child: Some(child),
            stdin,
            stdout: BufReader::new(stdout).lines(),
            stderr_task: Some(stderr_task),
            next_id: 1,
            server_protocol_version: String::new(),
        })
    }

    async fn initialize(&mut self) -> Result<()> {
        let initialize = timeout(
            STARTUP_TIMEOUT,
            self.request(
                "initialize",
                json!({
                    "protocolVersion": MCP_PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": { "name": "Konnect", "version": env!("CARGO_PKG_VERSION") }
                }),
                STARTUP_TIMEOUT,
            ),
        )
        .await
        .map_err(|_| anyhow::anyhow!("Freerouting MCP initialization timed out"))??;
        self.server_protocol_version = find_string(&initialize, &["protocolVersion"])
            .unwrap_or_else(|| MCP_PROTOCOL_VERSION.to_string());
        self.notify("notifications/initialized", json!({})).await?;
        Ok(())
    }

    async fn list_tools(&mut self) -> Result<BTreeMap<String, Value>> {
        let result = self
            .request("tools/list", json!({}), REQUEST_TIMEOUT)
            .await?;
        let tools = result
            .get("tools")
            .and_then(Value::as_array)
            .context("Freerouting tools/list returned no tools array")?;
        tools
            .iter()
            .map(|tool| {
                let name = tool
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .context("Freerouting tools/list contains an unnamed tool")?;
                let schema = tool
                    .get("inputSchema")
                    .cloned()
                    .context("Freerouting tools/list contains a tool without inputSchema")?;
                Ok((name, schema))
            })
            .collect()
    }

    async fn call_tool(&mut self, name: &str, arguments: Value) -> Result<Value> {
        let result = self
            .request(
                "tools/call",
                json!({ "name": name, "arguments": arguments }),
                REQUEST_TIMEOUT,
            )
            .await
            .with_context(|| format!("call Freerouting MCP tool '{name}'"))?;
        if result.get("isError").and_then(Value::as_bool) == Some(true) {
            bail!(
                "Freerouting MCP tool '{name}' returned an error: {}",
                compact(&result)
            );
        }
        Ok(expand_text_content(result))
    }

    async fn request(&mut self, method: &str, params: Value, limit: Duration) -> Result<Value> {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        self.write_json(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        }))
        .await?;
        let deadline = Instant::now() + limit;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                bail!("Freerouting MCP request '{method}' timed out");
            }
            let line = timeout(remaining, self.stdout.next_line())
                .await
                .map_err(|_| anyhow::anyhow!("Freerouting MCP request '{method}' timed out"))??
                .context("Freerouting MCP closed stdout before responding")?;
            if line.trim().is_empty() {
                continue;
            }
            let response: Value = serde_json::from_str(&line)
                .with_context(|| format!("Freerouting MCP emitted non-JSON stdout: {line}"))?;
            if response.get("id") != Some(&json!(id)) {
                if response.get("method").is_some() {
                    continue;
                }
                bail!("Freerouting MCP returned an unexpected response id");
            }
            if let Some(error) = response.get("error") {
                bail!(
                    "Freerouting MCP request '{method}' failed: {}",
                    compact(error)
                );
            }
            return response
                .get("result")
                .cloned()
                .context("Freerouting MCP response has no result");
        }
    }

    async fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.write_json(&json!({ "jsonrpc": "2.0", "method": method, "params": params }))
            .await
    }

    async fn write_json(&mut self, value: &Value) -> Result<()> {
        let mut bytes = serde_json::to_vec(value)?;
        bytes.push(b'\n');
        self.stdin
            .write_all(&bytes)
            .await
            .context("write Freerouting MCP stdin")?;
        self.stdin
            .flush()
            .await
            .context("flush Freerouting MCP stdin")
    }

    async fn close(mut self) -> String {
        let _ = self.stdin.shutdown().await;
        if let Some(mut child) = self.child.take() {
            if timeout(SHUTDOWN_TIMEOUT, child.wait()).await.is_err() {
                let _ = child.kill().await;
                let _ = child.wait().await;
            }
        }
        match self.stderr_task.take() {
            Some(task) => task.await.unwrap_or_default(),
            None => String::new(),
        }
    }
}

impl Drop for LocalMcpClient {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.start_kill();
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {
                    let _ = child.wait().await;
                });
            }
        }
        if let Some(task) = self.stderr_task.take() {
            task.abort();
        }
    }
}

fn expand_text_content(mut result: Value) -> Value {
    let parsed = result
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| item.get("text").and_then(Value::as_str))
        .filter_map(|text| serde_json::from_str::<Value>(text).ok())
        .collect::<Vec<_>>();
    if let Some(object) = result.as_object_mut() {
        object.insert("parsedTextContent".to_string(), Value::Array(parsed));
    }
    result
}

fn required_string(value: &Value, keys: &[&str], label: &str) -> Result<String> {
    find_string(value, keys).with_context(|| {
        format!(
            "Freerouting MCP response has no {label}: {}",
            compact(value)
        )
    })
}

fn find_string(value: &Value, keys: &[&str]) -> Option<String> {
    match value {
        Value::Object(object) => {
            for (key, value) in object {
                if keys
                    .iter()
                    .any(|candidate| key.eq_ignore_ascii_case(candidate))
                {
                    if let Some(value) = value.as_str() {
                        return Some(value.to_string());
                    }
                }
            }
            object.values().find_map(|value| find_string(value, keys))
        }
        Value::Array(values) => values.iter().find_map(|value| find_string(value, keys)),
        _ => None,
    }
}

fn absolute_utf8(path: &Path) -> Result<String> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    absolute
        .to_str()
        .map(str::to_string)
        .with_context(|| format!("path is not UTF-8: {}", absolute.display()))
}

fn temporary_output_path(output: &Path) -> Result<PathBuf> {
    let parent = output.parent().unwrap_or_else(|| Path::new("."));
    let name = output
        .file_name()
        .and_then(|name| name.to_str())
        .context("SES output has no UTF-8 filename")?;
    Ok(parent.join(format!(".{name}.konnect-{}.tmp.ses", uuid::Uuid::new_v4())))
}

fn duration_string(seconds: u64) -> String {
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let seconds = seconds % 60;
    format!("{hours:02}:{minutes:02}:{seconds:02}")
}

fn extension_is(path: &Path, expected: &str) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case(expected))
}

fn compact(value: &Value) -> String {
    let text = value.to_string();
    if text.chars().count() > 2_000 {
        let truncated: String = text.chars().take(2_000).collect();
        format!("{truncated}...")
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use konnect_ipc::{IpcEffectiveRoutingRules, IpcRoutingRules};

    #[cfg(windows)]
    fn process_is_running(pid: u32) -> bool {
        let output = std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
            .output()
            .expect("run tasklist");
        String::from_utf8_lossy(&output.stdout).contains(&format!("\"{pid}\""))
    }

    #[cfg(not(windows))]
    fn process_is_running(pid: u32) -> bool {
        std::process::Command::new("sh")
            .args(["-c", &format!("kill -0 {pid} 2>/dev/null")])
            .status()
            .expect("probe process")
            .success()
    }

    async fn wait_for_process_exit(pid: u32) -> bool {
        for _ in 0..50 {
            if !process_is_running(pid) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        false
    }

    fn routing_rules() -> IpcEffectiveRoutingRules {
        ["GND", "VCC"]
            .into_iter()
            .map(|net| {
                (
                    net.to_string(),
                    IpcRoutingRules {
                        class_name: "Default".to_string(),
                        constituents: vec!["Default".to_string()],
                        track_width_mm: Some(0.25),
                        clearance_mm: Some(0.2),
                        via_diameter_mm: Some(0.6),
                        via_drill_mm: Some(0.3),
                    },
                )
            })
            .collect()
    }

    #[test]
    fn nested_and_text_wrapped_identifiers_are_found() {
        let value = expand_text_content(json!({
            "content": [{ "type": "text", "text": "{\"data\":{\"sessionId\":\"abc\"}}" }]
        }));
        assert_eq!(find_string(&value, &["sessionId"]), Some("abc".to_string()));
    }

    #[test]
    fn duration_settings_use_documented_shape() {
        assert_eq!(duration_string(5 * 60), "00:05:00");
        assert_eq!(duration_string(25 * 60 * 60), "25:00:00");
    }

    #[test]
    fn output_path_is_unique_and_stays_beside_destination() {
        let output = Path::new("build/board.ses");
        let first = temporary_output_path(output).unwrap();
        let second = temporary_output_path(output).unwrap();
        assert_eq!(first.parent(), output.parent());
        assert_ne!(first, second);
        assert!(extension_is(&first, "ses"));
    }

    #[test]
    fn each_run_gets_its_own_free_ports() {
        let (a, b) = (free_loopback_port().unwrap(), free_loopback_port().unwrap());
        assert!(a > 0 && b > 0);
    }

    #[test]
    fn unauthenticated_servers_are_forced_to_loopback() {
        let arguments = server_arguments(41001, 41002);
        let has = |flag: &str| arguments.iter().any(|argument| argument == flag);
        assert!(has("--api_server.authentication.enabled=false"));
        assert!(has("--api_server-endpoints=http://127.0.0.1:41001"));
        assert!(has("--mcp_server.authentication.enabled=false"));
        assert!(has("--mcp_server-endpoints=http://127.0.0.1:41002"));
        assert!(has("--usage_and_diagnostic_data.disable_analytics=true"));
        assert!(!arguments
            .iter()
            .any(|argument| argument.contains("0.0.0.0")));
    }

    #[test]
    fn required_tool_schema_drift_is_refused_before_routing() {
        let object = |properties: Value, required: Value| json!({ "type": "object", "properties": properties, "required": required });
        let mut tools = BTreeMap::new();
        tools.insert("enqueue_job".into(), object(json!({"body": object(json!({"session_id": {"type":"string"}}), json!(["session_id"]))}), json!(["body"])));
        tools.insert(
            "upload_job_input_from_local_file".into(),
            object(
                json!({"jobId":{"type":"string"},"filePath":{"type":"string"}}),
                json!(["jobId", "filePath"]),
            ),
        );
        tools.insert(
            "start_job".into(),
            object(
                json!({"path": object(json!({"jobId":{"type":"string"}}), json!(["jobId"]))}),
                json!(["path"]),
            ),
        );
        tools.insert("get_job_details".into(), tools["start_job"].clone());
        tools.insert(
            "download_job_output_to_local_file".into(),
            tools["upload_job_input_from_local_file"].clone(),
        );
        validate_tool_contracts(&tools).unwrap();

        tools.get_mut("start_job").unwrap()["properties"]["path"]["required"] = json!([]);
        let error = validate_tool_contracts(&tools).unwrap_err().to_string();
        assert!(
            error.contains("start_job") && error.contains("path.jobId"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn stderr_diagnostics_are_preserved_beside_the_requested_output() {
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("board.ses");
        let path = write_diagnostics(&output, "router failed at pass 3")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            "router failed at pass 3"
        );
    }

    /// Optional local parity check for Freerouting's actual stdio MCP state
    /// machine. CI does not install Java or Freerouting.
    #[tokio::test]
    #[ignore = "requires Java and FREEROUTING_JAR"]
    async fn native_mcp_routes_the_export_fixture() {
        let jar = PathBuf::from(std::env::var_os("FREEROUTING_JAR").expect("set FREEROUTING_JAR"));
        let source = include_str!("../tests/fixtures/specctra_two_resistors.kicad_pcb");
        let temp = tempfile::tempdir().unwrap();
        let board = temp.path().join("board.kicad_pcb");
        let dsn = temp.path().join("board.dsn");
        let ses = temp.path().join("board.ses");
        std::fs::write(&board, source).unwrap();
        let export = crate::specctra::export_dsn(&board, source, &routing_rules()).unwrap();
        std::fs::write(&dsn, export.dsn).unwrap();

        let evidence = route_local(
            &jar,
            &dsn,
            &ses,
            &RouteSettings {
                max_passes: Some(2),
                optimizer_enabled: Some(false),
                job_timeout_seconds: Some(300),
                poll_interval: Duration::from_secs(2),
                overall_timeout: Duration::from_secs(300),
            },
        )
        .await
        .unwrap();
        assert_eq!(evidence.final_state, "COMPLETED");
        assert!(evidence.ses_bytes > 0);
        assert!(ses.is_file());
    }

    #[tokio::test]
    #[ignore = "requires Java and FREEROUTING_JAR"]
    async fn overall_timeout_closes_child_and_removes_partial_output() {
        let jar = PathBuf::from(std::env::var_os("FREEROUTING_JAR").expect("set FREEROUTING_JAR"));
        let source = include_str!("../tests/fixtures/specctra_two_resistors.kicad_pcb");
        let temp = tempfile::tempdir().unwrap();
        let board = temp.path().join("board.kicad_pcb");
        let dsn = temp.path().join("board.dsn");
        let ses = temp.path().join("board.ses");
        std::fs::write(&board, source).unwrap();
        let export = crate::specctra::export_dsn(&board, source, &routing_rules()).unwrap();
        std::fs::write(&dsn, export.dsn).unwrap();

        let error = format!(
            "{:#}",
            route_local(
                &jar,
                &dsn,
                &ses,
                &RouteSettings {
                    max_passes: Some(100),
                    optimizer_enabled: Some(true),
                    job_timeout_seconds: Some(300),
                    poll_interval: Duration::from_secs(5),
                    overall_timeout: Duration::from_millis(100),
                },
            )
            .await
            .unwrap_err()
        );
        assert!(error.contains("overall timeout"), "{error}");
        let pid = LAST_CHILD_PID.load(Ordering::SeqCst);
        assert!(
            wait_for_process_exit(pid).await,
            "Freerouting child {pid} survived timeout"
        );
        assert!(!ses.exists());
        assert!(
            std::fs::read_dir(temp.path()).unwrap().all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".tmp.ses")),
            "timeout left a temporary SES"
        );
    }

    #[tokio::test]
    #[ignore = "requires Java and FREEROUTING_JAR"]
    async fn cancelling_route_reaps_the_owned_child() {
        let jar = PathBuf::from(std::env::var_os("FREEROUTING_JAR").expect("set FREEROUTING_JAR"));
        let source = include_str!("../tests/fixtures/specctra_two_resistors.kicad_pcb");
        let temp = tempfile::tempdir().unwrap();
        let board = temp.path().join("board.kicad_pcb");
        let dsn = temp.path().join("board.dsn");
        let ses = temp.path().join("board.ses");
        std::fs::write(&board, source).unwrap();
        let export = crate::specctra::export_dsn(&board, source, &routing_rules()).unwrap();
        std::fs::write(&dsn, export.dsn).unwrap();

        LAST_CHILD_PID.store(0, Ordering::SeqCst);
        let task = tokio::spawn(async move {
            route_local(
                &jar,
                &dsn,
                &ses,
                &RouteSettings {
                    max_passes: Some(100),
                    optimizer_enabled: Some(true),
                    job_timeout_seconds: Some(300),
                    poll_interval: Duration::from_secs(5),
                    overall_timeout: Duration::from_secs(300),
                },
            )
            .await
        });
        let pid = loop {
            let pid = LAST_CHILD_PID.load(Ordering::SeqCst);
            if pid != 0 {
                break pid;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        task.abort();
        let _ = task.await;
        assert!(
            wait_for_process_exit(pid).await,
            "Freerouting child {pid} survived cancellation"
        );
    }
}

#[cfg(test)]
mod routing_log_tests {
    use super::*;

    const RUN: &str = include_str!("../tests/fixtures/freerouting_2_3_0_run.log");

    /// A real Freerouting 2.3.0 run on the module board's DSN (5 passes):
    /// the job ends "COMPLETED", but the router itself reports 12 unrouted
    /// and 52 violations.
    #[test]
    fn a_real_log_yields_the_routers_own_counts() {
        let summary = parse_routing_log(RUN).expect("score lines present");
        assert_eq!(summary.initial_unrouted, Some(64));
        assert_eq!(summary.autoroute_passes.len(), 5);
        assert_eq!(summary.autoroute_passes[0], RouterScore { score: 478.78, unrouted: 24, violations: 52 });
        assert_eq!(summary.autoroute_passes[4].unrouted, 14);
        assert_eq!(summary.final_score, Some(RouterScore { score: 660.59, unrouted: 12, violations: 52 }));
        assert_eq!(summary.complete(), Some(false));
    }

    #[test]
    fn mcp_json_lines_between_log_lines_are_ignored() {
        let mixed = format!("{{\"jsonrpc\":\"2.0\",\"result\":{{\"unrouted\":3}}}}\n{RUN}\n{{\"id\":9}}");
        assert_eq!(parse_routing_log(&mixed), parse_routing_log(RUN));
    }

    #[test]
    fn a_clean_finish_is_complete_and_a_silent_log_is_unknown() {
        let clean = "x INFO Auto-routing stage completed: started with 3 unrouted nets, completed in 1.0 seconds, final score: 990.00 (0 unrouted and 0 violations), using 1 total CPU seconds";
        let summary = parse_routing_log(clean).unwrap();
        assert_eq!(summary.initial_unrouted, Some(3));
        assert_eq!(summary.complete(), Some(true));
        assert_eq!(parse_routing_log("Freerouting v2.3.0 started\nno scores here"), None);
    }

    #[test]
    fn scores_parse_only_in_the_logged_shape() {
        assert_eq!(parse_score("660.59 (12 unrouted and 52 violations), using"), Some(RouterScore { score: 660.59, unrouted: 12, violations: 52 }));
        assert_eq!(parse_score("660.59"), None);
        assert_eq!(parse_score("abc (1 unrouted and 2 violations)"), None);
    }
}
