//! OpenAI-compatible adapter (`/v1/chat/completions`).
//!
//! One wire format covers most local servers (llama.cpp, vLLM, Ollama's
//! compatibility endpoint) and hosted APIs, so the harness does not need a
//! provider-specific dependency to have a vision path. Configuration:
//!
//! - `BH_VLM_URL`   full endpoint, e.g. `http://127.0.0.1:8790/v1/chat/completions`
//! - `BH_VLM_MODEL` model name sent in the request (default `vlm`)
//! - `BH_VLM_KEY`   optional bearer token
//! - `BH_VLM_TIMEOUT_MS` optional request timeout (default 8000)
//!
//! Planning uses `BH_LLM_*` when set and otherwise falls back to the `BH_VLM_*`
//! endpoint, since most OpenAI-compatible servers accept both text and images.

use std::time::Duration;

use async_trait::async_trait;
use bh_core::{BhError, Result, Step};
use base64::Engine as _;
use serde_json::{Value, json};

use crate::{Capability, ModelAdapter, ModelProposal, ModelRequest};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Text + images: locate a target in a screenshot, rank DOM candidates.
    Vision,
    /// Text only: turn a goal into steps.
    Text,
}

pub struct OpenAiCompat {
    url: String,
    model: String,
    key: Option<String>,
    timeout: Duration,
    kind: Kind,
}

impl OpenAiCompat {
    /// One adapter per configured endpoint. No endpoint means no adapters,
    /// which `ModelRouter` reports as "not configured" rather than guessing.
    pub fn from_env() -> Result<Vec<Self>> {
        let mut out = Vec::new();
        if let Some(a) = Self::vision_from_env()? {
            out.push(a);
        }
        if let Some(a) = Self::text_from_env()? {
            out.push(a);
        }
        Ok(out)
    }

    fn vision_from_env() -> Result<Option<Self>> {
        let Some(url) = std::env::var("BH_VLM_URL").ok().filter(|v| !v.trim().is_empty()) else {
            return Ok(None);
        };
        let model = std::env::var("BH_VLM_MODEL").unwrap_or_else(|_| "vlm".into());
        let key = std::env::var("BH_VLM_KEY").ok().filter(|k| !k.is_empty());
        let timeout = timeout_env("BH_VLM_TIMEOUT_MS", 8_000);
        Ok(Some(Self { url, model, key, timeout, kind: Kind::Vision }))
    }

    fn text_from_env() -> Result<Option<Self>> {
        let url = std::env::var("BH_LLM_URL")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .or_else(|| std::env::var("BH_VLM_URL").ok().filter(|v| !v.trim().is_empty()));
        let Some(url) = url else {
            return Ok(None);
        };
        let model = std::env::var("BH_LLM_MODEL")
            .ok()
            .or_else(|| std::env::var("BH_VLM_MODEL").ok())
            .unwrap_or_else(|| "llm".into());
        let key = std::env::var("BH_LLM_KEY")
            .ok()
            .filter(|k| !k.is_empty())
            .or_else(|| std::env::var("BH_VLM_KEY").ok().filter(|k| !k.is_empty()));
        let timeout = timeout_env("BH_LLM_TIMEOUT_MS", 30_000);
        Ok(Some(Self { url, model, key, timeout, kind: Kind::Text }))
    }

