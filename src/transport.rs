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
    let base: PathBuf = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("~"))
        .join(".docker/contexts/meta");
    read_meta_endpoints_from(&base)
}

/// Variante testável com base explícita.
pub fn read_meta_endpoints_from(base: &std::path::Path) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let entries = std::fs::read_dir(base);
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

/// Resolve o binário `docker` fora do PATH mínimo do SSH não-interativo.
/// Ordem: PATH (`docker --version` ok) → /opt/homebrew/bin/docker (mac arm64)
/// → /usr/local/bin/docker (mac intel/OrbStack) → ~/.docker/bin/docker
/// (Docker Desktop) → /usr/bin/docker (linux). Fallback: `"docker"`.
pub fn resolve_docker_bin() -> PathBuf {
    if std::process::Command::new("docker")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
    {
        return PathBuf::from("docker");
    }
    let mut candidates: Vec<PathBuf> = vec![
        PathBuf::from("/opt/homebrew/bin/docker"),
        PathBuf::from("/usr/local/bin/docker"),
        PathBuf::from("/usr/bin/docker"),
    ];
    if let Some(home) = dirs::home_dir() {
        candidates.push(home.join(".docker/bin/docker"));
    }
    for c in candidates {
        if c.is_file() {
            return c;
        }
    }
    PathBuf::from("docker")
}

/// Descobre contextos no startup: `docker context ls` + meta.json.
/// Fallback: `["default"]` (unix socket) se o CLI falhar.
pub async fn discover(context_ls_timeout: Duration) -> DiscoveredContexts {
    let base: PathBuf = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("~"))
        .join(".docker/contexts/meta");
    discover_with(&resolve_docker_bin(), &base, context_ls_timeout).await
}

