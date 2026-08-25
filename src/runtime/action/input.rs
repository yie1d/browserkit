use cdpkit::input::methods::DispatchKeyEvent;
use cdpkit::input::types::DispatchKeyEventType;

use crate::runtime::{ActionCompletion, BrowserError, OperationPhase};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct KeyChord {
    pub(super) key: String,
    pub(super) code: String,
    pub(super) text: Option<String>,
    pub(super) modifiers: i64,
    virtual_key: i64,
}

pub(super) fn parse_key(value: &str) -> Result<KeyChord, BrowserError> {
    let parts = value.split('+').collect::<Vec<_>>();
    if parts.is_empty() || parts.iter().any(|part| part.is_empty()) {
        return Err(invalid_key(value));
    }
    let (key, modifiers) = parts.split_last().expect("checked non-empty key chord");
    let mut mask = 0;
    for modifier in modifiers {
        mask |= match *modifier {
            "Alt" => 1,
            "Control" | "Ctrl" => 2,
            "Meta" | "Command" => 4,
            "Shift" => 8,
            _ => return Err(invalid_key(value)),
        };
    }
    let (code, mut text, virtual_key) = named_key(key)
        .or_else(|| {
            let mut chars = key.chars();
            let character = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            let upper = character.to_ascii_uppercase();
            let code = if character.is_ascii_alphabetic() {
                format!("Key{upper}")
            } else if character.is_ascii_digit() {
                format!("Digit{character}")
            } else {
                String::new()
            };
            Some((code, Some(character.to_string()), upper as i64))
        })
        .ok_or_else(|| invalid_key(value))?;
    if mask & (1 | 2 | 4) != 0 {
        text = None;
    }

    Ok(KeyChord {
        key: (*key).to_owned(),
        code,
        text,
        modifiers: mask,
        virtual_key,
    })
}

fn named_key(key: &str) -> Option<(String, Option<String>, i64)> {
    let virtual_key = match key {
        "Backspace" => 8,
        "Tab" => 9,
        "Enter" => 13,
        "Escape" => 27,
        "Space" => 32,
        "PageUp" => 33,
        "PageDown" => 34,
        "End" => 35,
        "Home" => 36,
        "ArrowLeft" => 37,
        "ArrowUp" => 38,
        "ArrowRight" => 39,
        "ArrowDown" => 40,
        "Delete" => 46,
        _ => return None,
    };
    let code = if key == "Space" { "Space" } else { key };
    let text = (key == "Space").then(|| " ".to_owned());
    Some((code.to_owned(), text, virtual_key))
}

fn invalid_key(value: &str) -> BrowserError {
    BrowserError::operation("press key", OperationPhase::Preparation)
        .with_message(format!("unsupported key or malformed key chord: {value:?}"))
}

pub(super) async fn press(session: &cdpkit::Session, value: &str) -> Result<(), BrowserError> {
    let chord = parse_key(value)?;
    dispatch(
        session,
        DispatchKeyEventType::KeyDown,
        &chord,
        chord.text.as_deref(),
    )
    .await?;
    dispatch(session, DispatchKeyEventType::KeyUp, &chord, None).await
}

pub(super) async fn type_text(session: &cdpkit::Session, value: &str) -> Result<(), BrowserError> {
    for character in value.chars() {
        let chord = parse_key(&character.to_string())?;
        dispatch(
            session,
            DispatchKeyEventType::KeyDown,
            &chord,
            chord.text.as_deref(),
        )
        .await?;
        dispatch(session, DispatchKeyEventType::KeyUp, &chord, None).await?;
    }
    Ok(())
}

async fn dispatch(
    session: &cdpkit::Session,
    event_type: DispatchKeyEventType,
    chord: &KeyChord,
    text: Option<&str>,
) -> Result<(), BrowserError> {
    let mut command = DispatchKeyEvent::new(event_type)
        .with_key(chord.key.clone())
        .with_code(chord.code.clone())
        .with_modifiers(chord.modifiers)
        .with_windows_virtual_key_code(chord.virtual_key)
        .with_native_virtual_key_code(chord.virtual_key);
    if let Some(text) = text {
        command = command
            .with_text(text.to_owned())
            .with_unmodified_text(text.to_owned());
    }
    command.send(session).await.map_err(|error| {
        BrowserError::cdp_operation("dispatch key event", OperationPhase::Dispatch, error)
            .with_action_completion(ActionCompletion::Unknown)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_chords_parse_modifiers_and_named_keys() {
        let chord = parse_key("Control+Shift+A").unwrap();
        assert_eq!(chord.key, "A");
        assert_eq!(chord.modifiers, 2 | 8);
        assert_eq!(
            chord.text, None,
            "accelerators must not insert printable text"
        );

        let shifted = parse_key("Shift+A").unwrap();
        assert_eq!(shifted.text.as_deref(), Some("A"));

        let enter = parse_key("Enter").unwrap();
        assert_eq!(enter.key, "Enter");
        assert_eq!(enter.code, "Enter");
        assert_eq!(enter.text, None);
    }

    #[test]
    fn malformed_key_chords_are_rejected_before_dispatch() {
        assert!(parse_key("").is_err());
        assert!(parse_key("Control+").is_err());
        assert!(parse_key("Hyper+A").is_err());
        assert!(parse_key("AB").is_err());
    }
}