    fn body(&self, req: &ModelRequest) -> Value {
        let mut user = String::new();
        match req.capability {
            Capability::Plan => {
                user.push_str("Plan a browser workflow for this goal.\n");
                user.push_str(&format!("Goal: {}\n", req.instructions));
            }
            Capability::Verify => {
                user.push_str("Judge whether this goal is achieved in the current page state.\n");
                user.push_str(&format!("Goal: {}\n", req.instructions));
            }
            Capability::Repair => {
                user.push_str("Repair a browser step that already failed.\n");
                user.push_str(&format!("Task goal: {}\n", req.instructions));
            }
            Capability::VisualLocate => {
                user.push_str("Locate this target in the screenshot.\n");
                user.push_str(&format!("Target: {}\n", req.instructions));
            }
            Capability::Locate => {
                user.push_str("Choose the control that best matches the task.\n");
                user.push_str(&format!("Task: {}\n", req.instructions));
                user.push_str("Candidates (in priority order):\n");
                for (id, description) in &req.criteria {
                    user.push_str(&format!("- {id}: {description}\n"));
                }
            }
        }
        if let Some(state) = &req.state {
            if !state.is_empty() {
                user.push_str(&format!("Context: {state}\n"));
            }
        }
        match req.capability {
            Capability::Plan => {
                user.push_str(
                    "Reply with one JSON object: {\"steps\":[...],\"confidence\":0.0-1.0,\
                     \"reason\":\"...\"}.\n\
                     Each step has: id (string), goal (string), actions (array), timeout_ms \
                     (1..600000), and optionally success (one condition), side_effect \
                     (read_only|reversible|irreversible), requires_approval (bool).\n\
                     Actions: navigate{url}; click{target,button?}; hover{target}; \
                     type{target,text,clear_first?}; press{key,modifiers?}; scroll{delta:{x,y}} \
                     or scroll{target}; drag{from,to}; wait_for{condition,timeout_ms?}; \
                     screenshot{name?}; extract{name,script}.\n\
                     Targets: {\"role\":\"button\",\"name\":\"...\"}, {\"label\":\"...\"}, \
                     {\"placeholder\":\"...\"}, {\"text\":\"...\"}, {\"css\":\"...\"}, \
                     {\"test_id\":\"...\"}, or {\"visual\":\"description\"}.\n\
                     Conditions: {\"type\":\"element_visible\",\"locator\":{...}}, \
                     {\"type\":\"element_count\",\"locator\":{...},\"op\":\"eq\",\"value\":2}, \
                     {\"type\":\"url_matches\",\"pattern\":\"**/ok\"}, \
                     {\"type\":\"js\",\"script\":\"...\"}, \
                     {\"type\":\"download_completed\",\"filename_regex\":\"...\",\
                     \"min_bytes\":N}.\n\
                     Prefer role+name locators over CSS. Do not invent element ids. Keep each \
                     step's success condition checkable from the page state.",
                );
            }
            Capability::Verify => {
                user.push_str("Choices (pick exactly one id):\n");
                for (id, description) in &req.criteria {
                    user.push_str(&format!("- {id}: {description}\n"));
                }
                if req.image.is_some() {
                    user.push_str(
                        "A screenshot of the current page is attached. Judge the state and \
                         the image together, and say so in the reason if they disagree.\n",
                    );
                }
                user.push_str(
                    "Reply with one JSON object: {\"choice\":\"<id>\",\
                     \"confidence\":0.0-1.0,\"reason\":\"...\"}. Judge only what the \
                     state shows; do not assume an action worked because it was attempted.",
                );
            }
            Capability::Repair => {
                user.push_str(
                    "Reply with one JSON object: {\"steps\":[<one replacement step>],\
                     \"confidence\":0.0-1.0,\"reason\":\"...\"}. The replacement must keep \
                     the failed step's id, executor, preconditions, success, fail_when, \
                     fallback_visual and side_effect exactly as given, and must not remove \
                     requires_approval. Fix only the actions. Do not add evaluate, extract, \
                     or a wait_for with a JS condition unless the failed step already \
                     contains that exact action. Do not navigate to a different host. Use \
                     only controls visible in the current page. Resume the workflow, do not \
                     restart it from login.",
                );
            }
            Capability::VisualLocate => {
                let (w, h) = req
                    .image
                    .as_ref()
                    .map(|i| (i.width, i.height))
                    .unwrap_or((0, 0));
                user.push_str(&format!(
                    "The image is {w}x{h} pixels. Reply with one JSON object: \
                     {{\"bbox_norm\":[x,y,w,h],\"confidence\":0.0-1.0,\"reason\":\"...\"}}. \
                     The box must be normalized to the image (0..1) and tightly enclose the target."
                ));
            }
            Capability::Locate => {
                user.push_str(
                    "Reply with one JSON object: {\"choice\":\"<candidate id>\",\
                     \"confidence\":0.0-1.0,\"reason\":\"...\"}.",
                );
            }
        }

        let mut content = vec![json!({ "type": "text", "text": user })];
        if let Some(img) = &req.image {
            let b64 = base64::engine::general_purpose::STANDARD.encode(&img.bytes);
            content.push(json!({
                "type": "image_url",
                "image_url": { "url": format!("data:{};base64,{}", img.media_type, b64) }
            }));
        }

        let max_tokens =
            if matches!(req.capability, Capability::Plan | Capability::Repair) { 1500 } else { 300 };
        json!({
            "model": self.model,
            "temperature": 0,
            "max_tokens": max_tokens,
            "response_format": { "type": "json_object" },
            "messages": [
                {
                    "role": "system",
                    "content": "You are a UI grounding model inside a browser harness. \
                                Reply with one JSON object and nothing else."
                },
                { "role": "user", "content": content }
            ]
        })
    }
}