/// Variante testável com binário e base de meta explícitos (sem PATH/env global).
pub async fn discover_with(
    docker_bin: &std::path::Path,
    meta_base: &std::path::Path,
    context_ls_timeout: Duration,
) -> DiscoveredContexts {
    let meta = read_meta_endpoints_from(meta_base);
    let names = list_context_names(docker_bin, context_ls_timeout).await;
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

async fn list_context_names(docker_bin: &std::path::Path, timeout: Duration) -> Vec<String> {
    let attempt = || async {
        tokio::process::Command::new(docker_bin)
            .args(["context", "ls", "--format", "{{.Name}}"])
            .output()
            .await
    };
    // Probe de startup: 1 retry curto contra falha transiente de spawn (load).
    let mut last_err = String::new();
    for _ in 0..2 {
        match tokio::time::timeout(timeout, attempt()).await {
            Ok(Ok(out)) if out.status.success() => {
                let names: Vec<String> = String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .map(str::to_string)
                    .collect();
                if !names.is_empty() {
                    return names;
                }
                last_err = "saída vazia".to_string();
            }
            Ok(Ok(out)) => {
                last_err = format!(
                    "exit {}: {}",
                    out.status,
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
            Ok(Err(e)) => last_err = format!("spawn: {e:#}"),
            Err(_) => last_err = format!("timeout {}s", timeout.as_secs()),
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    eprintln!("warn: `docker context ls` falhou ({last_err}); usando [default]");
    vec!["default".to_string()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_meta(dir: &std::path::Path, hash: &str, body: &str) {
        let d = dir.join(hash);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("meta.json"), body).unwrap();
    }

    #[test]
    fn macos_context_endpoints_dispatch_to_expected_transport() {
        let cases = [
            ("default", "unix:///var/run/docker.sock", Transport::Api),
            (
                "desktop-linux",
                "unix:///Users/luizg/.docker/run/docker.sock",
                Transport::Api,
            ),
            ("homeserver", "ssh://raspberrypi-srv", Transport::Cli),
            ("oracle-aux", "ssh://oracle-aux", Transport::Cli),
            ("oracle-desktop", "ssh://oracle-desktop", Transport::Cli),
            ("oracle-home", "ssh://oracle-home", Transport::Cli),
            (
                "orbstack",
                "unix:///Users/luizg/.orbstack/run/docker.sock",
                Transport::Api,
            ),
        ];

        for (context, endpoint, expected) in cases {
            assert_eq!(
                decide_transport(endpoint),
                expected,
                "wrong transport for context {context}"
            );
        }
        assert_eq!(decide_transport("tcp://x:2375"), Transport::Cli);
    }

    #[test]
    fn badge_labels() {
        assert_eq!(Transport::Api.badge(), "api");
        assert_eq!(Transport::Cli.badge(), "cli");
    }

    #[test]
    fn discovered_contexts_expose_names_and_lookup() {
        let discovered = DiscoveredContexts {
            contexts: vec![
                DockerContext {
                    name: "default".into(),
                    endpoint: "unix:///var/run/docker.sock".into(),
                    transport: Transport::Api,
                },
                DockerContext {
                    name: "homeserver".into(),
                    endpoint: "ssh://raspberrypi-srv".into(),
                    transport: Transport::Cli,
                },
            ],
        };

        assert_eq!(
            discovered.names(),
            vec!["default".to_string(), "homeserver".to_string()]
        );
        assert_eq!(
            discovered
                .get("homeserver")
                .map(|context| context.endpoint.as_str()),
            Some("ssh://raspberrypi-srv")
        );
        assert!(discovered.get("missing").is_none());
    }

    #[test]
    fn docker_binary_resolution_never_returns_an_empty_path() {
        let resolved = resolve_docker_bin();
        assert!(!resolved.as_os_str().is_empty());
        assert!(resolved.as_os_str() == "docker" || resolved.is_file());
    }

    #[test]
    fn read_meta_endpoints_matches_existing_home_metadata() {
        let Some(home) = dirs::home_dir() else {
            return;
        };
        let expected = read_meta_endpoints_from(&home.join(".docker/contexts/meta"));
        if !expected.is_empty() {
            assert_eq!(read_meta_endpoints(), expected);
        }
    }

    #[test]
    fn default_endpoint_is_unix() {
        assert!(default_endpoint_for("default").starts_with("unix://"));
    }

    #[test]
    fn meta_from_dir_parses_hosts_and_skips_garbage() {
        let dir = std::env::temp_dir().join(format!("dtmeta-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        write_meta(
            &dir,
            "aaaa",
            r#"{"Name":"default","Endpoints":{"docker":{"Host":"unix:///var/run/docker.sock"}}}"#,
        );
        write_meta(
            &dir,
            "bbbb",
            r#"{"Name":"homeserver","Endpoints":{"docker":{"Host":"ssh://lgldsilva@192.168.0.100"}}}"#,
        );
        write_meta(&dir, "cccc", "not-json{{{");
        write_meta(&dir, "eeee", r#"{"Name":"no-endpoints"}"#);
        write_meta(&dir, "ffff", r#"{"Name":"no-docker","Endpoints":{}}"#);
        write_meta(
            &dir,
            "gggg",
            r#"{"Name":"no-host","Endpoints":{"docker":{}}}"#,
        );
        std::fs::create_dir_all(dir.join("dddd")).unwrap(); // sem meta.json
        let map = read_meta_endpoints_from(&dir);
        assert_eq!(
            map.get("default").map(String::as_str),
            Some("unix:///var/run/docker.sock")
        );
        assert_eq!(
            map.get("homeserver").map(String::as_str),
            Some("ssh://lgldsilva@192.168.0.100")
        );
        assert_eq!(map.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn meta_missing_dir_is_empty() {
        let map = read_meta_endpoints_from(std::path::Path::new("/nonexistent-dt-meta"));
        assert!(map.is_empty());
    }

    /// `discover_with()` com `docker` fake e meta temporária (sem rede/daemon).
    #[tokio::test]
    async fn discover_with_fake_docker() {
        let dir = std::env::temp_dir().join(format!("dtbin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("docker");
        let mut f = std::fs::File::create(&bin).unwrap();
        writeln!(f, "#!/bin/sh").unwrap();
        writeln!(f, "printf 'default\\nhomeserver\\nghost\\n'").unwrap();
        drop(f);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let meta = dir.join("meta");
        std::fs::create_dir_all(&meta).unwrap();
        write_meta(
            &meta,
            "aaaa",
            r#"{"Name":"default","Endpoints":{"docker":{"Host":"unix:///var/run/docker.sock"}}}"#,
        );
        write_meta(
            &meta,
            "bbbb",
            r#"{"Name":"homeserver","Endpoints":{"docker":{"Host":"ssh://lgldsilva@192.168.0.100"}}}"#,
        );
        let d = discover_with(&bin, &meta, Duration::from_secs(10)).await;
        // Se o fake falhou, mostra o porquê em vez de mascarar como fallback.
        if d.contexts.len() == 1 && d.contexts[0].name == "default" {
            let direct = std::process::Command::new(&bin).output();
            let via_tokio = tokio::process::Command::new(&bin)
                .args(["context", "ls", "--format", "{{.Name}}"])
                .output()
                .await;
            panic!("fake docker não executou (fallback): std={direct:?} tokio={via_tokio:?}");
        }
        assert_eq!(d.contexts.len(), 3);
        let by_name: std::collections::HashMap<_, _> = d
            .contexts
            .iter()
            .map(|c| (c.name.as_str(), c.transport))
            .collect();
        assert_eq!(by_name["default"], Transport::Api);
        assert_eq!(by_name["homeserver"], Transport::Cli);
        assert_eq!(by_name["ghost"], Transport::Cli);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn discover_entrypoint_returns_contexts_or_default_fallback() {
        let discovered = discover(Duration::from_secs(3)).await;
        assert!(!discovered.contexts.is_empty());
        assert!(!discovered.names().is_empty());
    }

    #[tokio::test]
    async fn failed_context_ls_status_uses_default_fallback() {
        let dir = std::env::temp_dir().join(format!("dtbin-failed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("docker");
        std::fs::write(
            &bin,
            r#"#!/bin/sh
printf 'ghost\n'
echo simulated context failure >&2
exit 42
"#,
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let discovered =
            discover_with(&bin, &dir.join("missing-meta"), Duration::from_secs(2)).await;
        assert_eq!(discovered.names(), vec!["default"]);
        assert_eq!(
            discovered.get("default").map(|context| context.transport),
            Some(Transport::Api)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn discover_fallback_without_binary() {
        let d = discover_with(
            std::path::Path::new("/nonexistent-dt-docker"),
            std::path::Path::new("/nonexistent-dt-meta"),
            Duration::from_secs(2),
        )
        .await;
        assert_eq!(d.contexts.len(), 1);
        assert_eq!(d.contexts[0].name, "default");
    }

    #[tokio::test]
    async fn empty_context_listing_retries_then_falls_back() {
        let dir = std::env::temp_dir().join(format!("dtbin-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("docker");
        std::fs::write(&bin, "#!/bin/sh\nprintf '\\n  \\n'\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let discovered =
            discover_with(&bin, &dir.join("missing-meta"), Duration::from_secs(2)).await;
        assert_eq!(discovered.names(), vec!["default"]);
        assert_eq!(discovered.contexts[0].transport, Transport::Api);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn context_listing_timeout_falls_back_without_hanging() {
        let dir = std::env::temp_dir().join(format!("dtbin-timeout-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("docker");
        std::fs::write(&bin, "#!/bin/sh\nsleep 2\nprintf 'late\\n'\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let discovered =
            discover_with(&bin, &dir.join("missing-meta"), Duration::from_millis(50)).await;
        assert_eq!(discovered.names(), vec!["default"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
