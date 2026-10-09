//! Privileged attach/detach of the USB/IP device (vhci) for the daemon.
//!
//! `usbip attach/detach` and `modprobe vhci-hcd` need root, obtained at runtime via an
//! escalation: `pkexec` (desktop polkit prompt), `sudo` (`sudo -n`, non-interactive for
//! headless/test setups), or `none` (already root).
//!
//! One prompt per session instead of per connect: if the privileged helper is installed
//! (see packaging/), pkexec runs through it (a polkit action with `auth_admin_keep`);
//! otherwise it falls back to `pkexec usbip` (a prompt per connect). The USB/IP *server*
//! runs unprivileged inside the daemon; only the client-side attach to vhci needs root.

use std::collections::HashSet;
use std::path::Path;
use std::process::Command;

use anyhow::{bail, Result};

pub const REMOTE: &str = "127.0.0.1";

const SEARCH_DIRS: &[&str] = &["/usr/sbin", "/sbin", "/usr/bin", "/bin", "/usr/local/sbin", "/usr/local/bin"];

/// Install location of the privileged helper (must match exec.path in the polkit policy).
pub const HELPER_PATH: &str = "/usr/libexec/silex-usbip-helper";

/// Privileged usbip operations (injectable so the daemon is testable with a fake).
pub trait UsbipOps: Send + Sync {
    fn ensure_module(&self) -> Result<()>;
    fn used_ports(&self) -> HashSet<i32>;
    fn attach(&self, busid: &str) -> Result<()>;
    fn detach(&self, port: i32) -> Result<()>;
}

pub fn find_helper() -> Option<String> {
    let cand = std::env::var("SILEX_USBIP_HELPER").unwrap_or_else(|_| HELPER_PATH.to_string());
    if Path::new(&cand).exists() {
        Some(cand)
    } else {
        None
    }
}

/// Absolute path to `prog` (pkexec requires absolute paths, no PATH lookup).
pub fn resolve(prog: &str) -> String {
    if prog.starts_with('/') {
        return prog.to_string();
    }
    if let Ok(path) = std::env::var("PATH") {
        for dir in path.split(':') {
            let cand = format!("{dir}/{prog}");
            if Path::new(&cand).exists() {
                return cand;
            }
        }
    }
    for dir in SEARCH_DIRS {
        let cand = format!("{dir}/{prog}");
        if Path::new(&cand).exists() {
            return cand;
        }
    }
    prog.to_string()
}

fn escalation_prefix(mode: &str) -> Result<Vec<String>> {
    match mode {
        "pkexec" => Ok(vec![resolve("pkexec")]),
        "sudo" => Ok(vec![resolve("sudo"), "-n".to_string()]),
        "none" => Ok(Vec::new()),
        other => bail!("unknown escalation mode: {other:?}"),
    }
}

pub struct UsbipControl {
    escalation: String,
    remote: String,
    helper: Option<String>,
}

impl UsbipControl {
    pub fn new(escalation: &str) -> Self {
        Self {
            escalation: escalation.to_string(),
            remote: REMOTE.to_string(),
            helper: find_helper(),
        }
    }

    /// Build the command line including escalation.
    fn build_cmd(&self, argv: &[&str]) -> Vec<String> {
        if self.escalation == "pkexec" {
            if let Some(helper) = &self.helper {
                let helper_args: Option<Vec<String>> = match argv.first().copied() {
                    Some("usbip") => Some(argv[1..].iter().map(|s| s.to_string()).collect()),
                    Some("modprobe") => Some(vec!["modprobe".to_string()]),
                    _ => None,
                };
                if let Some(hargs) = helper_args {
                    let mut cmd = vec![resolve("pkexec"), helper.clone()];
                    cmd.extend(hargs);
                    return cmd;
                }
            }
        }
        let mut cmd = escalation_prefix(&self.escalation).unwrap_or_default();
        cmd.push(resolve(argv[0]));
        cmd.extend(argv[1..].iter().map(|s| s.to_string()));
        cmd
    }