#[async_trait]
impl ModelAdapter for OpenAiCompat {
    fn name(&self) -> &str {
        &self.model
    }

    fn supports(&self, capability: Capability) -> bool {
        match self.kind {
            Kind::Vision => matches!(capability, Capability::Locate | Capability::VisualLocate),
            Kind::Text => matches!(
                capability,
                Capability::Plan | Capability::Verify | Capability::Repair
            ),
        }
    }

    async fn propose(&self, req: &ModelRequest) -> Result<ModelProposal> {
        let body = self.body(req);
        let url = self.url.clone();
        let key = self.key.clone();
        let timeout = self.timeout;
        tokio::task::spawn_blocking(move || call(&url, key.as_deref(), timeout, &body))
            .await
            .map_err(|e| BhError::Other(format!("model task panicked: {e}")))?
    }
}

fn call(url: &str, key: Option<&str>, timeout: Duration, body: &Value) -> Result<ModelProposal> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .build()
        .into();
    let mut request = agent
        .post(url)
        .header("content-type", "application/json")
        .header("accept", "application/json");
    if let Some(k) = key {
        request = request.header("authorization", &format!("Bearer {k}"));
    }

    let mut response = request
        .send(body.to_string().as_bytes())
        .map_err(|e| BhError::Other(format!("model request failed: {e}")))?;
    let text = response
        .body_mut()
        .read_to_string()
        .map_err(|e| BhError::Other(format!("model response body: {e}")))?;
    let envelope: Value = serde_json::from_str(&text)
        .map_err(|e| BhError::Other(format!("model response is not JSON: {e}")))?;

    let content = envelope["choices"][0]["message"]["content"]
        .as_str()
        .or_else(|| {
            envelope["choices"][0]["message"]["content"]
                .as_array()
                .and_then(|parts| parts.iter().find_map(|p| p["text"].as_str()))
        })
        .ok_or_else(|| BhError::Other("model response has no message content".into()))?;

    parse_proposal(content)
}

fn parse_proposal(content: &str) -> Result<ModelProposal> {
    // Models fence JSON even when told not to; take the outermost object.
    let start = content.find('{');
    let end = content.rfind('}');
    let json = match (start, end) {
        (Some(s), Some(e)) if e > s => &content[s..=e],
        _ => {
            return Err(BhError::Other(format!(
                "model content is not a JSON object: {}",
                clip(content)
            )));
        }
    };
    let v: Value = serde_json::from_str(json)
        .map_err(|e| BhError::Other(format!("model JSON invalid: {e}: {}", clip(content))))?;
    let mut p = ModelProposal {
        choice: v["choice"].as_str().map(str::to_string),
        confidence: v["confidence"].as_f64().unwrap_or(0.0),
        reason: v["reason"].as_str().map(str::to_string),
        model: v["model"].as_str().unwrap_or_default().to_string(),
        ..Default::default()
    };
    if let Some(a) = v["bbox_norm"].as_array() {
        if a.len() == 4 {
            let mut b = [0.0; 4];
            for (i, slot) in b.iter_mut().enumerate() {
                *slot = a[i]
                    .as_f64()
                    .ok_or_else(|| BhError::Other("bbox_norm has a non-number".into()))?;
            }
            p.bbox_norm = Some(b);
        } else {
            return Err(BhError::Other("bbox_norm must have four numbers".into()));
        }
    }
    if let Some(raw) = v.get("steps") {
        // Parse strictly here: a step the router cannot deserialize is a
        // proposal error, not something to silently drop and run short.
        let steps: Vec<Step> = serde_json::from_value(raw.clone())
            .map_err(|e| BhError::Other(format!("plan steps invalid: {e}: {}", clip(content))))?;
        p.steps = Some(steps);
    }
    Ok(p)
}

