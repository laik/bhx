//! Observation engine: turn a live page into the element index a model can read.
//!
//! Cost profile (measured): the AX tree call is ~0.5 ms on a normal page but the
//! serialized tree for a 1000-row table is 4.9 MB / 13k nodes. Hence the hard
//! element budget and the "interactive only, visible only" filter.

use bh_browser::Session;
use bh_core::{BhError, ElementInfo, ElementState, Observation, Rect, Result, Viewport, STATE_FORMAT_VERSION};
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct ObserveOptions {
    pub max_elements: usize,
}

impl Default for ObserveOptions {
    fn default() -> Self {
        Self { max_elements: 120 }
    }
}

pub struct Observer {
    seq: u64,
    opts: ObserveOptions,
    last: Option<Observation>,
}

impl Observer {
    pub fn new(opts: ObserveOptions) -> Self {
        Self { seq: 0, opts, last: None }
    }

    pub fn last(&self) -> Option<&Observation> {
        self.last.as_ref()
    }

    /// Adjust the element budget for subsequent observations.
    pub fn set_max_elements(&mut self, max: usize) {
        self.opts.max_elements = max.max(1);
    }

    pub async fn observe(&mut self, session: &Session) -> Result<Observation> {
        self.seq += 1;
        let obs = build(session, &self.opts, self.seq).await?;
        self.last = Some(obs.clone());
        Ok(obs)
    }
}

pub async fn build(session: &Session, opts: &ObserveOptions, seq: u64) -> Result<Observation> {
    let tree = session
        .call("Accessibility.getFullAXTree", serde_json::json!({}))
        .await
        .map_err(|e| BhError::Other(format!("ax tree: {e}")))?;

    let nodes = tree["nodes"].as_array().cloned().unwrap_or_default();
    let mut elements = Vec::new();
    let mut truncated = false;

    for node in &nodes {
        if node["ignored"].as_bool().unwrap_or(false) {
            continue;
        }
        let Some(backend_id) = node["backendDOMNodeId"].as_i64() else {
            continue;
        };
        let role = node["role"]["value"].as_str().unwrap_or("").to_string();
        if !is_interactive(&role) {
            continue;
        }

        if elements.len() >= opts.max_elements {
            truncated = true;
            break;
        }

        let name = node["name"]["value"].as_str().unwrap_or("").trim().to_string();
        let mut value = node["value"]["value"].as_str().map(str::to_string);
        // Invariant: a password field's value never leaves the browser. Chrome
        // usually suppresses it in the AX tree already; a "show password"
        // toggle makes it a plain textbox, so check the DOM for the ones that
        // actually carry content.
        if value.as_deref().is_some_and(|v| !v.is_empty())
            && is_secret_field(session, backend_id).await
        {
            value = Some("***".into());
        }
        let Some(bbox) = session.box_model(backend_id).await? else {
            continue; // no layout box: not on screen
        };
        if bbox.w <= 0.0 || bbox.h <= 0.0 {
            continue;
        }

        let states = read_states(node);
        let eid = format!("e{}", elements.len() + 1);
        let hash = hash_of(&role, &name, value.as_deref(), &bbox);
        elements.push(ElementInfo {
            eid,
            role,
            name,
            value,
            bbox,
            states,
            hash,
            backend_node_id: backend_id,
            frame_id: "main".into(),
        });
    }

    let url = session.url().await?;
    let title = session.title().await?;
    let viewport: Viewport = session.viewport().await?;

    Ok(Observation {
        id: format!("obs_{seq}"),
        seq,
        state_format_version: STATE_FORMAT_VERSION,
        url,
        title,
        viewport,
        elements,
        truncated,
    })
}

/// True for password-like inputs. One extra round-trip, and only for fields
/// that already have a value worth leaking.
async fn is_secret_field(session: &Session, backend_node_id: i64) -> bool {
    let Ok(node) = session
        .call(
            "DOM.describeNode",
            serde_json::json!({ "backendNodeId": backend_node_id }),
        )
        .await
    else {
        return false;
    };
    let Some(attrs) = node["node"]["attributes"].as_array() else {
        return false;
    };
    let mut it = attrs.iter().filter_map(|a| a.as_str());
    while let (Some(k), Some(v)) = (it.next(), it.next()) {
        match k.to_ascii_lowercase().as_str() {
            "type" if v.eq_ignore_ascii_case("password") => return true,
            "autocomplete" if v.to_ascii_lowercase().contains("password") => return true,
            _ => {}
        }
    }
    false
}

/// Roles worth showing a decision model. Deliberately excludes structural
/// containers: they carry no actionable choice and inflate the state text.
pub fn is_interactive(role: &str) -> bool {
    matches!(
        role,
        "button"
            | "link"
            | "textbox"
            | "searchbox"
            | "combobox"
            | "checkbox"
            | "radio"
            | "tab"
            | "menuitem"
            | "menuitemcheckbox"
            | "menuitemradio"
            | "slider"
            | "spinbutton"
            | "switch"
            | "listbox"
            | "option"
            | "treeitem"
    )
}

fn read_states(node: &Value) -> Vec<ElementState> {
    let mut states = vec![ElementState::Enabled, ElementState::Visible];
    let Some(props) = node["properties"].as_array() else {
        return states;
    };
    for p in props {
        let name = p["name"].as_str().unwrap_or("");
        let v = &p["value"]["value"];
        match name {
            "disabled" if v.as_bool() == Some(true) => {
                states.retain(|s| *s != ElementState::Enabled);
                states.push(ElementState::Disabled);
            }
            "checked" if v.as_bool() == Some(true) || v.as_str() == Some("true") => {
                states.push(ElementState::Checked);
            }
            "focused" if v.as_bool() == Some(true) => {
                states.push(ElementState::Focused);
            }
            "hidden" if v.as_bool() == Some(true) => {
                states.retain(|s| *s != ElementState::Visible);
                states.push(ElementState::Hidden);
            }
            _ => {}
        }
    }
    states
}

fn hash_of(role: &str, name: &str, value: Option<&str>, bbox: &Rect) -> u64 {
    // FNV-1a; stability matters more than distribution quality.
    let mut h: u64 = 0xcbf29ce484222325;
    let mut feed = |bytes: &[u8]| {
        for b in bytes {
            h ^= *b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
    };
    feed(role.as_bytes());
    feed(b"\x1f");
    feed(name.as_bytes());
    feed(b"\x1f");
    feed(value.unwrap_or("").as_bytes());
    feed(b"\x1f");
    feed(&(bbox.x as i64).to_le_bytes());
    feed(&(bbox.y as i64).to_le_bytes());
    feed(&(bbox.w as i64).to_le_bytes());
    feed(&(bbox.h as i64).to_le_bytes());
    h
}
