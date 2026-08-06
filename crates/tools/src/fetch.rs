use crate::ToolContext;
use crate::ToolError;
use crate::ToolExecutor;
use crate::ToolOutput;
use async_trait::async_trait;
use futures::StreamExt;
use papermachine_protocol::ToolDefinition;
use reqwest::header::ACCEPT;
use reqwest::header::LOCATION;
use reqwest::header::USER_AGENT;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;
use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::Ipv6Addr;
use std::time::Duration;
use url::Host;
use url::Url;

const DEFAULT_MAX_BYTES: usize = 1024 * 1024;
const MAX_BYTES: usize = 2 * 1024 * 1024;
const DEFAULT_MAX_CHARS: usize = 20_000;
const MAX_CHARS: usize = 50_000;
const MAX_REDIRECTS: usize = 5;

#[derive(Clone, Copy, Default)]
pub struct FetchUrlTool;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FetchUrlArgs {
    url: String,
    max_bytes: Option<usize>,
    max_chars: Option<usize>,
    timeout_seconds: Option<u64>,
}

#[async_trait]
impl ToolExecutor for FetchUrlTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "fetch_url".to_string(),
            description: "Fetch a public HTTPS text, HTML, JSON, or XML resource. HTML is converted to readable text; downloads and returned text are independently bounded.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "url": {"type": "string", "description": "Public HTTPS URL"},
                    "max_bytes": {"type": "integer", "minimum": 1, "maximum": MAX_BYTES},
                    "max_chars": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_CHARS,
                        "description": "Maximum readable characters returned to the model"
                    },
                    "timeout_seconds": {"type": "integer", "minimum": 1, "maximum": 60}
                },
                "required": ["url"],
                "additionalProperties": false
            }),
            supports_parallel: true,
        }
    }

    fn supports_parallel(&self) -> bool {
        true
    }

    async fn execute(
        &self,
        context: ToolContext,
        arguments: Value,
    ) -> Result<ToolOutput, ToolError> {
        if !context.access.allows_research_network() {
            return Err(ToolError::PermissionDenied {
                tool: "fetch_url".to_string(),
                access: context.access,
            });
        }
        let args: FetchUrlArgs =
            serde_json::from_value(arguments).map_err(|error| ToolError::InvalidArguments {
                tool: "fetch_url".to_string(),
                message: error.to_string(),
            })?;
        let mut url = Url::parse(&args.url).map_err(|error| ToolError::InvalidArguments {
            tool: "fetch_url".to_string(),
            message: error.to_string(),
        })?;
        let max_bytes = args
            .max_bytes
            .unwrap_or(DEFAULT_MAX_BYTES)
            .clamp(1, MAX_BYTES);
        let max_chars = args
            .max_chars
            .unwrap_or(DEFAULT_MAX_CHARS)
            .clamp(1, MAX_CHARS);
        let timeout_seconds = args.timeout_seconds.unwrap_or(20).clamp(1, 60);

        for redirect_count in 0..=MAX_REDIRECTS {
            let destination = validate_destination(&url).await?;
            let host = url.host_str().ok_or_else(|| {
                ToolError::Network("fetch URL does not contain a host".to_string())
            })?;
            let client = reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(timeout_seconds))
                .resolve(host, destination)
                .build()
                .map_err(|error| ToolError::Network(error.to_string()))?;
            let response = tokio::select! {
                response = client
                    .get(url.clone())
                    .header(USER_AGENT, "PaperMachine/0.1")
                    .header(ACCEPT, "text/html, text/plain, application/json, application/xml, text/xml;q=0.9")
                    .timeout(Duration::from_secs(timeout_seconds))
                    .send() => response.map_err(|error| ToolError::Network(error_chain(&error)))?,
                _ = context.cancellation.cancelled() => return Err(ToolError::Cancelled),
            };
            if response.status().is_redirection() {
                if redirect_count == MAX_REDIRECTS {
                    return Err(ToolError::Network(format!(
                        "fetch exceeded {MAX_REDIRECTS} redirects"
                    )));
                }
                let location = response
                    .headers()
                    .get(LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .ok_or_else(|| {
                        ToolError::Network("redirect response has no valid Location".to_string())
                    })?;
                url = url
                    .join(location)
                    .map_err(|error| ToolError::Network(error.to_string()))?;
                continue;
            }
            if !response.status().is_success() {
                return Err(ToolError::Network(format!(
                    "fetch returned HTTP {}",
                    response.status()
                )));
            }
            let status = response.status().as_u16();
            let content_type = response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .unwrap_or("application/octet-stream")
                .to_string();
            if !supported_content_type(&content_type) {
                return Err(ToolError::Network(format!(
                    "unsupported fetch content type {content_type}"
                )));
            }
            let mut stream = response.bytes_stream();
            let mut bytes = Vec::with_capacity(max_bytes.min(16 * 1024));
            let mut truncated = false;
            loop {
                let next = tokio::select! {
                    next = stream.next() => next,
                    _ = context.cancellation.cancelled() => return Err(ToolError::Cancelled),
                };
                let Some(chunk) = next else {
                    break;
                };
                let chunk = chunk.map_err(|error| ToolError::Network(error_chain(&error)))?;
                let remaining = max_bytes.saturating_sub(bytes.len());
                let take = chunk.len().min(remaining);
                bytes.extend_from_slice(&chunk[..take]);
                if take < chunk.len() || bytes.len() == max_bytes {
                    truncated = true;
                    break;
                }
            }
            let (content, extracted) = readable_content(&content_type, &bytes)?;
            let (content, text_truncated) = truncate_chars(content, max_chars);
            let returned_chars = content.chars().count();
            return Ok(ToolOutput {
                value: json!({
                    "url": url.as_str(),
                    "status": status,
                    "content_type": content_type,
                    "content": content,
                    "downloaded_bytes": bytes.len(),
                    "returned_chars": returned_chars,
                    "extracted_html": extracted,
                    "download_truncated": truncated,
                    "text_truncated": text_truncated,
                }),
                summary: format!(
                    "fetched {} bytes and returned {} readable characters from {url}",
                    bytes.len(),
                    returned_chars
                ),
            });
        }
        Err(ToolError::Network("fetch redirect loop".to_string()))
    }
}