    fn run_priv(&self, argv: &[&str]) -> Result<()> {
        let cmd = self.build_cmd(argv);
        let out = Command::new(&cmd[0]).args(&cmd[1..]).output();
        match out {
            Ok(o) if o.status.success() => Ok(()),
            Ok(o) => {
                let detail = String::from_utf8_lossy(if !o.stderr.is_empty() { &o.stderr } else { &o.stdout });
                bail!("{} failed: {}", argv.join(" "), detail.trim());
            }
            Err(e) => bail!("{} failed: {e}", argv.join(" ")),
        }
    }
}

impl UsbipOps for UsbipControl {
    fn ensure_module(&self) -> Result<()> {
        // Only load if not already loaded -> saves a second pkexec prompt.
        if Path::new("/sys/module/vhci_hcd").exists() {
            return Ok(());
        }
        self.run_priv(&["modprobe", "vhci-hcd"])
    }

    fn used_ports(&self) -> HashSet<i32> {
        let out = Command::new(resolve("usbip")).arg("port").output();
        match out {
            Ok(o) => parse_used_ports(&String::from_utf8_lossy(&o.stdout)),
            Err(_) => HashSet::new(),
        }
    }

    fn attach(&self, busid: &str) -> Result<()> {
        self.run_priv(&["usbip", "attach", "-r", &self.remote, "-b", busid])
    }

    fn detach(&self, port: i32) -> Result<()> {
        let p = port.to_string();
        self.run_priv(&["usbip", "detach", "-p", &p])
    }
}

/// Extract busy vhci ports from the output of `usbip port`.
/// Lines look like: `Port 00: <Port in Use> ...`.
pub fn parse_used_ports(text: &str) -> HashSet<i32> {
    let mut ports = HashSet::new();
    for line in text.lines() {
        let line = line.trim_start();
        if let Some(rest) = line.strip_prefix("Port ") {
            if let Some(colon) = rest.find(':') {
                let (num, after) = rest.split_at(colon);
                if after.contains("<Port in Use>") {
                    if let Ok(n) = num.trim().parse::<i32>() {
                        ports.insert(n);
                    }
                }
            }
        }
    }
    ports
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ports() {
        let text = "\
Imported USB devices
====================
Port 00: <Port in Use> at High Speed(480Mbps)
       unknown vendor : unknown product (0930:6544)
Port 01: <Port Available>
Port 02: <Port in Use> at Full Speed(12Mbps)
";
        let ports = parse_used_ports(text);
        assert_eq!(ports, HashSet::from([0, 2]));
    }

    #[test]
    fn build_cmd_without_helper_uses_escalation() {
        let ctl = UsbipControl { escalation: "sudo".into(), remote: REMOTE.into(), helper: None };
        let cmd = ctl.build_cmd(&["usbip", "attach", "-r", "127.0.0.1", "-b", "1-1"]);
        assert_eq!(cmd[1], "-n");
        assert!(cmd[0].ends_with("sudo"));
        assert!(cmd.iter().any(|c| c == "attach") && cmd.iter().any(|c| c == "1-1"));
    }

    #[test]
    fn build_cmd_none_is_direct() {
        let ctl = UsbipControl { escalation: "none".into(), remote: REMOTE.into(), helper: None };
        let cmd = ctl.build_cmd(&["usbip", "detach", "-p", "0"]);
        assert!(cmd[0].ends_with("usbip"));
        assert_eq!(cmd.last().unwrap(), "0");
    }

    #[test]
    fn build_cmd_with_helper_routes_through_pkexec() {
        let ctl = UsbipControl {
            escalation: "pkexec".into(),
            remote: REMOTE.into(),
            helper: Some("/usr/libexec/silex-usbip-helper".into()),
        };
        let cmd = ctl.build_cmd(&["usbip", "attach", "-r", "127.0.0.1", "-b", "1-2"]);
        assert!(cmd[0].ends_with("pkexec"));
        assert_eq!(cmd[1], "/usr/libexec/silex-usbip-helper");
        assert_eq!(cmd[2], "attach");
        let m = ctl.build_cmd(&["modprobe", "vhci-hcd"]);
        assert_eq!(m[2], "modprobe");
    }
}
