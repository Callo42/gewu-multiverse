//! Per-role custom external MCP servers (`agents/<role>/mcp.json`).
//!
//! Each role can connect to external services (databases, browsers, messaging,
//! issue trackers, cloud APIs) through its own `mcp.json` file while staying
//! strictly confined to its workspace on the filesystem. Secrets referenced via
//! `${ENV_VAR}` are resolved exclusively from that role's own configured
//! environment and `[agent_env_secrets.<role>]` entries — never from the host
//! environment or another role's secrets.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde_json::{Map, Value, json};

use crate::credentials::Secret;
use crate::engine::settings::validate_external_env_key;

/// Maximum size of a role's `mcp.json` file (256 KiB).
const MAX_MCP_CONFIG_BYTES: u64 = 256 * 1024;

/// Reserved MCP server name owned by the Multiverse Forgejo supervisor bridge.
pub const RESERVED_FORGEJO_SERVER: &str = "gewu_forgejo";

/// One validated external MCP server transport.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExternalMcpTransport {
    Stdio {
        command: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
    },
    Http {
        url: String,
        headers: BTreeMap<String, String>,
    },
}

/// One validated external MCP server configured for a single role.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalMcpServer {
    pub name: String,
    pub transport: ExternalMcpTransport,
}

/// Validate a custom MCP server name.
pub fn validate_server_name(name: &str) -> Result<(), String> {
    let trimmed = name.trim();
    if trimmed.is_empty() || trimmed.len() > 64 {
        return Err(format!(
            "MCP server name `{name}` must be 1–64 ASCII characters"
        ));
    }
    if trimmed == RESERVED_FORGEJO_SERVER {
        return Err(format!(
            "MCP server name `{RESERVED_FORGEJO_SERVER}` is reserved for the bundled Forgejo bridge"
        ));
    }
    if !trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
    {
        return Err(format!(
            "MCP server name `{name}` may only contain ASCII letters, digits, `-` and `_`"
        ));
    }
    Ok(())
}

/// Expand `${VAR}` placeholders using ONLY the role's non-secret `env` map.
/// Used for `command`, `args`, and HTTP `url` so secrets NEVER enter process
/// `argv` (`/proc/<pid>/cmdline`) or URL strings.
pub fn expand_role_env_only(
    raw: &str,
    env: &BTreeMap<String, String>,
    secrets: &BTreeMap<String, Secret>,
    field_label: &str,
) -> Result<String, String> {
    let mut out = String::with_capacity(raw.len());
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
            let Some(rel_end) = raw[i + 2..].find('}') else {
                return Err("unclosed `${...}` variable placeholder in mcp.json".to_owned());
            };
            let var_name = &raw[i + 2..i + 2 + rel_end];
            if var_name.is_empty()
                || !var_name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_')
            {
                return Err(format!(
                    "invalid variable name `${{{var_name}}}` in mcp.json"
                ));
            }
            if secrets.contains_key(var_name) {
                return Err(format!(
                    "mcp.json `{field_label}` cannot reference secret `${{{var_name}}}` (secrets are allowed only in `env` or HTTP `headers`, never in argv or URLs)"
                ));
            } else if let Some(plain) = env.get(var_name) {
                out.push_str(plain);
            } else {
                return Err(format!(
                    "mcp.json references `${{{var_name}}}`, which is not configured in this role's env"
                ));
            }
            i += 2 + rel_end + 1;
        } else {
            let ch = raw[i..].chars().next().expect("valid utf-8 boundary");
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    Ok(out)
}

/// Expand `${VAR}` placeholders using ONLY the role's own env + secret env maps.
/// Never reads host process environment variables.
pub fn expand_role_vars(
    raw: &str,
    env: &BTreeMap<String, String>,
    secrets: &BTreeMap<String, Secret>,
) -> Result<String, String> {
    let mut out = String::with_capacity(raw.len());
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
            let Some(rel_end) = raw[i + 2..].find('}') else {
                return Err("unclosed `${...}` variable placeholder in mcp.json".to_owned());
            };
            let var_name = &raw[i + 2..i + 2 + rel_end];
            if var_name.is_empty()
                || !var_name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_')
            {
                return Err(format!(
                    "invalid variable name `${{{var_name}}}` in mcp.json"
                ));
            }
            if let Some(secret) = secrets.get(var_name) {
                out.push_str(secret.expose());
            } else if let Some(plain) = env.get(var_name) {
                out.push_str(plain);
            } else {
                return Err(format!(
                    "mcp.json references `${{{var_name}}}`, which is not configured in this role's env or [agent_env_secrets.<role>]"
                ));
            }
            i += 2 + rel_end + 1;
        } else {
            let ch = raw[i..].chars().next().expect("valid utf-8 boundary");
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    Ok(out)
}