fn readable_content(content_type: &str, bytes: &[u8]) -> Result<(String, bool), ToolError> {
    let media_type = content_type.split(';').next().unwrap_or_default().trim();
    if matches!(media_type, "text/html" | "application/xhtml+xml") {
        let rendered = html2text::from_read(bytes, 120).map_err(|error| {
            ToolError::Execution(format!("failed to extract HTML text: {error}"))
        })?;
        return Ok((rendered, true));
    }
    Ok((String::from_utf8_lossy(bytes).into_owned(), false))
}

fn truncate_chars(content: String, max_chars: usize) -> (String, bool) {
    let Some((byte_index, _)) = content.char_indices().nth(max_chars) else {
        return (content, false);
    };
    let mut truncated = content[..byte_index].to_string();
    truncated.push_str("\n\n[Content truncated by PaperMachine]");
    (truncated, true)
}

fn error_chain(error: &dyn std::error::Error) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        let cause_message = cause.to_string();
        if !cause_message.is_empty() && !message.contains(&cause_message) {
            message.push_str(": ");
            message.push_str(&cause_message);
        }
        source = cause.source();
    }
    message
}

async fn validate_destination(url: &Url) -> Result<std::net::SocketAddr, ToolError> {
    if url.scheme() != "https" {
        return Err(ToolError::Network(
            "fetch_url only permits HTTPS URLs".to_string(),
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(ToolError::Network(
            "fetch URL must not contain credentials".to_string(),
        ));
    }
    if url.port().is_some_and(|port| port != 443) {
        return Err(ToolError::Network(
            "fetch URL must use the standard HTTPS port".to_string(),
        ));
    }
    let host = url
        .host()
        .ok_or_else(|| ToolError::Network("fetch URL has no host".to_string()))?;
    let addresses = match host {
        Host::Ipv4(address) => vec![IpAddr::V4(address)],
        Host::Ipv6(address) => vec![IpAddr::V6(address)],
        Host::Domain(domain) => tokio::net::lookup_host((domain, 443))
            .await
            .map_err(|error| ToolError::Network(error.to_string()))?
            .map(|address| address.ip())
            .collect::<Vec<_>>(),
    };
    if addresses.is_empty() || addresses.iter().any(|address| !is_public_ip(*address)) {
        return Err(ToolError::Network(
            "fetch destination resolved to a private or reserved address".to_string(),
        ));
    }
    Ok(std::net::SocketAddr::new(addresses[0], 443))
}

fn supported_content_type(value: &str) -> bool {
    let media_type = value.split(';').next().unwrap_or_default().trim();
    media_type.starts_with("text/")
        || matches!(
            media_type,
            "application/json"
                | "application/ld+json"
                | "application/xml"
                | "application/xhtml+xml"
                | "application/rss+xml"
                | "application/atom+xml"
        )
}

fn is_public_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_public_ipv4(address),
        IpAddr::V6(address) => is_public_ipv6(address),
    }
}

