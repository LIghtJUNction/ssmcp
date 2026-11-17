//! Tab-scoped input batches and host-side condition waits.
use super::{CLICK_SCRIPT, FILL_SCRIPT, Page, key_spec, modifier_bits, ref_action};
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;

pub(super) const BATCH_TIMEOUT: Duration = Duration::from_secs(25);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Act {
    pub target_id: String,
    pub actions: Vec<Action>,
    #[serde(default = "super::enabled")]
    pub snapshot: bool,
    #[serde(default = "super::default_elements")]
    pub max_elements: usize,
    #[serde(default = "super::default_chars")]
    pub max_chars: usize,
}

impl Act {
    pub fn validate(&self) -> Result<(), String> {
        super::valid_target(&self.target_id)?;
        if self.actions.is_empty() || self.actions.len() > 16 {
            return Err("act requires between 1 and 16 actions".into());
        }
        super::snapshot_bounds(self.max_elements, self.max_chars)?;
        // Validate the entire request before the first input, including later steps.
        for (index, action) in self.actions.iter().enumerate() {
            action
                .validate()
                .map_err(|error| format!("action {index}: {error}"))?;
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Action {
    Click {
        #[serde(rename = "ref")]
        element_ref: String,
        #[serde(default = "super::left_button")]
        button: String,
        #[serde(default = "super::one")]
        click_count: u32,
    },
    Fill {
        #[serde(rename = "ref")]
        element_ref: String,
        text: String,
    },
    Key {
        key: String,
        #[serde(default)]
        modifiers: Vec<String>,
    },
    Scroll {
        #[serde(default)]
        delta_x: i64,
        #[serde(default)]
        delta_y: i64,
    },
    Wait(WaitCondition),
}

impl Action {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Click { .. } => "click",
            Self::Fill { .. } => "fill",
            Self::Key { .. } => "key",
            Self::Scroll { .. } => "scroll",
            Self::Wait(_) => "wait",
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::Click {
                element_ref,
                button,
                click_count,
            } => {
                valid_ref(element_ref)?;
                if !["left", "right", "middle"].contains(&button.as_str())
                    || !(1..=3).contains(click_count)
                {
                    return Err("invalid mouse button or click_count".into());
                }
            }
            Self::Fill { element_ref, text } => {
                valid_ref(element_ref)?;
                if text.len() > 32768 {
                    return Err("fill text exceeds 32768 UTF-8 bytes".into());
                }
            }
            Self::Key { key, modifiers } => {
                key_spec(key)?;
                modifier_bits(modifiers)?;
            }
            Self::Scroll { delta_x, delta_y } => {
                if !(-100000..=100000).contains(delta_x) || !(-100000..=100000).contains(delta_y) {
                    return Err("scroll delta exceeds 100000 CSS pixels".into());
                }
            }
            Self::Wait(condition) => condition.validate()?,
        }
        Ok(())
    }
}

fn valid_ref(element_ref: &str) -> Result<(), String> {
    if element_ref.is_empty() || element_ref.len() > 80 {
        return Err("ref must be an observed snapshot reference of at most 80 bytes".into());
    }
    Ok(())
}

pub(super) async fn run(page: &mut Page, action: &Action) -> Result<Value, String> {
    action.validate()?;
    match action {
        Action::Click {
            element_ref,
            button,
            click_count,
        } => {
            let point = ref_action(page, element_ref, CLICK_SCRIPT, vec![]).await?;
            let x = point["x"].as_f64().ok_or("element has no x coordinate")?;
            let y = point["y"].as_f64().ok_or("element has no y coordinate")?;
            let buttons = match button.as_str() {
                "left" => 1,
                "right" => 2,
                _ => 4,
            };
            page.cdp
                .call(
                    "Input.dispatchMouseEvent",
                    json!({"type":"mouseMoved","x":x,"y":y}),
                )
                .await?;
            page.cdp.input_pair("Input.dispatchMouseEvent",
                json!({"type":"mousePressed","x":x,"y":y,"button":button,"buttons":buttons,"clickCount":click_count}),
                json!({"type":"mouseReleased","x":x,"y":y,"button":button,"buttons":0,"clickCount":click_count})
            ).await?;
            Ok(json!({"ref":element_ref,"clicked":true}))
        }
        Action::Fill { element_ref, text } => {
            let prepared = ref_action(page, element_ref, FILL_SCRIPT, vec![json!(text)]).await?;
            if prepared["insert_text"] == true {
                page.cdp
                    .call("Input.insertText", json!({"text":text}))
                    .await?;
            }
            Ok(json!({"ref":element_ref,"filled":true}))
        }
        Action::Key { key, modifiers } => {
            let (code, actual, text) = key_spec(key)?;
            let modifiers = modifier_bits(modifiers)?;
            let mut down = json!({"type":"keyDown","key":actual,"windowsVirtualKeyCode":code,
                "nativeVirtualKeyCode":code,"modifiers":modifiers});
            if modifiers & (1 | 2 | 4) == 0
                && let Some(text) = text
            {
                down["text"] = json!(text);
            }
            page.cdp
                .input_pair(
                    "Input.dispatchKeyEvent",
                    down,
                    json!({"type":"keyUp","key":actual,"windowsVirtualKeyCode":code,
                    "nativeVirtualKeyCode":code,"modifiers":modifiers}),
                )
                .await?;
            Ok(json!({"key":key,"pressed":true}))
        }
        Action::Scroll { delta_x, delta_y } => {
            let size = page
                .cdp
                .value("({width:innerWidth,height:innerHeight})")
                .await?;
            page.cdp
                .call(
                    "Input.dispatchMouseEvent",
                    json!({"type":"mouseWheel",
                "x":size["width"].as_f64().unwrap_or(800.0)/2.0,
                "y":size["height"].as_f64().unwrap_or(600.0)/2.0,
                "deltaX":delta_x,"deltaY":delta_y}),
                )
                .await?;
            Ok(json!({"scrolled":true}))
        }
        Action::Wait(condition) => wait(page, condition).await,
    }
}

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum WaitState {
    #[default]
    Visible,
    Hidden,
    Attached,
    Detached,
}

impl WaitState {
    fn name(self) -> &'static str {
        match self {
            Self::Visible => "visible",
            Self::Hidden => "hidden",
            Self::Attached => "attached",
            Self::Detached => "detached",
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct WaitCondition {
    selector: Option<String>,
    text: Option<String>,
    #[serde(default)]
    state: WaitState,
    #[serde(default = "default_wait")]
    pub timeout_ms: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct WaitArgs {
    pub target_id: String,
    selector: Option<String>,
    text: Option<String>,
    #[serde(default)]
    state: WaitState,
    #[serde(default = "default_wait")]
    timeout_ms: u64,
}

impl WaitArgs {
    pub fn into_condition(self) -> WaitCondition {
        WaitCondition {
            selector: self.selector,
            text: self.text,
            state: self.state,
            timeout_ms: self.timeout_ms,
        }
    }
}

fn default_wait() -> u64 {
    5000
}

impl WaitCondition {
    pub fn validate(&self) -> Result<(), String> {
        if self.selector.is_none() && self.text.is_none() {
            return Err("wait requires a selector and/or literal text".into());
        }
        for value in [&self.selector, &self.text].into_iter().flatten() {
            if value.is_empty() || value.len() > 4096 {
                return Err("wait selector and text must contain 1..4096 UTF-8 bytes".into());
            }
        }
        if !(1..=10000).contains(&self.timeout_ms) {
            return Err("wait timeout_ms must be between 1 and 10000".into());
        }
        Ok(())
    }

    fn expression(&self) -> String {
        // Values are JSON-encoded; selectors/text are data, never JavaScript source.
        format!(
            r#"(() => {{
            const selector={}, text={}, state={};
            const e=selector===null ? document.body : document.querySelector(selector);
            const matched=!!e && (text===null || (e.innerText ?? e.textContent ?? '').includes(text));
            const style=e ? getComputedStyle(e) : null;
            const visible=matched && [...e.getClientRects()].some(r=>r.width>0 && r.height>0) && style.visibility==='visible' && style.display!=='none';
            return state==='attached' ? matched : state==='detached' ? !matched : state==='hidden' ? !visible : !!visible;
        }})()"#,
            json!(self.selector),
            json!(self.text),
            json!(self.state.name())
        )
    }
}

pub(super) async fn wait(page: &mut Page, condition: &WaitCondition) -> Result<Value, String> {
    condition.validate()?;
    let started = tokio::time::Instant::now();
    let expression = condition.expression();
    tokio::time::timeout(Duration::from_millis(condition.timeout_ms), async {
        loop {
            match page.cdp.value(&expression).await {
                Ok(Value::Bool(true)) => {
                    return Ok(json!({"matched":true,
                    "state":condition.state.name(),"elapsed_ms":started.elapsed().as_millis()}));
                }
                Ok(_) => {}
                // A navigation destroys the old context while the next document commits.
                Err(error)
                    if error.contains("Cannot find context")
                        || error.contains("Cannot find default execution context")
                        || error.contains("Execution context was destroyed") => {}
                Err(error) => return Err(error),
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .map_err(|_| {
        format!(
            "browser condition was not {} within {} ms",
            condition.state.name(),
            condition.timeout_ms
        )
    })?
}

pub(super) fn wait_schema() -> Value {
    json!({
        "selector":{"type":"string","minLength":1,"maxLength":4096},
        "text":{"type":"string","minLength":1,"maxLength":4096},
        "state":{"type":"string","enum":["visible","hidden","attached","detached"],"default":"visible"},
        "timeout_ms":{"type":"integer","minimum":1,"maximum":10000,"default":5000}
    })
}

pub(super) fn act_schema() -> Value {
    let step = |kind: &str, properties: Value, required: &[&str]| {
        let mut properties = properties;
        properties["type"] = json!({"const":kind});
        let required: Vec<_> = std::iter::once("type")
            .chain(required.iter().copied())
            .collect();
        json!({"type":"object","properties":properties,"required":required,"additionalProperties":false})
    };
    let element_ref = json!({"type":"string","minLength":1,"maxLength":80});
    json!({
        "actions":{"type":"array","minItems":1,"maxItems":16,"items":{"oneOf":[
            step("fill",json!({"ref":element_ref,"text":{"type":"string","maxLength":32768}}), &["ref","text"]),
            step("click",json!({"ref":element_ref,"button":{"type":"string","enum":["left","right","middle"],"default":"left"},"click_count":{"type":"integer","minimum":1,"maximum":3,"default":1}}), &["ref"]),
            step("key",json!({"key":{"type":"string","minLength":1,"maxLength":32},"modifiers":{"type":"array","maxItems":4,"uniqueItems":true,"items":{"type":"string","enum":["Alt","Control","Meta","Shift"]}}}), &["key"]),
            step("scroll",json!({"delta_x":{"type":"integer","minimum":-100000,"maximum":100000},"delta_y":{"type":"integer","minimum":-100000,"maximum":100000}}), &[]),
            step("wait",wait_schema(), &[])
        ]}},
        "snapshot":{"type":"boolean","default":true},
        "max_elements":{"type":"integer","minimum":1,"maximum":200,"default":100},
        "max_chars":{"type":"integer","minimum":1,"maximum":32768,"default":12000}
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batches_validate_later_steps_before_any_input() {
        let args: Act = serde_json::from_value(json!({"target_id":"t","actions":[
            {"type":"key","key":"Enter"},{"type":"scroll","delta_y":100001}
        ]}))
        .unwrap();
        assert!(args.validate().unwrap_err().contains("action 1"));
        for actions in [
            json!([]),
            json!([{"type":"wait"}]),
            json!([{"type":"fill","ref":"s1e1","text":"x","extra":true}]),
        ] {
            let parsed = serde_json::from_value::<Act>(json!({"target_id":"t","actions":actions}));
            assert!(parsed.is_err() || parsed.unwrap().validate().is_err());
        }
    }

    #[test]
    fn wait_literals_remain_encoded_and_empty_conditions_are_rejected() {
        let condition: WaitCondition =
            serde_json::from_value(json!({"text":"\";globalThis.attack=1;//","timeout_ms":10}))
                .unwrap();
        condition.validate().unwrap();
        assert!(
            condition
                .expression()
                .contains(&json!("\";globalThis.attack=1;//").to_string())
        );
        let action: Action =
            serde_json::from_value(json!({"type":"wait","selector":"#ready","state":"attached"}))
                .unwrap();
        action.validate().unwrap();
        assert!(
            serde_json::from_value::<Action>(
                json!({"type":"wait","selector":"#ready","unknown":true})
            )
            .is_err()
        );
    }
}