/// Load and validate `agents/<role>/mcp.json`, returning an empty list when absent.
pub fn load_role_mcp_servers(
    path: &Path,
    role_env: &BTreeMap<String, String>,
    role_secrets: &BTreeMap<String, Secret>,
) -> Result<Vec<ExternalMcpServer>, String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(format!("{} cannot be inspected: {error}", path.display()));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        return Err(format!(
            "{} must be a regular file (not a symlink or directory)",
            path.display()
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let my_uid = rustix::process::geteuid().as_raw();
        if metadata.uid() != my_uid {
            return Err(format!(
                "{} must be owned by the current user",
                path.display()
            ));
        }
        let mode = metadata.permissions().mode() & 0o7777;
        if mode & 0o022 != 0 {
            return Err(format!(
                "{} has group/world write permissions ({:04o}); restrict it to 0600 or 0644",
                path.display(),
                mode
            ));
        }
    }
    if metadata.len() > MAX_MCP_CONFIG_BYTES {
        return Err(format!(
            "{} exceeds the {} KiB limit",
            path.display(),
            MAX_MCP_CONFIG_BYTES / 1024
        ));
    }
    let text = crate::mode::read_private_regular(path)
        .map_err(|error| format!("{} cannot be read: {error}", path.display()))?;
    parse_role_mcp_json(&text, role_env, role_secrets)
}

/// Reject cloud instance-metadata / link-local / unspecified hosts (including
/// decimal, hex, octal, shorthand IPv4 and IPv4-mapped IPv6 encodings) and
/// embedded userinfo in role HTTP MCP server URLs.
fn validate_http_mcp_url(name: &str, url: &str) -> Result<(), String> {
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .ok_or_else(|| {
            format!("mcp.json server `{name}` `url` must start with http:// or https://")
        })?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.contains('@') {
        return Err(format!(
            "mcp.json server `{name}` `url` must not contain embedded userinfo (`@`); pass credentials via `headers`"
        ));
    }
    let host_raw = if let Some(bracketed) = authority.strip_prefix('[') {
        let Some((inner, _)) = bracketed.split_once(']') else {
            return Err(format!("mcp.json server `{name}` `url` has malformed IPv6 host"));
        };
        inner
    } else {
        authority.split(':').next().unwrap_or("")
    };
    let host = host_raw.trim().trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() {
        return Err(format!("mcp.json server `{name}` `url` has an empty host"));
    }
    if matches!(
        host.as_str(),
        "metadata.google.internal"
            | "metadata.goog"
            | "metadata.azure.internal"
            | "metadata.tencentyun.com"
            | "instance-data"
            | "instance-data.ec2.internal"
    ) {
        return Err(format!(
            "mcp.json server `{name}` `url` host `{host}` is a blocked cloud metadata endpoint"
        ));
    }
    if let Some(ip) = parse_ip_lenient(&host)
        && is_blocked_metadata_ip(ip)
    {
        return Err(format!(
            "mcp.json server `{name}` `url` host `{host}` resolves to a blocked link-local/metadata IP ({ip})"
        ));
    }
    Ok(())
}

/// Parse standard IPv4/IPv6 as well as legacy `inet_aton` numeric forms (single
/// integer `2852039166`, hex `0xa9fea9fe`, octal `0251.0376.0251.0376`, or
/// 2/3-part shorthand `169.254.43518`) so obfuscated metadata URLs are caught.
fn parse_ip_lenient(host: &str) -> Option<std::net::IpAddr> {
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return Some(ip);
    }
    // Strip optional IPv6 zone index (e.g. fe80::1%eth0).
    if let Some((addr_part, _)) = host.split_once('%')
        && let Ok(v6) = addr_part.parse::<std::net::Ipv6Addr>()
    {
        return Some(std::net::IpAddr::V6(v6));
    }
    parse_ipv4_aton(host).map(std::net::IpAddr::V4)
}

