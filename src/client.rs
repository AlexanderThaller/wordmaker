//! `wordmaker client`: a command line MCP client for a running `WordMaker`
//! server, so agents without MCP support can drive the CMS from a shell.

use std::path::PathBuf;

use anyhow::{
    Context,
    bail,
};
use base64::Engine;
use rmcp::{
    RoleClient,
    ServiceExt,
    model::{
        CallToolRequestParams,
        ContentBlock,
        JsonObject,
        Tool,
    },
    service::RunningService,
    transport::{
        StreamableHttpClientTransport,
        streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use serde_json::Value;

use crate::config::Config;

#[derive(clap::Args)]
pub struct ClientArgs {
    /// MCP endpoint; defaults to `<base-url>/mcp`.
    #[arg(long, env = "WORDMAKER_MCP_URL")]
    url: Option<String>,

    /// Bearer token of the server.
    #[arg(long, env = "WORDMAKER_TOKEN", hide_env_values = true)]
    token: Option<String>,

    #[command(subcommand)]
    command: ClientCommand,
}

#[derive(clap::Subcommand)]
enum ClientCommand {
    /// Print the server's name, version and usage instructions.
    Info,
    /// List the tools, or describe one tool in detail.
    Tools {
        /// Show only this tool.
        name: Option<String>,
        /// Print the raw JSON tool definitions, with full input schemas.
        #[arg(long)]
        json: bool,
    },
    /// Call a tool and print its result.
    ///
    /// Arguments come from a JSON object (`-` reads it from stdin) and/or
    /// the `--arg`, `--json`, `--file` and `--base64` options, which take
    /// precedence.
    #[command(verbatim_doc_comment)]
    Call(CallArgs),
}

#[derive(clap::Args)]
struct CallArgs {
    /// Tool name, e.g. `create_page`.
    tool: String,

    /// Arguments as a JSON object, or `-` to read it from stdin.
    arguments: Option<String>,

    /// String argument, `KEY=VALUE`.
    #[arg(long = "arg", value_name = "KEY=VALUE")]
    args: Vec<String>,

    /// JSON argument, `KEY=JSON`, e.g. `--json publish=true`.
    #[arg(long = "json", value_name = "KEY=JSON")]
    json_args: Vec<String>,

    /// String argument read from a file, `KEY=PATH`, e.g. `--file
    /// body=page.adoc`.
    #[arg(long = "file", value_name = "KEY=PATH")]
    file_args: Vec<String>,

    /// Base64 encoded file argument, `KEY=PATH`, e.g.
    /// `--base64 content_base64=logo.png`.
    #[arg(long = "base64", value_name = "KEY=PATH")]
    base64_args: Vec<String>,
}

pub async fn run(config: &Config, args: ClientArgs) -> anyhow::Result<()> {
    // Read arguments before connecting, so mistakes fail fast.
    let call = match &args.command {
        ClientCommand::Call(call) => Some(call_arguments(call)?),
        _ => None,
    };

    let client = connect(config, args.url, args.token).await?;
    let result = match args.command {
        ClientCommand::Info => {
            info(&client);
            Ok(())
        }
        ClientCommand::Tools { name, json } => tools(&client, name.as_deref(), json).await,
        ClientCommand::Call(CallArgs { tool, .. }) => {
            call_tool(&client, tool, call.unwrap_or_default()).await
        }
    };
    let _ = client.cancel().await;
    result
}

async fn connect(
    config: &Config,
    url: Option<String>,
    token: Option<String>,
) -> anyhow::Result<RunningService<RoleClient, ()>> {
    let url = url.unwrap_or_else(|| format!("{}/mcp", config.base_url.trim_end_matches('/')));
    let token = token
        .filter(|t| !t.trim().is_empty())
        .context("WORDMAKER_TOKEN must be set to the server's token")?;
    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(url.clone()).auth_header(token.clone()),
    );
    match ().serve(transport).await {
        Ok(client) => Ok(client),
        // rmcp's worker logs the HTTP failure and hands back only "transport
        // channel closed", so ask the endpoint again to say what went wrong.
        Err(err) => match diagnose(&url, &token).await {
            Some(cause) => bail!("could not connect to the MCP server at {url}: {cause}"),
            None => Err(anyhow::Error::new(err)
                .context(format!("could not connect to the MCP server at {url}"))),
        },
    }
}

/// Probe `url` with a plain `initialize` request and describe why it fails,
/// or `None` when the endpoint answers fine and the cause lies elsewhere.
async fn diagnose(url: &str, token: &str) -> Option<String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .ok()?;
    let response = client
        .post(url)
        .bearer_auth(token)
        .header(reqwest::header::ACCEPT, "application/json, text/event-stream")
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(serde_json::json!({
            "jsonrpc": "2.0",
            "id": 0,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": { "name": "wordmaker-client", "version": env!("CARGO_PKG_VERSION") },
            },
        })
        .to_string())
        .send()
        .await;
    match response {
        Err(err) if err.is_connect() || err.is_timeout() => {
            Some(format!("the server is not reachable ({err})"))
        }
        Err(err) => Some(err.to_string()),
        Ok(response) if response.status().is_success() => None,
        Ok(response) => {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            Some(describe_failure(status, &body))
        }
    }
}

