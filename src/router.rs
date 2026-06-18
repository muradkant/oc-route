use anyhow::{anyhow, Result};
use serde::Deserialize;
use serde_json::Value;

use crate::config::Profile;

#[derive(Clone, Debug, Deserialize)]
pub struct Decision {
    pub model: String,
    #[serde(default)]
    pub rationale: String,
}

pub fn build_routing_xml(profile: &Profile, history: &[Value], new_message: &str) -> String {
    let windowed = apply_sliding_window(history, profile.sliding_window);
    let conversation = render_conversation(&windowed);

    let model_pool = profile
        .model_pool
        .iter()
        .map(|m| format!(r#"  <model id="{}" />"#, xml_escape(m)))
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        r#"<routing_task>
You are a model router. Read the conversation and the new message,
then select the most appropriate model from the available pool.
Follow the user's routing rules exactly.
</routing_task>

<routing_rules>
{routing_rules}
</routing_rules>

<available_models>
{model_pool}
</available_models>

<conversation>
{conversation}
</conversation>

<new_message>
{new_message}
</new_message>

<output_format>
Respond with ONLY a JSON object containing:
- "model": the model ID from available_models (format: "providerID/modelID")
- "rationale": a one-line explanation of why this model was chosen
Do not include any other text, markdown fences, or commentary.
</output_format>
"#,
        routing_rules = xml_escape(profile.routing_prompt.trim()),
        model_pool = model_pool,
        conversation = conversation,
        new_message = xml_escape(new_message),
    )
}

fn apply_sliding_window(history: &[Value], window: usize) -> Vec<Value> {
    if history.len() <= window {
        history.to_vec()
    } else {
        history[history.len() - window..].to_vec()
    }
}

fn render_conversation(history: &[Value]) -> String {
    if history.is_empty() {
        return "  <!-- no prior conversation -->".to_string();
    }
    let mut out = Vec::with_capacity(history.len());
    for msg in history {
        if let Some(rendered) = render_message(msg) {
            out.push(rendered);
        }
    }
    out.join("\n")
}

fn render_message(msg: &Value) -> Option<String> {
    let info = msg.get("info")?;
    let role = info.get("role").and_then(|r| r.as_str()).unwrap_or("unknown");
    let model_attr = match info.get("model").and_then(|m| m.as_str()) {
        Some(m) => format!(r#" model="{}""#, xml_escape(m)),
        None => match (
            info.get("providerID").and_then(|v| v.as_str()),
            info.get("modelID").and_then(|v| v.as_str()),
        ) {
            (Some(p), Some(m)) => format!(r#" model="{}""#, xml_escape(&format!("{}/{}", p, m))),
            _ => String::new(),
        },
    };

    let parts = msg.get("parts").and_then(|p| p.as_array())?;
    let mut body = String::new();
    for part in parts {
        let ptype = part.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match ptype {
            "text" => {
                if let Some(t) = part.get("text").and_then(|t| t.as_str()) {
                    let t = t.trim();
                    if !t.is_empty() {
                        body.push_str(&format!("    <text>{}</text>\n", xml_escape(t)));
                    }
                }
            }
            "tool" => {
                let name = part.get("name").and_then(|n| n.as_str()).unwrap_or("tool");
                let summary = tool_summary(part);
                body.push_str(&format!(
                    "    <tool_call name=\"{}\"{}/>\n",
                    xml_escape(name),
                    summary
                        .as_deref()
                        .map(|s| format!(r#" summary="{}""#, xml_escape(s)))
                        .unwrap_or_default()
                ));
            }
            "reasoning" | "step-start" | "step-finish" | "error" => {}
            "file" => {
                let name = part
                    .get("filename")
                    .and_then(|n| n.as_str())
                    .or_else(|| part.get("url").and_then(|u| u.as_str()))
                    .unwrap_or("file");
                body.push_str(&format!("    <file name=\"{}\" />\n", xml_escape(name)));
            }
            "agent" => {
                let name = part.get("name").and_then(|n| n.as_str()).unwrap_or("agent");
                body.push_str(&format!("    <agent name=\"{}\" />\n", xml_escape(name)));
            }
            "subtask" => {
                let agent = part.get("agent").and_then(|a| a.as_str()).unwrap_or("general");
                body.push_str(&format!("    <subtask agent=\"{}\" />\n", xml_escape(agent)));
            }
            _ => {}
        }
    }

    if body.is_empty() {
        return None;
    }
    Some(format!(
        "  <message role=\"{}\"{}>\n{}</message>",
        xml_escape(role),
        model_attr,
        body
    ))
}

fn tool_summary(part: &Value) -> Option<String> {
    let input = part.get("input");
    let primary_path = input
        .and_then(|i| {
            i.get("path")
                .or_else(|| i.get("filePath"))
                .or_else(|| i.get("file"))
        })
        .and_then(|v| v.as_str());
    let command = input.and_then(|i| i.get("command")).and_then(|v| v.as_str());
    let query = input.and_then(|i| i.get("query")).and_then(|v| v.as_str());
    let pattern = input.and_then(|i| i.get("pattern")).and_then(|v| v.as_str());

    if let Some(p) = primary_path {
        let p = p.rsplit('/').next().unwrap_or(p);
        return Some(p.to_string());
    }
    if let Some(c) = command {
        let c = c.chars().take(60).collect::<String>();
        return Some(format!("$ {}", c));
    }
    if let Some(q) = query {
        let q = q.chars().take(60).collect::<String>();
        return Some(format!("query: {}", q));
    }
    if let Some(p) = pattern {
        let p = p.chars().take(60).collect::<String>();
        return Some(format!("pattern: {}", p));
    }
    None
}

pub fn parse_decision(raw: &str) -> Result<Decision> {
    let cleaned = strip_code_fences(raw);
    let cleaned = cleaned.trim();
    let json_str = extract_json_object(cleaned)
        .ok_or_else(|| anyhow!("router response did not contain a JSON object: {}", raw))?;
    let decision: Decision = serde_json::from_str(&json_str)
        .map_err(|e| anyhow!("failed to parse router JSON '{}': {}", json_str, e))?;
    Ok(decision)
}

fn strip_code_fences(s: &str) -> String {
    let t = s.trim();
    if let Some(after_open) = t.strip_prefix("```") {
        let without_lang = after_open
            .strip_prefix("json")
            .map(|s| s.trim_start())
            .unwrap_or_else(|| after_open.trim_start());
        if let Some(end) = without_lang.rfind("```") {
            return without_lang[..end].trim().to_string();
        }
        return without_lang.trim().to_string();
    }
    t.to_string()
}

fn extract_json_object(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let start = bytes.iter().position(|&b| b == b'{')?;
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escape = false;
    for (i, &b) in bytes[start..].iter().enumerate() {
        if in_string {
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(s[start..start + i + 1].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

pub fn validate_model(model_id: &str, pool: &[String]) -> Option<(String, String)> {
    let normalized = model_id.trim();
    if pool.iter().any(|m| m == normalized) {
        return crate::config::split_model_id(normalized);
    }
    let (p, m) = crate::config::split_model_id(normalized)?;
    if pool.iter().any(|entry| {
        crate::config::split_model_id(entry)
            .map(|(ep, em)| ep == p && em == m)
            .unwrap_or(false)
    }) {
        Some((p, m))
    } else {
        None
    }
}

fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_json() {
        let d = parse_decision(r#"{"model":"anthropic/claude-sonnet-4-5","rationale":"x"}"#).unwrap();
        assert_eq!(d.model, "anthropic/claude-sonnet-4-5");
        assert_eq!(d.rationale, "x");
    }

    #[test]
    fn parses_fenced_json() {
        let raw = "```json\n{\"model\":\"openai/gpt-4o\",\"rationale\":\"y\"}\n```";
        let d = parse_decision(raw).unwrap();
        assert_eq!(d.model, "openai/gpt-4o");
    }

    #[test]
    fn parses_json_with_surrounding_text() {
        let raw = "Here is my decision:\n{\"model\":\"a/b\",\"rationale\":\"z\"}\nThanks.";
        let d = parse_decision(raw).unwrap();
        assert_eq!(d.model, "a/b");
    }

    #[test]
    fn validate_model_accepts_pool_member() {
        let pool = vec!["anthropic/claude".to_string(), "openai/gpt-4o".to_string()];
        assert!(validate_model("openai/gpt-4o", &pool).is_some());
    }

    #[test]
    fn validate_model_rejects_non_member() {
        let pool = vec!["anthropic/claude".to_string()];
        assert!(validate_model("google/gemini", &pool).is_none());
    }
}