fn parse_ipv4_part(part: &str) -> Option<u64> {
    if part.is_empty() {
        return None;
    }
    if let Some(hex) = part.strip_prefix("0x").or_else(|| part.strip_prefix("0X")) {
        if hex.is_empty() {
            return None;
        }
        u64::from_str_radix(hex, 16).ok()
    } else if part.len() > 1 && part.starts_with('0') {
        u64::from_str_radix(&part[1..], 8).ok()
    } else {
        part.parse::<u64>().ok()
    }
}

fn parse_ipv4_aton(host: &str) -> Option<std::net::Ipv4Addr> {
    let parts: Vec<&str> = host.split('.').collect();
    if parts.is_empty() || parts.len() > 4 {
        return None;
    }
    let mut vals = Vec::with_capacity(parts.len());
    for p in parts {
        vals.push(parse_ipv4_part(p)?);
    }
    let num: u32 = match vals.as_slice() {
        [a] => u32::try_from(*a).ok()?,
        [a, b] if *a <= 0xff && *b <= 0xff_ffff => ((*a as u32) << 24) | (*b as u32),
        [a, b, c] if *a <= 0xff && *b <= 0xff && *c <= 0xffff => {
            ((*a as u32) << 24) | ((*b as u32) << 16) | (*c as u32)
        }
        [a, b, c, d] if *a <= 0xff && *b <= 0xff && *c <= 0xff && *d <= 0xff => {
            ((*a as u32) << 24) | ((*b as u32) << 16) | ((*c as u32) << 8) | (*d as u32)
        }
        _ => return None,
    };
    Some(std::net::Ipv4Addr::from(num))
}

fn is_blocked_metadata_v4(v4: std::net::Ipv4Addr) -> bool {
    let o = v4.octets();
    // 169.254.0.0/16 (AWS/GCP/Azure/OpenStack link-local metadata),
    // 100.100.100.200 (Alibaba Cloud metadata), 0.0.0.0 (unspecified).
    (o[0] == 169 && o[1] == 254)
        || (o == [100, 100, 100, 200])
        || v4.is_unspecified()
}

fn is_blocked_metadata_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => is_blocked_metadata_v4(v4),
        std::net::IpAddr::V6(v6) => {
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return is_blocked_metadata_v4(mapped);
            }
            if let Some(compat) = v6.to_ipv4()
                && !v6.is_loopback()
            {
                return is_blocked_metadata_v4(compat);
            }
            let segs = v6.segments();
            // Link-local fe80::/10, AWS EC2 IPv6 IMDS fd00:ec2::254, unspecified ::
            (segs[0] & 0xffc0) == 0xfe80
                || segs == [0xfd00, 0x0ec2, 0, 0, 0, 0, 0, 0x0254]
                || v6.is_unspecified()
        }
    }
}