fn describe_failure(status: reqwest::StatusCode, body: &str) -> String {
    // The server answers errors as `{"error": "..."}`.
    let detail = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v["error"].as_str().map(str::to_owned))
        .unwrap_or_else(|| body.trim().chars().take(200).collect());
    let detail = if detail.is_empty() {
        String::new()
    } else {
        format!(": {detail}")
    };
    let hint = match status {
        reqwest::StatusCode::UNAUTHORIZED => "; check WORDMAKER_TOKEN or --token",
        reqwest::StatusCode::FORBIDDEN => {
            "; the server may not accept this host name, see WORDMAKER_ALLOWED_HOSTS"
        }
        // A path the server does not route lands on the public site, which
        // only answers GET.
        reqwest::StatusCode::NOT_FOUND | reqwest::StatusCode::METHOD_NOT_ALLOWED => {
            "; check WORDMAKER_MCP_URL or --url"
        }
        _ => "",
    };
    format!("HTTP {status}{detail}{hint}")
}

fn info(client: &RunningService<RoleClient, ()>) {
    let Some(server) = client.peer_info() else {
        return;
    };
    if let Some(implementation) = &server.server_info {
        println!("{} {}", implementation.name, implementation.version);
    }
    if let Some(instructions) = &server.instructions {
        println!("\n{instructions}");
    }
}

async fn tools(
    client: &RunningService<RoleClient, ()>,
    name: Option<&str>,
    json: bool,
) -> anyhow::Result<()> {
    let mut tools = client.list_all_tools().await?;
    if let Some(name) = name {
        tools.retain(|tool| tool.name == name);
        if tools.is_empty() {
            bail!("no tool named `{name}`; run `wordmaker client tools` to list them");
        }
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&tools)?);
        return Ok(());
    }
    for (i, tool) in tools.iter().enumerate() {
        if i > 0 {
            println!();
        }
        print_tool(tool);
    }
    if name.is_none() {
        println!("\nCall a tool with `wordmaker client call <TOOL> [JSON] [--arg KEY=VALUE]...`.");
    }
    Ok(())
}

fn print_tool(tool: &Tool) {
    println!("{}", tool.name);
    if let Some(description) = &tool.description {
        println!("  {description}");
    }
    let schema = Value::Object((*tool.input_schema).clone());
    let required: Vec<&str> = schema["required"]
        .as_array()
        .map_or_else(Vec::new, |r| r.iter().filter_map(Value::as_str).collect());
    let Some(properties) = schema["properties"].as_object() else {
        return;
    };
    for (key, property) in properties {
        let kind = type_name(&schema, property);
        let flag = if required.contains(&key.as_str()) {
            ", required"
        } else {
            ""
        };
        print!("    {key} ({kind}{flag})");
        match property["description"].as_str() {
            Some(description) => println!(": {}", description.replace('\n', " ")),
            None => println!(),
        }
    }
}

/// A short type name for a property schema, following `$ref`s and
/// `anyOf`/`oneOf` (as schemars emits for `Option` and enums).
fn type_name(root: &Value, property: &Value) -> String {
    if let Some(reference) = property["$ref"].as_str() {
        let target = reference
            .strip_prefix("#/")
            .map(|path| path.split('/').fold(root, |value, key| &value[key]));
        return match target {
            Some(target) if !target.is_null() => type_name(root, target),
            _ => "object".to_owned(),
        };
    }
    if let Some(values) = property["enum"].as_array() {
        return values
            .iter()
            .map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_owned))
            .collect::<Vec<_>>()
            .join("|");
    }
    if let Some(variants) = property["anyOf"]
        .as_array()
        .or_else(|| property["oneOf"].as_array())
    {
        let names: Vec<String> = variants
            .iter()
            .map(|v| type_name(root, v))
            .filter(|n| n != "null")
            .collect();
        return names.join("|");
    }
    match &property["type"] {
        Value::String(kind) if kind == "array" => {
            format!("{}[]", type_name(root, &property["items"]))
        }
        Value::String(kind) => kind.clone(),
        Value::Array(kinds) => kinds
            .iter()
            .filter_map(Value::as_str)
            .filter(|k| *k != "null")
            .collect::<Vec<_>>()
            .join("|"),
        _ => "any".to_owned(),
    }
}