fn is_public_ipv4(address: Ipv4Addr) -> bool {
    let [a, b, c, _] = address.octets();
    !(address.is_unspecified()
        || address.is_loopback()
        || address.is_private()
        || address.is_link_local()
        || address.is_multicast()
        || address.is_broadcast()
        || address.is_documentation()
        || a == 0
        || (a == 100 && (64..=127).contains(&b))
        || (a == 192 && b == 0 && c == 0)
        || (a == 198 && (18..=19).contains(&b))
        || a >= 240)
}

fn is_public_ipv6(address: Ipv6Addr) -> bool {
    if let Some(address) = address.to_ipv4_mapped() {
        return is_public_ipv4(address);
    }
    let segments = address.segments();
    !(address.is_unspecified()
        || address.is_loopback()
        || address.is_multicast()
        || (segments[0] & 0xfe00) == 0xfc00
        || (segments[0] & 0xffc0) == 0xfe80
        || (segments[0] == 0x2001 && segments[1] == 0x0db8))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_and_reserved_addresses_are_rejected() {
        for address in [
            "127.0.0.1",
            "10.1.2.3",
            "169.254.1.1",
            "192.168.1.1",
            "100.64.0.1",
            "::1",
            "fc00::1",
            "fe80::1",
        ] {
            assert!(!is_public_ip(
                address.parse().expect("address should parse")
            ));
        }
        assert!(is_public_ip(
            "1.1.1.1".parse().expect("address should parse")
        ));
        assert!(is_public_ip(
            "2606:4700:4700::1111"
                .parse()
                .expect("address should parse")
        ));
    }

    #[test]
    fn html_is_rendered_without_script_or_style_noise() {
        let html = br#"<html><head><style>.hidden { color: red; }</style><script>secret()</script></head><body><main><h1>Evidence</h1><p>A supported claim.</p></main></body></html>"#;
        let (content, extracted) =
            readable_content("text/html; charset=utf-8", html).expect("HTML should render");
        assert!(extracted);
        assert!(content.contains("Evidence"));
        assert!(content.contains("A supported claim."));
        assert!(!content.contains("secret()"));
        assert!(!content.contains("color: red"));
    }

    #[test]
    fn returned_text_is_bounded_on_character_boundaries() {
        let (content, truncated) = truncate_chars("研究 evidence".to_string(), 3);
        assert!(truncated);
        assert!(content.starts_with("研究 "));
        assert!(content.contains("Content truncated"));
    }
}