/// Parse and validate one `mcp.json` document.
pub fn parse_role_mcp_json(
    text: &str,
    role_env: &BTreeMap<String, String>,
    role_secrets: &BTreeMap<String, Secret>,
) -> Result<Vec<ExternalMcpServer>, String> {
    let root: Value = serde_json::from_str(text)
        .map_err(|error| format!("mcp.json is not valid JSON: {error}"))?;
    let Some(root_obj) = root.as_object() else {
        return Err("mcp.json must be a JSON object".to_owned());
    };
    let servers_val = root_obj
        .get("mcpServers")
        .or_else(|| root_obj.get("mcp_servers"))
        .ok_or_else(|| "mcp.json must contain an `mcpServers` object".to_owned())?;
    let Some(servers_obj) = servers_val.as_object() else {
        return Err("mcp.json `mcpServers` must be a JSON object".to_owned());
    };

    let mut servers = Vec::new();
    for (name, spec) in servers_obj {
        validate_server_name(name)?;
        let Some(obj) = spec.as_object() else {
            return Err(format!("mcp.json server `{name}` must be a JSON object"));
        };
        if let Some(enabled) = obj.get("enabled").and_then(Value::as_bool)
            && !enabled
        {
            continue;
        }
        if let Some(command_raw) = obj.get("command").and_then(Value::as_str) {
            let command = expand_role_env_only(command_raw.trim(), role_env, role_secrets, "command")?;
            if command.is_empty() {
                return Err(format!("mcp.json server `{name}` has an empty `command`"));
            }
            let mut args = Vec::new();
            if let Some(raw_args) = obj.get("args") {
                let Some(arr) = raw_args.as_array() else {
                    return Err(format!(
                        "mcp.json server `{name}` field `args` must be an array of strings"
                    ));
                };
                for item in arr {
                    let Some(arg_str) = item.as_str() else {
                        return Err(format!(
                            "mcp.json server `{name}` field `args` must contain only strings"
                        ));
                    };
                    args.push(expand_role_env_only(arg_str, role_env, role_secrets, "args")?);
                }
            }
            let mut env = BTreeMap::new();
            if let Some(raw_env) = obj.get("env") {
                let Some(env_obj) = raw_env.as_object() else {
                    return Err(format!(
                        "mcp.json server `{name}` field `env` must be an object"
                    ));
                };
                for (k, v) in env_obj {
                    validate_external_env_key(k)?;
                    let Some(v_str) = v.as_str() else {
                        return Err(format!(
                            "mcp.json server `{name}` env `{k}` must be a string"
                        ));
                    };
                    env.insert(k.clone(), expand_role_vars(v_str, role_env, role_secrets)?);
                }
            }
            servers.push(ExternalMcpServer {
                name: name.clone(),
                transport: ExternalMcpTransport::Stdio { command, args, env },
            });
        } else if let Some(url_raw) = obj.get("url").and_then(Value::as_str) {
            let url = expand_role_env_only(url_raw.trim(), role_env, role_secrets, "url")?;
            validate_http_mcp_url(name, &url)?;
            let mut headers = BTreeMap::new();
            if let Some(raw_headers) = obj.get("headers") {
                let Some(h_obj) = raw_headers.as_object() else {
                    return Err(format!(
                        "mcp.json server `{name}` field `headers` must be an object"
                    ));
                };
                for (k, v) in h_obj {
                    let Some(v_str) = v.as_str() else {
                        return Err(format!(
                            "mcp.json server `{name}` header `{k}` must be a string"
                        ));
                    };
                    headers.insert(k.clone(), expand_role_vars(v_str, role_env, role_secrets)?);
                }
            }
            servers.push(ExternalMcpServer {
                name: name.clone(),
                transport: ExternalMcpTransport::Http { url, headers },
            });
        } else {
            return Err(format!(
                "mcp.json server `{name}` must specify either `command` (stdio) or `url` (http)"
            ));
        }
    }
    servers.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(servers)
}

/// Insert external MCP servers into Pi's `mcpServers` map.
pub fn insert_pi_servers(target: &mut Map<String, Value>, servers: &[ExternalMcpServer]) {
    for server in servers {
        match &server.transport {
            ExternalMcpTransport::Stdio { command, args, env } => {
                let mut entry = Map::new();
                entry.insert("command".to_owned(), json!(command));
                entry.insert("args".to_owned(), json!(args));
                if !env.is_empty() {
                    entry.insert("env".to_owned(), json!(env));
                }
                entry.insert("exposure".to_owned(), json!("direct"));
                target.insert(server.name.clone(), Value::Object(entry));
            }
            ExternalMcpTransport::Http { url, headers } => {
                let mut entry = Map::new();
                entry.insert("url".to_owned(), json!(url));
                if !headers.is_empty() {
                    entry.insert("headers".to_owned(), json!(headers));
                }
                entry.insert("exposure".to_owned(), json!("direct"));
                target.insert(server.name.clone(), Value::Object(entry));
            }
        }
    }
}