async fn call_tool(
    client: &RunningService<RoleClient, ()>,
    tool: String,
    arguments: JsonObject,
) -> anyhow::Result<()> {
    let result = client
        .call_tool(CallToolRequestParams::new(tool).with_arguments(arguments))
        .await?;

    let mut output = String::new();
    for block in &result.content {
        if !output.is_empty() {
            output.push('\n');
        }
        match block {
            ContentBlock::Text(text) => output.push_str(&text.text),
            other => output.push_str(&serde_json::to_string_pretty(other)?),
        }
    }
    if output.is_empty()
        && let Some(structured) = &result.structured_content
    {
        output = serde_json::to_string_pretty(structured)?;
    }

    if result.is_error == Some(true) {
        bail!("{output}");
    }
    println!("{output}");
    Ok(())
}

fn call_arguments(call: &CallArgs) -> anyhow::Result<JsonObject> {
    let mut arguments = match call.arguments.as_deref() {
        None => JsonObject::new(),
        Some(source) => {
            let source = if source == "-" {
                std::io::read_to_string(std::io::stdin()).context("could not read stdin")?
            } else {
                source.to_owned()
            };
            match serde_json::from_str(&source).context("arguments are not valid JSON")? {
                Value::Object(object) => object,
                _ => bail!("arguments must be a JSON object"),
            }
        }
    };

    for arg in &call.args {
        let (key, value) = split_pair(arg, "--arg")?;
        arguments.insert(key.to_owned(), Value::String(value.to_owned()));
    }
    for arg in &call.json_args {
        let (key, value) = split_pair(arg, "--json")?;
        let value = serde_json::from_str(value)
            .with_context(|| format!("--json {key}: `{value}` is not valid JSON"))?;
        arguments.insert(key.to_owned(), value);
    }
    for arg in &call.file_args {
        let (key, path) = split_pair(arg, "--file")?;
        let text = std::fs::read_to_string(PathBuf::from(path))
            .with_context(|| format!("--file {key}: could not read {path}"))?;
        arguments.insert(key.to_owned(), Value::String(text));
    }
    for arg in &call.base64_args {
        let (key, path) = split_pair(arg, "--base64")?;
        let bytes = std::fs::read(PathBuf::from(path))
            .with_context(|| format!("--base64 {key}: could not read {path}"))?;
        arguments.insert(
            key.to_owned(),
            Value::String(base64::engine::general_purpose::STANDARD.encode(bytes)),
        );
    }
    Ok(arguments)
}

fn split_pair<'a>(arg: &'a str, option: &str) -> anyhow::Result<(&'a str, &'a str)> {
    match arg.split_once('=') {
        Some((key, value)) if !key.is_empty() => Ok((key, value)),
        _ => bail!("{option} expects KEY=VALUE, got `{arg}`"),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn call(arguments: Option<&str>, args: &[&str], json_args: &[&str]) -> CallArgs {
        CallArgs {
            tool: "t".into(),
            arguments: arguments.map(str::to_owned),
            args: args.iter().map(|s| (*s).to_owned()).collect(),
            json_args: json_args.iter().map(|s| (*s).to_owned()).collect(),
            file_args: Vec::new(),
            base64_args: Vec::new(),
        }
    }

    #[test]
    fn options_override_json_arguments() {
        let arguments = call_arguments(&call(
            Some(r#"{"slug": "a", "publish": false}"#),
            &["slug=b=c", "title=123"],
            &["publish=true"],
        ))
        .unwrap();
        assert_eq!(
            Value::Object(arguments),
            json!({ "slug": "b=c", "title": "123", "publish": true })
        );
    }

    #[test]
    fn rejects_bad_arguments() {
        assert!(call_arguments(&call(Some("[1]"), &[], &[])).is_err());
        assert!(call_arguments(&call(None, &["novalue"], &[])).is_err());
        assert!(call_arguments(&call(None, &[], &["x=nope"])).is_err());
    }

    #[test]
    fn describes_http_failures() {
        let msg = describe_failure(
            reqwest::StatusCode::UNAUTHORIZED,
            r#"{"error":"missing or invalid API token"}"#,
        );
        assert_eq!(
            msg,
            "HTTP 401 Unauthorized: missing or invalid API token; check WORDMAKER_TOKEN or --token"
        );
        let msg = describe_failure(reqwest::StatusCode::NOT_FOUND, "");
        assert_eq!(msg, "HTTP 404 Not Found; check WORDMAKER_MCP_URL or --url");
        let msg = describe_failure(
            reqwest::StatusCode::BAD_GATEWAY,
            "<html>proxy down</html>\n",
        );
        assert_eq!(msg, "HTTP 502 Bad Gateway: <html>proxy down</html>");
    }

    #[test]
    fn type_names() {
        let root = json!({
            "$defs": { "Status": { "type": "string", "enum": ["draft", "published"] } },
        });
        let name = |p: Value| type_name(&root, &p);
        assert_eq!(name(json!({ "type": ["string", "null"] })), "string");
        assert_eq!(
            name(json!({ "type": "array", "items": { "type": "string" } })),
            "string[]"
        );
        assert_eq!(
            name(json!({ "anyOf": [{ "$ref": "#/$defs/Status" }, { "type": "null" }] })),
            "draft|published"
        );
    }
}
