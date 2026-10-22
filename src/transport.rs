//! Dispatch de transporte por contexto — decidido 1x no startup (sem flapping).
//!
//! Estratégia (P0 review kimi k3):
//! - Lê `~/.docker/contexts/meta/*/meta.json` → mapa nome → endpoint.
//! - Roda `docker context ls --format {{.Name}}` com timeout do config.
//! - `unix://` → [`Transport::Api`] (Bollard, rustls — sem openssl).
//!   `ssh://` (homeserver, oci-ampere-1/2) e demais → [`Transport::Cli`]
//!   (`tokio::process docker --context X stats/inspect`).
//! - Decisão persistida em [`DiscoveredContexts`]; badge `api/cli` na TUI;
//!   uma linha de log por contexto no startup.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// Bollard async (unix socket local).
    Api,
    /// Subprocesso `docker --context` (túneis ssh).
    Cli,
}

impl Transport {
    pub fn badge(self) -> &'static str {
        match self {
            Transport::Api => "api",
            Transport::Cli => "cli",
        }
    }
}

#[derive(Debug, Clone)]
pub struct DockerContext {
    pub name: String,
    pub endpoint: String,
    pub transport: Transport,
}

#[derive(Debug, Clone, Default)]
pub struct DiscoveredContexts {
    pub contexts: Vec<DockerContext>,
}

impl DiscoveredContexts {
    pub fn names(&self) -> Vec<String> {
        self.contexts.iter().map(|c| c.name.clone()).collect()
    }

    pub fn get(&self, name: &str) -> Option<&DockerContext> {
        self.contexts.iter().find(|c| c.name == name)
    }
}

#[derive(Debug, Deserialize)]
struct MetaFile {
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "Endpoints")]
    endpoints: Option<HashMap<String, MetaEndpoint>>,
}

#[derive(Debug, Deserialize)]
struct MetaEndpoint {
    #[serde(rename = "Host")]
    host: Option<String>,
}

/// Lê o mapa nome → endpoint a partir de `~/.docker/contexts/meta/*/meta.json`.
pub fn read_meta_endpoints() -> HashMap<String, String> {
    let mut map = HashMap::new();
    let base: PathBuf = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("~"))
        .join(".docker/contexts/meta");
    let entries = std::fs::read_dir(&base);
    if entries.is_err() {
        return map;
    }
    for entry in entries.expect("read_dir ok").flatten() {
        let meta = entry.path().join("meta.json");
        if !meta.is_file() {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&meta) else {
            continue;
        };
        let Ok(parsed) = serde_json::from_str::<MetaFile>(&text) else {
            continue;
        };
        if let Some(eps) = parsed.endpoints {
            if let Some(docker) = eps.get("docker") {
                if let Some(host) = docker.host.clone() {
                    map.insert(parsed.name, host);
                }
            }
        }
    }
    map
}

fn decide_transport(endpoint: &str) -> Transport {
    if endpoint.starts_with("unix://") {
        Transport::Api
    } else {
        // ssh://, tcp://, npipe:// etc → CLI (sem flapping por request).
        Transport::Cli
    }
}

/// Descobre contextos no startup: `docker context ls` + meta.json.
/// Fallback: `["default"]` (unix socket) se o CLI falhar.
pub async fn discover(context_ls_timeout: Duration) -> DiscoveredContexts {
    let meta = read_meta_endpoints();
    let names = list_context_names(context_ls_timeout).await;
    let mut out = Vec::new();
    for name in names {
        let endpoint = meta
            .get(&name)
            .cloned()
            .unwrap_or_else(|| default_endpoint_for(&name));
        let transport = decide_transport(&endpoint);
        // Uma linha de log por contexto, 1x no startup.
        eprintln!(
            "transport: context={name} endpoint={endpoint} via={}",
            transport.badge()
        );
        out.push(DockerContext {
            name,
            endpoint,
            transport,
        });
    }
    DiscoveredContexts { contexts: out }
}

fn default_endpoint_for(name: &str) -> String {
    if name == "default" {
        "unix:///var/run/docker.sock".to_string()
    } else {
        "unknown://".to_string()
    }
}

async fn list_context_names(timeout: Duration) -> Vec<String> {
    let run = async {
        tokio::process::Command::new("docker")
            .args(["context", "ls", "--format", "{{.Name}}"])
            .output()
            .await
    };
    let Ok(Ok(out)) = tokio::time::timeout(timeout, run).await else {
        return vec!["default".to_string()];
    };
    if !out.status.success() {
        return vec!["default".to_string()];
    }
    let names: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    if names.is_empty() {
        vec!["default".to_string()]
    } else {
        names
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unix_goes_api_ssh_goes_cli() {
        assert_eq!(
            decide_transport("unix:///var/run/docker.sock"),
            Transport::Api
        );
        assert_eq!(
            decide_transport("ssh://lgldsilva@192.168.0.100"),
            Transport::Cli
        );
        assert_eq!(
            decide_transport("ssh://lgldsilva@10.228.143.11"),
            Transport::Cli
        );
        assert_eq!(decide_transport("tcp://x:2375"), Transport::Cli);
    }

    #[test]
    fn badge_labels() {
        assert_eq!(Transport::Api.badge(), "api");
        assert_eq!(Transport::Cli.badge(), "cli");
    }

    #[test]
    fn default_endpoint_is_unix() {
        assert!(default_endpoint_for("default").starts_with("unix://"));
    }
}