/// Render external MCP servers as Codex TOML `[mcp_servers.<name>]` sections.
pub fn render_codex_toml_servers(servers: &[ExternalMcpServer]) -> String {
    let mut out = String::new();
    for server in servers {
        match &server.transport {
            ExternalMcpTransport::Stdio { command, args, env } => {
                out.push_str(&format!("\n[mcp_servers.{}]\n", server.name));
                out.push_str(&format!("command = {}\n", toml_escape(command)));
                let rendered_args: Vec<String> = args.iter().map(|a| toml_escape(a)).collect();
                out.push_str(&format!("args = [{}]\n", rendered_args.join(", ")));
                out.push_str("startup_timeout_sec = 20\ntool_timeout_sec = 120\n");
                if !env.is_empty() {
                    out.push_str(&format!("\n[mcp_servers.{}.env]\n", server.name));
                    for (k, v) in env {
                        out.push_str(&format!("{k} = {}\n", toml_escape(v)));
                    }
                }
            }
            ExternalMcpTransport::Http { url, headers } => {
                out.push_str(&format!("\n[mcp_servers.{}]\n", server.name));
                out.push_str(&format!("url = {}\n", toml_escape(url)));
                out.push_str("startup_timeout_sec = 20\ntool_timeout_sec = 120\n");
                if !headers.is_empty() {
                    out.push_str(&format!("\n[mcp_servers.{}.http_headers]\n", server.name));
                    for (k, v) in headers {
                        out.push_str(&format!("{} = {}\n", toml_escape(k), toml_escape(v)));
                    }
                }
            }
        }
    }
    out
}

/// Render external MCP servers as DSH `overlay.yml` plugin insertions.
pub fn render_dsh_overlay_servers(servers: &[ExternalMcpServer]) -> String {
    let mut out = String::new();
    for server in servers {
        match &server.transport {
            ExternalMcpTransport::Stdio { command, args, env } => {
                let id = format!("role-mcp-{}", server.name);
                let rendered_args: Vec<String> = args.iter().map(|a| yaml_escape(a)).collect();
                out.push_str(&format!(
                    "- insert:\n    - id: {}\n      name: '@deepseek-ai/dsh-mcp-client'\n      config:\n        serverName: {}\n        transport: stdio\n        command: {}\n        args: [{}]\n",
                    yaml_escape(&id),
                    yaml_escape(&server.name),
                    yaml_escape(command),
                    rendered_args.join(", "),
                ));
                if !env.is_empty() {
                    out.push_str("        env:\n");
                    for (k, v) in env {
                        out.push_str(&format!("          {}: {}\n", k, yaml_escape(v)));
                    }
                }
            }
            ExternalMcpTransport::Http { url, headers } => {
                let id = format!("role-mcp-{}", server.name);
                out.push_str(&format!(
                    "- insert:\n    - id: {}\n      name: '@deepseek-ai/dsh-mcp-client'\n      config:\n        serverName: {}\n        transport: http\n        url: {}\n",
                    yaml_escape(&id),
                    yaml_escape(&server.name),
                    yaml_escape(url),
                ));
                if !headers.is_empty() {
                    out.push_str("        headers:\n");
                    for (k, v) in headers {
                        out.push_str(&format!(
                            "          {}: {}\n",
                            yaml_escape(k),
                            yaml_escape(v)
                        ));
                    }
                }
            }
        }
    }
    out
}

/// Insert external MCP servers into OpenCode's `mcp` config object.
pub fn insert_opencode_servers(target: &mut Map<String, Value>, servers: &[ExternalMcpServer]) {
    for server in servers {
        match &server.transport {
            ExternalMcpTransport::Stdio { command, args, env } => {
                let mut full_cmd = Vec::with_capacity(args.len() + 1);
                full_cmd.push(command.clone());
                full_cmd.extend(args.iter().cloned());
                let mut entry = Map::new();
                entry.insert("type".to_owned(), json!("local"));
                entry.insert("command".to_owned(), json!(full_cmd));
                entry.insert("enabled".to_owned(), json!(true));
                if !env.is_empty() {
                    entry.insert("environment".to_owned(), json!(env));
                }
                target.insert(server.name.clone(), Value::Object(entry));
            }
            ExternalMcpTransport::Http { url, headers } => {
                let mut entry = Map::new();
                entry.insert("type".to_owned(), json!("remote"));
                entry.insert("url".to_owned(), json!(url));
                entry.insert("enabled".to_owned(), json!(true));
                if !headers.is_empty() {
                    entry.insert("headers".to_owned(), json!(headers));
                }
                target.insert(server.name.clone(), Value::Object(entry));
            }
        }
    }
}

fn toml_escape(text: &str) -> String {
    format!(
        "\"{}\"",
        text.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
            .replace('\r', "\\r")
    )
}

