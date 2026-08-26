use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccessibilityState {
    pub disabled: bool,
    pub checked: Option<bool>,
    pub expanded: Option<bool>,
    pub selected: Option<bool>,
    pub pressed: Option<bool>,
    pub readonly: bool,
    pub required: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccessibilityFacts {
    pub role: Option<String>,
    pub name: Option<String>,
    pub description: Option<String>,
    pub value: Option<String>,
    pub state: AccessibilityState,
    #[serde(default)]
    pub availability: AccessibilityAvailability,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable_reason: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum AccessibilityAvailability {
    Available,
    #[default]
    Unavailable,
    Truncated,
}

pub(super) fn facts_from_ax_node(
    node: Option<&cdpkit::accessibility::types::AXNode>,
    truncated: bool,
) -> AccessibilityFacts {
    let Some(node) = node.filter(|node| !node.ignored) else {
        return AccessibilityFacts {
            availability: if truncated {
                AccessibilityAvailability::Truncated
            } else {
                AccessibilityAvailability::Unavailable
            },
            unavailable_reason: Some(if truncated {
                "snapshot accessibility budget was exhausted".to_owned()
            } else {
                "Chrome returned no accessibility node".to_owned()
            }),
            ..AccessibilityFacts::default()
        };
    };
    let mut facts = AccessibilityFacts {
        role: string_value(node.role.as_ref()),
        name: string_value(node.name.as_ref()),
        description: string_value(node.description.as_ref()),
        value: string_value(node.value.as_ref()),
        availability: AccessibilityAvailability::Available,
        ..AccessibilityFacts::default()
    };
    for property in node.properties.as_deref().unwrap_or_default() {
        let value = bool_value(Some(&property.value));
        match property.name.as_ref() {
            "disabled" => facts.state.disabled = value.unwrap_or(false),
            "checked" => facts.state.checked = value,
            "expanded" => facts.state.expanded = value,
            "selected" => facts.state.selected = value,
            "pressed" => facts.state.pressed = value,
            "readonly" => facts.state.readonly = value.unwrap_or(false),
            "required" => facts.state.required = value.unwrap_or(false),
            _ => {}
        }
    }
    facts
}

fn string_value(value: Option<&cdpkit::accessibility::types::AXValue>) -> Option<String> {
    match value?.value.as_ref()? {
        serde_json::Value::String(value) => Some(value.clone()),
        serde_json::Value::Bool(value) => Some(value.to_string()),
        serde_json::Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

fn bool_value(value: Option<&cdpkit::accessibility::types::AXValue>) -> Option<bool> {
    match value?.value.as_ref()? {
        serde_json::Value::Bool(value) => Some(*value),
        serde_json::Value::String(value) if value == "true" || value == "checked" => Some(true),
        serde_json::Value::String(value) if value == "false" || value == "unchecked" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use cdpkit::accessibility::types::{AXNode, AXProperty, AXPropertyName, AXValue, AXValueType};
    use cdpkit::dom::types::BackendNodeId;
    use serde_json::json;

    use super::*;

    fn value(value: serde_json::Value) -> AXValue {
        AXValue {
            type_: AXValueType::String,
            value: Some(value),
            related_nodes: None,
            sources: None,
        }
    }

    #[test]
    fn chrome_computed_complex_name_role_and_state_are_preserved() {
        let node = AXNode {
            node_id: "ax-1".to_owned(),
            ignored: false,
            ignored_reasons: None,
            role: Some(value(json!("switch"))),
            chrome_role: None,
            name: Some(value(json!("Billing notifications"))),
            description: Some(value(json!("Sent to the account owner"))),
            value: None,
            properties: Some(vec![
                AXProperty {
                    name: AXPropertyName::Checked,
                    value: value(json!(true)),
                },
                AXProperty {
                    name: AXPropertyName::Disabled,
                    value: value(json!(false)),
                },
                AXProperty {
                    name: AXPropertyName::Required,
                    value: value(json!(true)),
                },
            ]),
            parent_id: None,
            child_ids: None,
            backend_dom_node_id: Some(42 as BackendNodeId),
            frame_id: None,
        };
        let facts = facts_from_ax_node(Some(&node), false);
        assert_eq!(facts.availability, AccessibilityAvailability::Available);
        assert_eq!(facts.role.as_deref(), Some("switch"));
        assert_eq!(facts.name.as_deref(), Some("Billing notifications"));
        assert_eq!(facts.state.checked, Some(true));
        assert!(facts.state.required);
    }

    #[test]
    fn missing_and_budget_truncated_ax_are_explicit_not_guessed() {
        assert_eq!(
            facts_from_ax_node(None, false).availability,
            AccessibilityAvailability::Unavailable
        );
        assert_eq!(
            facts_from_ax_node(None, true).availability,
            AccessibilityAvailability::Truncated
        );
        assert!(facts_from_ax_node(None, false).role.is_none());
    }
}
