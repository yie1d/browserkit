// Legacy keyboard helpers kept here until act.rs owns them in Task 7.

use crate::error::BkError;

/// Parse a key string like "Control+Shift+Enter" and dispatch keyDown/keyUp events.
pub async fn dispatch_key_combo(
    session: &cdpkit::Session<'_>,
    key_str: &str,
) -> Result<(), BkError> {
    let parts: Vec<&str> = key_str.split('+').collect();

    let mut modifiers: i64 = 0;
    let mut main_key: Option<&str> = None;

    for part in &parts {
        match *part {
            "Alt" => modifiers |= 1,
            "Control" | "Ctrl" => modifiers |= 2,
            "Meta" | "Command" | "Cmd" => modifiers |= 4,
            "Shift" => modifiers |= 8,
            _ => main_key = Some(part),
        }
    }

    let key_name = main_key.unwrap_or(parts.last().unwrap_or(&""));
    let key_def = resolve_key(key_name);

    if modifiers & 1 != 0 {
        send_key_event(session, "rawKeyDown", "Alt", "AltLeft", 18, None, modifiers).await?;
    }
    if modifiers & 2 != 0 {
        send_key_event(session, "rawKeyDown", "Control", "ControlLeft", 17, None, modifiers)
            .await?;
    }
    if modifiers & 4 != 0 {
        send_key_event(session, "rawKeyDown", "Meta", "MetaLeft", 91, None, modifiers).await?;
    }
    if modifiers & 8 != 0 {
        send_key_event(session, "rawKeyDown", "Shift", "ShiftLeft", 16, None, modifiers).await?;
    }

    let event_type = if key_def.text.is_some() {
        "keyDown"
    } else {
        "rawKeyDown"
    };
    send_key_event(
        session,
        event_type,
        key_def.key,
        key_def.code,
        key_def.key_code,
        key_def.text,
        modifiers,
    )
    .await?;

    send_key_event(
        session,
        "keyUp",
        key_def.key,
        key_def.code,
        key_def.key_code,
        None,
        modifiers,
    )
    .await?;

    if modifiers & 8 != 0 {
        send_key_event(session, "keyUp", "Shift", "ShiftLeft", 16, None, 0).await?;
    }
    if modifiers & 4 != 0 {
        send_key_event(session, "keyUp", "Meta", "MetaLeft", 91, None, 0).await?;
    }
    if modifiers & 2 != 0 {
        send_key_event(session, "keyUp", "Control", "ControlLeft", 17, None, 0).await?;
    }
    if modifiers & 1 != 0 {
        send_key_event(session, "keyUp", "Alt", "AltLeft", 18, None, 0).await?;
    }

    Ok(())
}

async fn send_key_event(
    session: &cdpkit::Session<'_>,
    type_: &str,
    key: &str,
    code: &str,
    key_code: i64,
    text: Option<&str>,
    modifiers: i64,
) -> Result<(), BkError> {
    use cdpkit::Sender;

    let mut cmd = cdpkit::input::methods::DispatchKeyEvent::new(type_)
        .with_key(key)
        .with_code(code)
        .with_windows_virtual_key_code(key_code)
        .with_native_virtual_key_code(key_code);

    if modifiers != 0 {
        cmd = cmd.with_modifiers(modifiers);
    }
    if let Some(t) = text {
        cmd = cmd.with_text(t);
    }

    session.send_cmd(cmd).await?;
    Ok(())
}

struct KeyDef {
    key: &'static str,
    code: &'static str,
    key_code: i64,
    text: Option<&'static str>,
}