fn yaml_escape(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_forgejo_server_name_is_rejected() {
        let env = BTreeMap::new();
        let secrets = BTreeMap::new();
        let raw = r#"{"mcpServers":{"gewu_forgejo":{"command":"echo"}}}"#;
        let err = parse_role_mcp_json(raw, &env, &secrets).expect_err("reserved name rejected");
        assert!(err.contains("reserved"));
    }

    #[test]
    fn role_var_expansion_uses_only_role_env_and_secrets() {
        let mut env = BTreeMap::new();
        env.insert("API_HOST".to_owned(), "https://api.example.com".to_owned());
        let mut secrets = BTreeMap::new();
        secrets.insert("EXT_KEY".to_owned(), Secret::new("tok-secret-999"));
        let raw = r#"{
            "mcpServers": {
                "ext-http": {
                    "url": "${API_HOST}/mcp",
                    "headers": { "Authorization": "Bearer ${EXT_KEY}" }
                },
                "ext-stdio": {
                    "command": "my-mcp",
                    "args": ["--endpoint", "${API_HOST}"],
                    "env": { "SERVICE_TOKEN": "${EXT_KEY}" }
                }
            }
        }"#;
        let servers = parse_role_mcp_json(raw, &env, &secrets).expect("valid mcp.json");
        assert_eq!(servers.len(), 2);
        match &servers[0].transport {
            ExternalMcpTransport::Http { url, headers } => {
                assert_eq!(url, "https://api.example.com/mcp");
                assert_eq!(
                    headers.get("Authorization").map(String::as_str),
                    Some("Bearer tok-secret-999")
                );
            }
            _ => panic!("expected http"),
        }
        // Unconfigured variable (even if set on host, e.g. PATH) must fail closed.
        let bad = r#"{"mcpServers":{"x":{"command":"echo","args":["${PATH}"]}}}"#;
        assert!(parse_role_mcp_json(bad, &env, &secrets).is_err());
    }

    #[test]
    fn forbidden_env_keys_in_mcp_stdio_are_refused() {
        let env = BTreeMap::new();
        let secrets = BTreeMap::new();
        let bad =
            r#"{"mcpServers":{"x":{"command":"echo","env":{"LD_PRELOAD":"/tmp/evil.so"}}}}"#;
        assert!(parse_role_mcp_json(bad, &env, &secrets).is_err());
    }

    #[test]
    fn secrets_are_forbidden_in_mcp_command_args_and_url_and_metadata_ssrf_is_blocked() {
        let env = BTreeMap::new();
        let mut secrets = BTreeMap::new();
        secrets.insert("EXT_KEY".to_owned(), Secret::new("tok-secret-999"));
        let bad_args = r#"{"mcpServers":{"x":{"command":"my-mcp","args":["--key","${EXT_KEY}"]}}}"#;
        assert!(parse_role_mcp_json(bad_args, &env, &secrets).is_err());
        let bad_url = r#"{"mcpServers":{"x":{"url":"https://api.example.com/?k=${EXT_KEY}"}}}"#;
        assert!(parse_role_mcp_json(bad_url, &env, &secrets).is_err());
        let bad_meta = r#"{"mcpServers":{"x":{"url":"http://169.254.169.254/latest/meta-data/"}}}"#;
        assert!(parse_role_mcp_json(bad_meta, &env, &secrets).is_err());
        for obfuscated in [
            "http://2852039166/latest/meta-data/",
            "http://0xa9fea9fe/latest/meta-data/",
            "http://0251.0376.0251.0376/latest/meta-data/",
            "http://169.254.43518/latest/meta-data/",
            "http://[::ffff:169.254.169.254]/latest/meta-data/",
            "http://[::ffff:a9fe:a9fe]/latest/meta-data/",
            "http://[fd00:ec2::254]/latest/meta-data/",
            "http://[fe80::1]/mcp",
            "http://100.100.100.200/latest/meta-data/",
            "https://user:pass@api.example.com/mcp",
        ] {
            let raw = format!(r#"{{"mcpServers":{{"x":{{"url":"{obfuscated}"}}}}}}"#);
            assert!(
                parse_role_mcp_json(&raw, &env, &secrets).is_err(),
                "expected SSRF/userinfo URL `{obfuscated}` to be rejected"
            );
        }
    }
}
