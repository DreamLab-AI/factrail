//! The on/off switch: the session store beats the manifest default.

use serde_json::Value;

/// Is factrail on for this session?
///
/// Resolution order: a boolean in the session store (what `/factrail on|off`
/// wrote) wins; then the `enabledByDefault` option — a boolean as given, or a
/// string that is off when it is `0`, `false`, `off` or `no` (case-insensitive,
/// untrimmed, as before) and on otherwise. Nothing usable anywhere ⇒ on, because
/// the plugin is only registered at all when the manifest gate is.
///
/// ```
/// use factrail_policy::resolve_enabled;
/// use serde_json::json;
/// assert!(resolve_enabled(None, None));
/// assert!(!resolve_enabled(None, Some(&json!("OFF"))));
/// assert!(resolve_enabled(Some(&json!(true)), Some(&json!(false))));
/// ```
pub fn resolve_enabled(store: Option<&Value>, option: Option<&Value>) -> bool {
    if let Some(Value::Bool(b)) = store {
        return *b;
    }
    match option {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => {
            !matches!(s.to_lowercase().as_str(), "0" | "false" | "off" | "no")
        }
        _ => true,
    }
}

/// A parsed `/factrail <args>` command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwitchCommand {
    /// `on`, `enable`, `1`, `true`.
    On,
    /// `off`, `disable`, `0`, `false`.
    Off,
    /// No argument, or `status`.
    Status,
    /// Anything else.
    Help,
}

impl SwitchCommand {
    /// Parse the command's arguments, trimmed and case-insensitive.
    ///
    /// ```
    /// use factrail_policy::SwitchCommand;
    /// assert_eq!(SwitchCommand::parse(" OFF "), SwitchCommand::Off);
    /// assert_eq!(SwitchCommand::parse(""), SwitchCommand::Status);
    /// assert_eq!(SwitchCommand::parse("maybe"), SwitchCommand::Help);
    /// ```
    pub fn parse(args: &str) -> Self {
        match args.trim().to_lowercase().as_str() {
            "on" | "enable" | "1" | "true" => Self::On,
            "off" | "disable" | "0" | "false" => Self::Off,
            "" | "status" => Self::Status,
            _ => Self::Help,
        }
    }

    /// The canonical spelling: `on`, `off`, `status` or `help`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::On => "on",
            Self::Off => "off",
            Self::Status => "status",
            Self::Help => "help",
        }
    }
}