fn resolve_key(name: &str) -> KeyDef {
    match name {
        "Enter" | "Return" => KeyDef {
            key: "Enter",
            code: "Enter",
            key_code: 13,
            text: Some("\r"),
        },
        "Tab" => KeyDef {
            key: "Tab",
            code: "Tab",
            key_code: 9,
            text: Some("\t"),
        },
        "Escape" | "Esc" => KeyDef {
            key: "Escape",
            code: "Escape",
            key_code: 27,
            text: None,
        },
        "Backspace" => KeyDef {
            key: "Backspace",
            code: "Backspace",
            key_code: 8,
            text: None,
        },
        "Delete" | "Del" => KeyDef {
            key: "Delete",
            code: "Delete",
            key_code: 46,
            text: None,
        },
        "ArrowUp" | "Up" => KeyDef {
            key: "ArrowUp",
            code: "ArrowUp",
            key_code: 38,
            text: None,
        },
        "ArrowDown" | "Down" => KeyDef {
            key: "ArrowDown",
            code: "ArrowDown",
            key_code: 40,
            text: None,
        },
        "ArrowLeft" | "Left" => KeyDef {
            key: "ArrowLeft",
            code: "ArrowLeft",
            key_code: 37,
            text: None,
        },
        "ArrowRight" | "Right" => KeyDef {
            key: "ArrowRight",
            code: "ArrowRight",
            key_code: 39,
            text: None,
        },
        "Home" => KeyDef {
            key: "Home",
            code: "Home",
            key_code: 36,
            text: None,
        },
        "End" => KeyDef {
            key: "End",
            code: "End",
            key_code: 35,
            text: None,
        },
        "PageUp" => KeyDef {
            key: "PageUp",
            code: "PageUp",
            key_code: 33,
            text: None,
        },
        "PageDown" => KeyDef {
            key: "PageDown",
            code: "PageDown",
            key_code: 34,
            text: None,
        },
        "Space" => KeyDef {
            key: " ",
            code: "Space",
            key_code: 32,
            text: Some(" "),
        },
        "Insert" => KeyDef {
            key: "Insert",
            code: "Insert",
            key_code: 45,
            text: None,
        },
        "F1" => KeyDef {
            key: "F1",
            code: "F1",
            key_code: 112,
            text: None,
        },
        "F2" => KeyDef {
            key: "F2",
            code: "F2",
            key_code: 113,
            text: None,
        },
        "F3" => KeyDef {
            key: "F3",
            code: "F3",
            key_code: 114,
            text: None,
        },
        "F4" => KeyDef {
            key: "F4",
            code: "F4",
            key_code: 115,
            text: None,
        },
        "F5" => KeyDef {
            key: "F5",
            code: "F5",
            key_code: 116,
            text: None,
        },
        "F6" => KeyDef {
            key: "F6",
            code: "F6",
            key_code: 117,
            text: None,
        },
        "F7" => KeyDef {
            key: "F7",
            code: "F7",
            key_code: 118,
            text: None,
        },
        "F8" => KeyDef {
            key: "F8",
            code: "F8",
            key_code: 119,
            text: None,
        },
        "F9" => KeyDef {
            key: "F9",
            code: "F9",
            key_code: 120,
            text: None,
        },
        "F10" => KeyDef {
            key: "F10",
            code: "F10",
            key_code: 121,
            text: None,
        },
        "F11" => KeyDef {
            key: "F11",
            code: "F11",
            key_code: 122,
            text: None,
        },
        "F12" => KeyDef {
            key: "F12",
            code: "F12",
            key_code: 123,
            text: None,
        },
        other => {
            if other.len() == 1 {
                let ch = other.chars().next().unwrap();
                let upper = ch.to_ascii_uppercase();
                let key_code = upper as i64;
                let key_str: &'static str = Box::leak(other.to_string().into_boxed_str());
                let text_str: &'static str = Box::leak(other.to_lowercase().into_boxed_str());
                let code_str: &'static str = if ch.is_ascii_alphabetic() {
                    Box::leak(format!("Key{}", upper).into_boxed_str())
                } else if ch.is_ascii_digit() {
                    Box::leak(format!("Digit{}", ch).into_boxed_str())
                } else {
                    key_str
                };
                KeyDef {
                    key: key_str,
                    code: code_str,
                    key_code,
                    text: Some(text_str),
                }
            } else {
                let key_str: &'static str = Box::leak(other.to_string().into_boxed_str());
                KeyDef {
                    key: key_str,
                    code: key_str,
                    key_code: 0,
                    text: None,
                }
            }
        }
    }
}
