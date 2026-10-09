//! Persistent settings shared with the client.
//!
//! Stored as JSON at `$XDG_CONFIG_HOME/silex/settings.json` (fallback
//! `~/.config/silex/settings.json`). The schema matches the client's so the client can
//! read the file directly (e.g. for the `start_hidden` bootstrap) and edit it through
//! the daemon over IPC. The daemon owns the file on disk. Unknown fields are ignored on
//! load.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

const REFRESH_MIN: i64 = 3;
const REFRESH_MAX: i64 = 60;

pub fn config_dir() -> PathBuf {
    if let Ok(x) = std::env::var("XDG_CONFIG_HOME") {
        if !x.is_empty() {
            return PathBuf::from(x).join("silex");
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".config").join("silex")
}

pub fn config_path() -> PathBuf {
    config_dir().join("settings.json")
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct DeviceRule {
    pub server_mac: String,
    pub key: String,
    pub label: String,
    pub alias: String,
    pub auto_connect: bool,
    pub reconnect_on_error: bool,
    pub launch_app: String,
    pub disconnect_on_app_close: bool,
    pub auto_disconnect_on_request: bool,
    pub auto_disconnect_timeout: i64,
}

impl Default for DeviceRule {
    fn default() -> Self {
        Self {
            server_mac: String::new(),
            key: String::new(),
            label: String::new(),
            alias: String::new(),
            auto_connect: false,
            reconnect_on_error: true,
            launch_app: String::new(),
            disconnect_on_app_close: false,
            auto_disconnect_on_request: false,
            auto_disconnect_timeout: 120,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct SearchAddress {
    pub address: String,
    pub port: Option<u16>,
}

impl Default for SearchAddress {
    fn default() -> Self {
        Self { address: String::new(), port: None }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Settings {
    pub autostart: bool,
    pub start_hidden: bool,
    pub notify_errors: bool,
    pub confirm_disconnect: bool,
    pub auto_connect_new: bool,
    pub hide_on_close: bool,
    pub refresh_interval: i64,
    pub search_enabled: bool,
    pub search_addresses: Vec<SearchAddress>,
    pub columns: Vec<String>,
    pub column_widths: BTreeMap<String, i64>,
    pub filter_enabled: bool,
    pub filter_servers: Vec<String>,
    pub device_rules: Vec<DeviceRule>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            autostart: false,
            start_hidden: false,
            notify_errors: true,
            confirm_disconnect: true,
            auto_connect_new: false,
            hide_on_close: true,
            refresh_interval: 10,
            search_enabled: false,
            search_addresses: Vec::new(),
            columns: vec![
                "name".into(),
                "type".into(),
                "status".into(),
                "hostname".into(),
                "model".into(),
            ],
            column_widths: BTreeMap::new(),
            filter_enabled: false,
            filter_servers: Vec::new(),
            device_rules: Vec::new(),
        }
    }
}

impl Settings {
    pub fn clamp(&mut self) {
        self.refresh_interval = self.refresh_interval.clamp(REFRESH_MIN, REFRESH_MAX);
    }

    pub fn from_dict(value: Value) -> Self {
        let mut s: Settings = serde_json::from_value(value).unwrap_or_default();
        s.clamp();
        s
    }

    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }

    pub fn rule_for(&self, server_mac: &str, key: &str) -> Option<&DeviceRule> {
        self.device_rules.iter().find(|r| r.server_mac == server_mac && r.key == key)
    }

    pub fn should_auto_connect(&self, server_mac: &str, key: &str) -> bool {
        match self.rule_for(server_mac, key) {
            Some(r) => r.auto_connect,
            None => self.auto_connect_new,
        }
    }

    /// Create or update a per-device rule (upsert). Only non-null fields are applied.
    pub fn set_device_rule(&mut self, server_mac: &str, key: &str, updates: &Map<String, Value>) {
        let idx = self.device_rules.iter().position(|r| r.server_mac == server_mac && r.key == key);
        let mut rule = match idx {
            Some(i) => self.device_rules.remove(i),
            None => DeviceRule { server_mac: server_mac.to_string(), key: key.to_string(), ..Default::default() },
        };
        // Merge via JSON so field names map automatically; skip mac/key and nulls.
        let mut obj = match serde_json::to_value(&rule) {
            Ok(Value::Object(m)) => m,
            _ => Map::new(),
        };
        for (k, v) in updates {
            if k == "mac" || k == "key" || v.is_null() {
                continue;
            }
            if obj.contains_key(k) {
                obj.insert(k.clone(), v.clone());
            }
        }
        if let Ok(updated) = serde_json::from_value::<DeviceRule>(Value::Object(obj)) {
            rule = updated;
        }
        rule.server_mac = server_mac.to_string();
        rule.key = key.to_string();
        self.device_rules.push(rule);
    }
}

pub struct SettingsStore {
    path: PathBuf,
    settings: Mutex<Settings>,
}

impl SettingsStore {
    pub fn new(path: Option<PathBuf>) -> Self {
        let path = path.unwrap_or_else(config_path);
        let settings = Self::load_from(&path);
        Self { path, settings: Mutex::new(settings) }
    }

    fn load_from(path: &PathBuf) -> Settings {
        match std::fs::read_to_string(path) {
            Ok(text) => match serde_json::from_str::<Value>(&text) {
                Ok(v) => Settings::from_dict(v),
                Err(_) => Settings::default(),
            },
            Err(_) => Settings::default(),
        }
    }

    pub fn get(&self) -> Settings {
        self.settings.lock().unwrap().clone()
    }

    fn write_atomic(path: &PathBuf, s: &Settings) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut clamped = s.clone();
        clamped.clamp();
        let json = serde_json::to_string_pretty(&clamped)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json.as_bytes())?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Apply a partial patch to the current settings and save.
    pub fn update(&self, patch: Value) -> Result<Settings> {
        let mut guard = self.settings.lock().unwrap();
        let mut base = match guard.to_value() {
            Value::Object(m) => m,
            _ => Map::new(),
        };
        if let Value::Object(p) = patch {
            for (k, v) in p {
                base.insert(k, v);
            }
        }
        let mut merged = Settings::from_dict(Value::Object(base));
        merged.clamp();
        *guard = merged.clone();
        Self::write_atomic(&self.path, &merged)?;
        Ok(merged)
    }

    /// Upsert a device rule and save.
    pub fn set_device_rule(&self, server_mac: &str, key: &str, updates: &Map<String, Value>) -> Result<Settings> {
        let mut guard = self.settings.lock().unwrap();
        guard.set_device_rule(server_mac, key, updates);
        let snapshot = guard.clone();
        Self::write_atomic(&self.path, &snapshot)?;
        Ok(snapshot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn defaults_match_reference() {
        let s = Settings::default();
        assert!(s.hide_on_close);
        assert!(s.confirm_disconnect);
        assert!(s.notify_errors);
        assert_eq!(s.refresh_interval, 10);
        assert_eq!(s.columns, vec!["name", "type", "status", "hostname", "model"]);
    }

    #[test]
    fn clamp_refresh() {
        let s = Settings::from_dict(json!({"refresh_interval": 999}));
        assert_eq!(s.refresh_interval, 60);
        let s = Settings::from_dict(json!({"refresh_interval": 1}));
        assert_eq!(s.refresh_interval, 3);
    }

    #[test]
    fn ignores_unknown_keys() {
        let s = Settings::from_dict(json!({"refresh_interval": 10, "future_field": 42}));
        assert_eq!(s.refresh_interval, 10);
    }

    #[test]
    fn device_rule_default_reconnect_true() {
        assert!(DeviceRule::default().reconnect_on_error);
        assert_eq!(DeviceRule::default().auto_disconnect_timeout, 120);
    }

    #[test]
    fn should_auto_connect_prefers_rule() {
        let mut s = Settings { auto_connect_new: false, ..Default::default() };
        s.set_device_rule("MAC", "KEY", &json!({"auto_connect": true}).as_object().unwrap().clone());
        assert!(s.should_auto_connect("MAC", "KEY"));
        assert!(!s.should_auto_connect("MAC", "OTHER"));
    }

    #[test]
    fn roundtrip_via_store() {
        let dir = std::env::temp_dir().join(format!("osl-settings-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        let store = SettingsStore::new(Some(path.clone()));
        store.update(json!({"refresh_interval": 25})).unwrap();
        store
            .set_device_rule(
                "00:11:22:33:44:55",
                "1234:5678:USB Flash Memory",
                &json!({"label": "Good stick", "auto_connect": true}).as_object().unwrap().clone(),
            )
            .unwrap();

        let reloaded = SettingsStore::new(Some(path.clone()));
        let s = reloaded.get();
        assert_eq!(s.refresh_interval, 25);
        assert_eq!(s.device_rules.len(), 1);
        assert!(s.device_rules[0].auto_connect);
        assert_eq!(s.device_rules[0].label, "Good stick");
        std::fs::remove_dir_all(&dir).ok();
    }
}