fn timeout_env(name: &str, default_ms: u64) -> Duration {
    let ms = std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(default_ms);
    Duration::from_millis(ms)
}

fn clip(s: &str) -> String {
    s.chars().take(200).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fenced_json() {
        let p = parse_proposal("```json\n{\"bbox_norm\":[0.1,0.2,0.3,0.4],\"confidence\":0.8}\n```")
            .unwrap();
        assert_eq!(p.bbox_norm, Some([0.1, 0.2, 0.3, 0.4]));
        assert!((p.confidence - 0.8).abs() < 1e-9);
    }

    #[test]
    fn rejects_a_non_object() {
        assert!(parse_proposal("I cannot find it.").is_err());
    }

    #[test]
    fn request_carries_the_image_as_a_data_uri() {
        let a = OpenAiCompat {
            url: "http://x".into(),
            model: "m".into(),
            key: None,
            timeout: Duration::from_secs(1),
            kind: Kind::Vision,
        };
        let mut req = ModelRequest::visual("the red button", "click");
        req.image = Some(crate::ImageData {
            media_type: "image/png",
            bytes: vec![1, 2, 3],
            width: 10,
            height: 10,
        });
        let body = a.body(&req);
        let url = body["messages"][1]["content"][1]["image_url"]["url"]
            .as_str()
            .unwrap();
        assert!(url.starts_with("data:image/png;base64,"));
    }

    #[test]
    fn parses_plan_steps() {
        let p = parse_proposal(
            r#"{"confidence":0.7,"steps":[
                {"id":"s1","goal":"open the list","actions":[{"type":"navigate","url":"http://x"}],"timeout_ms":5000},
                {"id":"s2","goal":"export","actions":[{"type":"click","target":{"text":"Export"}}],"timeout_ms":5000}
            ]}"#,
        )
        .unwrap();
        let steps = p.steps.unwrap();
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[1].id, "s2");
        assert_eq!(steps[1].actions.len(), 1);
    }

    #[test]
    fn a_non_array_steps_field_is_an_error() {
        assert!(parse_proposal(r#"{"steps":"do it"}"#).is_err());
    }

    #[test]
    fn adapters_support_only_their_own_capabilities() {
        let mk = |kind| OpenAiCompat {
            url: "http://x".into(),
            model: "m".into(),
            key: None,
            timeout: Duration::from_secs(1),
            kind,
        };
        let vision = mk(Kind::Vision);
        assert!(vision.supports(Capability::VisualLocate));
        assert!(vision.supports(Capability::Locate));
        assert!(!vision.supports(Capability::Plan));

        let text = mk(Kind::Text);
        assert!(text.supports(Capability::Plan));
        assert!(text.supports(Capability::Verify));
        assert!(!text.supports(Capability::VisualLocate));
    }

    #[test]
    fn a_plan_body_asks_for_steps_and_has_no_image() {
        let a = OpenAiCompat {
            url: "http://x".into(),
            model: "m".into(),
            key: None,
            timeout: Duration::from_secs(1),
            kind: Kind::Text,
        };
        let body = a.body(&ModelRequest::plan("export the orders", "url: http://x"));
        let text = body["messages"][1]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("\"steps\""));
        assert!(text.contains("export the orders"));
        assert!(text.contains("download_completed"));
        // The prompt has to name the wire format, not a convenient shorthand:
        // `key{...}` / `wait{ms}` would parse as unknown variants and be rejected.
        assert!(text.contains("press{key"));
        assert!(text.contains("wait_for{condition"));
        assert!(text.contains("element_count"));
        assert!(!text.contains("key{key}"));
        assert!(!text.contains("wait{ms}"));
        assert_eq!(body["messages"][1]["content"].as_array().unwrap().len(), 1);
        assert_eq!(body["max_tokens"], 1500);
    }
}
