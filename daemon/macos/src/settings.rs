// The menu bar's "Settings..." window: the config fields people actually
// need to change, so config.json needn't be edited by hand. It's a plain
// alert with the fields in it, run on the main thread (the menu bar's).
// Saving keeps every other field in config.json as it was, then restarts
// the app, which only reads its config at startup.

use objc2::rc::Retained;
use objc2::{MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{NSAlert, NSApplication, NSSecureTextField, NSTextField, NSView};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};
use serde_json::{Map, Value};

use mwb_protocol::config;

/// One editable config field.
struct Field {
    key: &'static str,
    label: &'static str,
    kind: Kind,
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Text,
    Secret,
    MachineId,
    PositiveNumber,
}

const FIELDS: &[Field] = &[
    Field { key: "windows_ip", label: "Windows PC (host:port)", kind: Kind::Text },
    Field { key: "security_key", label: "Security key", kind: Kind::Secret },
    Field { key: "machine_name", label: "This Mac's name", kind: Kind::Text },
    Field { key: "machine_id", label: "This Mac's ID", kind: Kind::MachineId },
    Field { key: "scroll_speed", label: "Scroll speed", kind: Kind::PositiveNumber },
];

const ROW: f64 = 30.0;
const LABEL_WIDTH: f64 = 170.0;
const FIELD_WIDTH: f64 = 230.0;
const SAVE: isize = 1000; // NSAlertFirstButtonReturn

/// Shows the window; on Save, writes the config and restarts the app.
pub fn show(mtm: MainThreadMarker) {
    let mut current = read_config_object();
    let mut error: Option<String> = None;
    loop {
        let alert = NSAlert::new(mtm);
        alert.setMessageText(&NSString::from_str("MWB Mac Bridge Settings"));
        let note = "Saved to config.json, and the app restarts to use them. Other settings stay as they are (Edit Config... for those).";
        let text = match &error {
            Some(e) => format!("{e}\n\n{note}"),
            None => note.to_string(),
        };
        alert.setInformativeText(&NSString::from_str(&text));
        alert.addButtonWithTitle(&NSString::from_str("Save and Restart"));
        alert.addButtonWithTitle(&NSString::from_str("Cancel"));

        let height = ROW * FIELDS.len() as f64;
        let view = NSView::initWithFrame(NSView::alloc(mtm), NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(LABEL_WIDTH + FIELD_WIDTH, height)));
        let mut inputs: Vec<Retained<NSTextField>> = Vec::new();
        for (i, field) in FIELDS.iter().enumerate() {
            let y = height - ROW * (i as f64 + 1.0) + 4.0;
            let label = NSTextField::labelWithString(&NSString::from_str(field.label), mtm);
            label.setFrame(NSRect::new(NSPoint::new(0.0, y + 2.0), NSSize::new(LABEL_WIDTH - 8.0, 20.0)));
            view.addSubview(&label);
            let frame = NSRect::new(NSPoint::new(LABEL_WIDTH, y), NSSize::new(FIELD_WIDTH, 22.0));
            let input: Retained<NSTextField> = if field.kind == Kind::Secret {
                Retained::into_super(NSSecureTextField::initWithFrame(NSSecureTextField::alloc(mtm), frame))
            } else {
                NSTextField::initWithFrame(NSTextField::alloc(mtm), frame)
            };
            input.setStringValue(&NSString::from_str(&display_value(current.get(field.key))));
            view.addSubview(&input);
            inputs.push(input);
        }
        alert.setAccessoryView(Some(&view));
        alert.window().setInitialFirstResponder(Some(&inputs[0]));

        // A menu-bar-only app isn't active, and the window would open behind
        // whatever is.
        #[allow(deprecated)]
        NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
        if alert.runModal() != SAVE {
            return;
        }

        let typed: Vec<String> = inputs.iter().map(|f| f.stringValue().to_string().trim().to_string()).collect();
        match apply(&mut current, &typed) {
            Ok(()) => {
                if let Err(e) = write_config_object(&current) {
                    error = Some(format!("Couldn't save config.json: {e}"));
                    continue;
                }
                crate::tray::restart_app();
                return;
            }
            Err(e) => {
                // Shown again with what was typed, and what's wrong.
                for (field, value) in FIELDS.iter().zip(&typed) {
                    current.insert(field.key.to_string(), Value::String(value.clone()));
                }
                error = Some(e);
            }
        }
    }
}

fn display_value(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Bool(b)) => b.to_string(),
        _ => String::new(),
    }
}

/// Checks what was typed and puts it into `config`, typed as the daemon
/// reads it. Everything else in `config` is left alone.
fn apply(config: &mut Map<String, Value>, typed: &[String]) -> Result<(), String> {
    let mut updates = Vec::new();
    for (field, value) in FIELDS.iter().zip(typed) {
        let parsed = match field.kind {
            Kind::Text | Kind::Secret if value.is_empty() => return Err(format!("{} can't be empty.", field.label)),
            Kind::Text | Kind::Secret => Value::String(value.clone()),
            Kind::MachineId => match value.parse::<u32>() {
                Ok(id) if id != 0 && id != 255 => Value::from(id),
                _ => return Err(format!("{} must be a whole number other than 0 and 255 (it has to match this Mac's ID in Windows' MachinePool).", field.label)),
            },
            Kind::PositiveNumber => match value.parse::<f64>() {
                Ok(n) if n > 0.0 && n.is_finite() => serde_json::Number::from_f64(n).map(Value::Number).ok_or("not a number")?,
                _ => return Err(format!("{} must be a number above 0 (1 is normal).", field.label)),
            },
        };
        updates.push((field.key, parsed));
    }
    for (key, value) in updates {
        config.insert(key.to_string(), value);
    }
    Ok(())
}

fn read_config_object() -> Map<String, Value> {
    std::fs::read_to_string(config::config_path())
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

fn write_config_object(config: &Map<String, Value>) -> std::io::Result<()> {
    let path = config::config_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let body = serde_json::to_string_pretty(&Value::Object(config.clone())).map_err(std::io::Error::other)?;
    std::fs::write(path, body + "\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(values: [&str; 5]) -> Vec<String> {
        values.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn saving_types_the_fields_and_keeps_the_rest() {
        let mut config: Map<String, Value> = serde_json::from_str(r#"{"windows_keys": false, "neighbors": {"right": 5}, "machine_id": 2}"#).unwrap();
        apply(&mut config, &typed(["pc.local:15101", "key", "mac", "7", "1.5"])).unwrap();
        assert_eq!(config["machine_id"], Value::from(7u32));
        assert_eq!(config["scroll_speed"], serde_json::json!(1.5));
        assert_eq!(config["windows_ip"], Value::from("pc.local:15101"));
        assert_eq!(config["windows_keys"], Value::Bool(false));
        assert_eq!(config["neighbors"]["right"], Value::from(5));
    }

    #[test]
    fn bad_values_are_refused_and_nothing_changes() {
        let mut config: Map<String, Value> = serde_json::from_str(r#"{"machine_id": 2}"#).unwrap();
        assert!(apply(&mut config, &typed(["pc", "key", "mac", "255", "1"])).is_err());
        assert!(apply(&mut config, &typed(["pc", "key", "mac", "abc", "1"])).is_err());
        assert!(apply(&mut config, &typed(["pc", "", "mac", "3", "1"])).is_err());
        assert!(apply(&mut config, &typed(["pc", "key", "mac", "3", "0"])).is_err());
        assert_eq!(config["machine_id"], Value::from(2));
        assert!(!config.contains_key("windows_ip"));
    }
}
