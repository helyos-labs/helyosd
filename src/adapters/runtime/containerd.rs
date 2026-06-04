use std::collections::HashMap;
use std::path::PathBuf;

use async_trait::async_trait;
use futures::StreamExt;
use tokio::io::AsyncBufReadExt;
use tokio::process::Command;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use helyos_core::error::{HelyosError, Result};
use helyos_core::ports::runtime::*;

use super::cni::CniManager;
use super::log_tailer::LogTailer;

/// Container runtime adapter that delegates to the containerd `ctr` CLI.
///
/// All operations run against the `helyos` containerd namespace. Container logs
/// are stored under `{data_dir}/logs/{container_id}/` and streamed by
/// [`LogTailer`].
pub struct ContainerdRuntime {
    namespace: String,
    data_dir: PathBuf,
    cni: Mutex<CniManager>,
    /// container_id -> netns path
    netns_map: Mutex<HashMap<String, String>>,
    /// container_id -> (network_name, ip_address)
    network_map: Mutex<HashMap<String, (String, String)>>,
}

/// Normalize a short image reference into its fully qualified form.
///
/// - `"nginx"` → `"docker.io/library/nginx:latest"`
/// - `"nginx:1.25"` → `"docker.io/library/nginx:1.25"`
/// - `"myorg/myimg:v1"` → `"docker.io/myorg/myimg:v1"`
/// - `"ghcr.io/org/img:v1"` → `"ghcr.io/org/img:v1"` (unchanged)
pub fn normalize_image_ref(image: &str) -> String {
    // If the image reference contains a dot in the first segment (before any
    // colon) it already includes a registry host (e.g. `ghcr.io/…`,
    // `docker.io/…`).  We strip the tag portion first so that a version like
    // `nginx:1.25` isn't mistaken for a registry host due to the `.` in the
    // tag.
    let first_segment = image.split('/').next().unwrap_or(image);
    let host_part = first_segment.split(':').next().unwrap_or(first_segment);
    let has_registry = host_part.contains('.');

    if has_registry {
        // Already fully qualified — just ensure a tag is present.
        if image.contains(':') {
            return image.to_string();
        }
        return format!("{image}:latest");
    }

    // No registry. Split off the tag.
    let (name, tag) = match image.rsplit_once(':') {
        Some((n, t)) => (n, t.to_string()),
        None => (image, "latest".to_string()),
    };

    // A bare name like "nginx" → "docker.io/library/nginx".
    // A name with an org like "myorg/myimg" → "docker.io/myorg/myimg".
    if name.contains('/') {
        format!("docker.io/{name}:{tag}")
    } else {
        format!("docker.io/library/{name}:{tag}")
    }
}

/// Build the `ctr containers create` argument vector for a container config.
///
/// Pure helper extracted so argument construction (env/label/mount formatting)
/// can be unit-tested without invoking `ctr`. `normalized` is the
/// already-normalized image reference.
fn build_create_args(normalized: &str, config: &ContainerConfig) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "containers".to_string(),
        "create".to_string(),
        normalized.to_string(),
        config.name.clone(),
    ];

    // Environment variables.
    for (k, v) in &config.env {
        args.push("--env".to_string());
        args.push(format!("{k}={v}"));
    }

    // Labels.
    for (k, v) in &config.labels {
        args.push("--label".to_string());
        args.push(format!("{k}={v}"));
    }

    // Mount binds.
    for vol in &config.volumes {
        let opts = if vol.read_only {
            "rbind:ro"
        } else {
            "rbind:rw"
        };
        args.push("--mount".to_string());
        args.push(format!(
            "type=bind,src={},dst={},options={}",
            vol.source, vol.target, opts
        ));
    }

    args
}

/// Format a containerd log-uri (`file://<path>`) from a log file path.
///
/// Pure helper extracted so URI formatting can be tested without spawning
/// `ctr tasks start`.
fn log_uri(path: &std::path::Path) -> String {
    format!("file://{}", path.to_string_lossy())
}

impl ContainerdRuntime {
    /// Create a new runtime backed by the `ctr` CLI.
    ///
    /// `data_dir` is used for log storage and CNI configuration.
    pub fn new(data_dir: &str) -> Result<Self> {
        Ok(Self {
            namespace: "helyos".to_string(),
            data_dir: PathBuf::from(data_dir),
            cni: Mutex::new(CniManager::new(data_dir)),
            netns_map: Mutex::new(HashMap::new()),
            network_map: Mutex::new(HashMap::new()),
        })
    }

    /// Verify that containerd is reachable by invoking `ctr version`.
    pub async fn ping(&self) -> Result<()> {
        let output = Command::new("ctr")
            .args(["version"])
            .output()
            .await
            .map_err(|e| HelyosError::Runtime(format!("failed to run ctr: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(HelyosError::Runtime(format!(
                "containerd unreachable: {stderr}"
            )));
        }
        Ok(())
    }

    /// Run a `ctr` command in the configured namespace.
    async fn ctr(&self, args: &[&str]) -> Result<std::process::Output> {
        let output = Command::new("ctr")
            .arg("--namespace")
            .arg(&self.namespace)
            .args(args)
            .output()
            .await
            .map_err(|e| HelyosError::Runtime(format!("failed to run ctr: {e}")))?;
        Ok(output)
    }

    /// Run a `ctr` command and return an error if it fails.
    async fn ctr_ok(&self, args: &[&str]) -> Result<String> {
        let output = self.ctr(args).await?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(HelyosError::Runtime(format!(
                "ctr {} failed: {}",
                args.join(" "),
                stderr.trim()
            )));
        }
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    /// Directory where logs for a given container are stored.
    fn log_dir(&self, container_id: &str) -> PathBuf {
        self.data_dir.join("logs").join(container_id)
    }

    /// Path to the stdout log file for a container.
    fn stdout_log_path(&self, container_id: &str) -> PathBuf {
        self.log_dir(container_id).join("stdout.log")
    }

    /// Path to the stderr log file for a container.
    fn stderr_log_path(&self, container_id: &str) -> PathBuf {
        self.log_dir(container_id).join("stderr.log")
    }
}

#[async_trait]
impl ContainerRuntime for ContainerdRuntime {
    fn runtime_name(&self) -> &'static str {
        "containerd"
    }

    async fn pull_image(&self, image: &str) -> Result<()> {
        let normalized = normalize_image_ref(image);
        info!(image = %normalized, "pulling image via ctr");
        self.ctr_ok(&["images", "pull", &normalized]).await?;
        info!(image = %normalized, "image pulled");
        Ok(())
    }

    async fn create_container(&self, config: &ContainerConfig) -> Result<String> {
        let normalized = normalize_image_ref(&config.image);
        debug!(name = config.name, image = %normalized, "creating container via ctr");

        let args = build_create_args(&normalized, config);

        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        self.ctr_ok(&arg_refs).await?;

        // Create the log directory.
        let log_dir = self.log_dir(&config.name);
        tokio::fs::create_dir_all(&log_dir)
            .await
            .map_err(|e| HelyosError::Runtime(format!("create log dir: {e}")))?;

        info!(id = config.name, "container created");
        Ok(config.name.clone())
    }

    async fn start_container(&self, id: &str) -> Result<()> {
        debug!(id, "starting container task via ctr");

        // Ensure log files exist so containerd can write to them.
        let stdout_log = self.stdout_log_path(id);
        let stderr_log = self.stderr_log_path(id);
        for log_file in [&stdout_log, &stderr_log] {
            if let Some(parent) = log_file.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| HelyosError::Runtime(format!("create log dir: {e}")))?;
            }
            tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(log_file)
                .await
                .map_err(|e| {
                    HelyosError::Runtime(format!("create log file {}: {e}", log_file.display()))
                })?;
        }

        let stdout_uri = log_uri(&stdout_log);
        let stderr_uri = log_uri(&stderr_log);

        // Start the task and redirect stdout/stderr to log files via
        // containerd's log-uri mechanism.
        let output = Command::new("ctr")
            .arg("--namespace")
            .arg(&self.namespace)
            .args(["tasks", "start", "--detach"])
            .arg("--log-uri")
            .arg(&stdout_uri)
            .arg("--stderr-uri")
            .arg(&stderr_uri)
            .arg(id)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .await
            .map_err(|e| HelyosError::Runtime(format!("failed to run ctr tasks start: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(HelyosError::Runtime(format!(
                "ctr tasks start failed: {}",
                stderr.trim()
            )));
        }

        // Extract PID from stdout (ctr prints the task PID).
        let stdout = String::from_utf8_lossy(&output.stdout);
        let pid = stdout.trim();

        // Record the network namespace path derived from the task PID.
        if let Ok(pid_num) = pid.parse::<u64>() {
            let netns_path = format!("/proc/{pid_num}/ns/net");
            let mut netns = self.netns_map.lock().await;
            netns.insert(id.to_string(), netns_path.clone());
            debug!(id, netns = %netns_path, "recorded netns for container");
        } else {
            debug!(id, stdout = %pid, log = %stdout_uri, "could not parse PID from ctr output");
        }

        debug!(id, "container task started");
        Ok(())
    }

    async fn stop_container(&self, id: &str, timeout_secs: u64) -> Result<()> {
        debug!(id, timeout_secs, "stopping container via ctr");

        // Send SIGTERM first.
        let result = self
            .ctr(&["tasks", "kill", "--signal", "SIGTERM", id])
            .await?;
        if !result.status.success() {
            let stderr = String::from_utf8_lossy(&result.stderr);
            // Task might already be stopped — don't fail immediately.
            warn!(id, stderr = %stderr.trim(), "SIGTERM failed, task may already be stopped");
            return Ok(());
        }

        // Wait for the container to exit within the timeout.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
        loop {
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            let check = self.ctr(&["tasks", "list"]).await?;
            let stdout = String::from_utf8_lossy(&check.stdout);
            let still_running = stdout
                .lines()
                .any(|line| line.contains(id) && line.contains("RUNNING"));
            if !still_running {
                debug!(id, "container stopped after SIGTERM");
                return Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }

        // Timeout reached — send SIGKILL.
        warn!(id, "SIGTERM timed out, sending SIGKILL");
        self.ctr_ok(&["tasks", "kill", "--signal", "SIGKILL", id])
            .await?;
        debug!(id, "container killed with SIGKILL");
        Ok(())
    }

    async fn remove_container(&self, id: &str, force: bool) -> Result<()> {
        debug!(id, force, "removing container via ctr");

        // Delete the task first (if it exists).
        let task_result = self.ctr(&["tasks", "delete", id]).await?;
        if !task_result.status.success() {
            let stderr = String::from_utf8_lossy(&task_result.stderr);
            // Task may not exist — only warn.
            debug!(id, stderr = %stderr.trim(), "task delete returned non-zero (may be expected)");
        }

        // Delete the container.
        let mut args = vec!["containers", "delete"];
        if force {
            // No force flag in ctr containers delete, but we already cleaned the
            // task above. Just proceed.
        }
        args.push(id);
        self.ctr_ok(&args).await?;

        // Clean up in-memory state.
        {
            let mut netns = self.netns_map.lock().await;
            netns.remove(id);
        }
        {
            let mut nmap = self.network_map.lock().await;
            nmap.remove(id);
        }

        // Remove log directory.
        let log_dir = self.log_dir(id);
        if log_dir.exists() {
            let _ = tokio::fs::remove_dir_all(&log_dir).await;
        }

        debug!(id, "container removed");
        Ok(())
    }

    async fn inspect_container(&self, id: &str) -> Result<ContainerInfo> {
        // Get container info for metadata.
        let info_output = self.ctr_ok(&["containers", "info", id]).await?;

        // Parse image from container info output.
        let image = info_output
            .lines()
            .find(|line| line.contains("Image:") || line.contains("image:"))
            .and_then(|line| line.split_whitespace().last())
            .unwrap_or("unknown")
            .to_string();

        // Determine state from task list.
        let tasks_output = self.ctr(&["tasks", "list"]).await?;
        let tasks_stdout = String::from_utf8_lossy(&tasks_output.stdout);
        let state = tasks_stdout
            .lines()
            .find(|line| line.contains(id))
            .map(|line| {
                if line.contains("RUNNING") {
                    ContainerState::Running
                } else if line.contains("STOPPED") {
                    ContainerState::Exited
                } else if line.contains("PAUSED") {
                    ContainerState::Paused
                } else if line.contains("CREATED") {
                    ContainerState::Created
                } else {
                    ContainerState::Unknown
                }
            })
            .unwrap_or(ContainerState::Created); // Container exists but no task = Created

        Ok(ContainerInfo {
            id: id.to_string(),
            name: id.to_string(),
            image,
            state,
        })
    }

    async fn logs(&self, id: &str, tail: Option<u64>) -> Result<LogStream> {
        let log_path = self.stdout_log_path(id);
        let stream = LogTailer::tail(&log_path, tail).await?;
        Ok(Box::pin(stream))
    }

    async fn container_exists(&self, name: &str) -> Result<bool> {
        let output = self.ctr(&["containers", "info", name]).await?;
        Ok(output.status.success())
    }

    async fn create_network(&self, name: &str) -> Result<String> {
        let cni = self.cni.lock().await;
        cni.ensure_network(name)
            .map_err(|e| HelyosError::Runtime(format!("create network: {e}")))?;
        info!(name, "CNI network created");
        Ok(name.to_string())
    }

    async fn remove_network(&self, name: &str) -> Result<()> {
        let cni = self.cni.lock().await;
        cni.remove_network(name)
            .map_err(|e| HelyosError::Runtime(format!("remove network: {e}")))?;
        debug!(name, "CNI network removed");
        Ok(())
    }

    async fn connect_to_network(&self, container_id: &str, network: &str) -> Result<()> {
        let cni = self.cni.lock().await;
        let ip = cni
            .attach(container_id, network)
            .map_err(|e| HelyosError::Runtime(format!("CNI attach: {e}")))?;

        let mut nmap = self.network_map.lock().await;
        nmap.insert(
            container_id.to_string(),
            (network.to_string(), ip.to_string()),
        );
        debug!(container_id, network, ip = %ip, "connected to CNI network");
        Ok(())
    }

    async fn container_ip(&self, container_id: &str, network: &str) -> Result<String> {
        let nmap = self.network_map.lock().await;
        match nmap.get(container_id) {
            Some((net, ip)) if net == network => Ok(ip.clone()),
            _ => Err(HelyosError::Runtime(format!(
                "no IP found for container {container_id} on network {network}"
            ))),
        }
    }

    async fn events(&self) -> Result<EventStream> {
        let namespace = self.namespace.clone();

        let mut child = Command::new("ctr")
            .arg("--namespace")
            .arg(&namespace)
            .arg("events")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| HelyosError::Runtime(format!("failed to start ctr events: {e}")))?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| HelyosError::Runtime("no stdout from ctr events".to_string()))?;

        let reader = tokio::io::BufReader::new(stdout);
        let lines = tokio_stream::wrappers::LinesStream::new(reader.lines());

        let stream = lines.filter_map(move |line_result| {
            let _child_guard = &child;
            async move {
                let line = match line_result {
                    Ok(l) => l,
                    Err(e) => {
                        warn!(error = %e, "error reading ctr events");
                        return None;
                    }
                };

                // ctr events outputs lines like:
                //   <timestamp> <topic> <payload>
                // Topic patterns:
                //   /tasks/exit    → ContainerDied
                //   /tasks/start   → ContainerStarted
                //   /tasks/oom     → ContainerOom
                if line.contains("/tasks/exit") {
                    let container_id = extract_container_id(&line).unwrap_or_default();
                    let exit_code = extract_exit_code(&line).unwrap_or(-1);
                    Some(RuntimeEvent::ContainerDied {
                        container_id,
                        exit_code,
                    })
                } else if line.contains("/tasks/start") {
                    let container_id = extract_container_id(&line).unwrap_or_default();
                    Some(RuntimeEvent::ContainerStarted { container_id })
                } else if line.contains("/tasks/oom") {
                    let container_id = extract_container_id(&line).unwrap_or_default();
                    Some(RuntimeEvent::ContainerOom { container_id })
                } else {
                    None
                }
            }
        });

        Ok(Box::pin(stream))
    }
}

/// Best-effort extraction of the container ID from a `ctr events` line.
fn extract_container_id(line: &str) -> Option<String> {
    // The event payload usually contains an `id` field somewhere in the JSON or
    // protobuf text.  We try a simple heuristic: find `"container_id":"<value>"`
    // or just the second whitespace-delimited token after the topic.
    if let Some(start) = line.find("container_id") {
        let rest = &line[start..];
        // Try JSON-like: container_id":"value"
        let value = rest
            .split('"')
            .nth(2)
            .or_else(|| rest.split(':').nth(1).map(|s| s.trim_matches('"')));
        return value.map(|s| s.trim().to_string());
    }
    // Fallback: try splitting by whitespace and grabbing what looks like an ID.
    None
}

/// Best-effort extraction of the exit code from a `ctr events` line.
fn extract_exit_code(line: &str) -> Option<i64> {
    if let Some(start) = line.find("exit_status") {
        let rest = &line[start..];
        rest.split(|c: char| !c.is_ascii_digit() && c != '-')
            .find(|s| !s.is_empty())
            .and_then(|s| s.parse().ok())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(name: &str, image: &str) -> ContainerConfig {
        ContainerConfig {
            name: name.into(),
            image: image.into(),
            command: vec![],
            env: HashMap::new(),
            ports: vec![],
            volumes: vec![],
            labels: HashMap::new(),
            network: None,
            dns: vec![],
            dns_search: vec![],
        }
    }

    // ---- normalize_image_ref ----

    #[test]
    fn normalize_bare_image() {
        assert_eq!(
            normalize_image_ref("nginx"),
            "docker.io/library/nginx:latest"
        );
    }

    #[test]
    fn normalize_image_with_tag() {
        assert_eq!(
            normalize_image_ref("nginx:1.25"),
            "docker.io/library/nginx:1.25"
        );
    }

    #[test]
    fn normalize_image_with_org() {
        assert_eq!(
            normalize_image_ref("myorg/myimg:v1"),
            "docker.io/myorg/myimg:v1"
        );
    }

    #[test]
    fn normalize_image_with_registry() {
        assert_eq!(
            normalize_image_ref("ghcr.io/org/img:v1"),
            "ghcr.io/org/img:v1"
        );
    }

    #[test]
    fn normalize_docker_io_unchanged() {
        assert_eq!(
            normalize_image_ref("docker.io/library/nginx:latest"),
            "docker.io/library/nginx:latest"
        );
    }

    #[test]
    fn normalize_org_without_tag_gets_latest() {
        assert_eq!(
            normalize_image_ref("myorg/myimg"),
            "docker.io/myorg/myimg:latest"
        );
    }

    #[test]
    fn normalize_registry_without_tag_gets_latest() {
        assert_eq!(
            normalize_image_ref("ghcr.io/org/img"),
            "ghcr.io/org/img:latest"
        );
    }

    // ---- build_create_args ----

    #[test]
    fn create_args_have_fixed_prefix() {
        let args = build_create_args("docker.io/library/nginx:latest", &cfg("web", "nginx"));
        assert_eq!(
            &args[0..4],
            &[
                "containers",
                "create",
                "docker.io/library/nginx:latest",
                "web"
            ]
        );
    }

    #[test]
    fn create_args_include_env_pairs() {
        let mut c = cfg("web", "nginx");
        c.env.insert("FOO".into(), "bar".into());
        let args = build_create_args("img", &c);
        let pos = args.iter().position(|a| a == "--env").expect("--env flag");
        assert_eq!(args[pos + 1], "FOO=bar");
    }

    #[test]
    fn create_args_include_label_pairs() {
        let mut c = cfg("web", "nginx");
        c.labels.insert("app".into(), "api".into());
        let args = build_create_args("img", &c);
        let pos = args
            .iter()
            .position(|a| a == "--label")
            .expect("--label flag");
        assert_eq!(args[pos + 1], "app=api");
    }

    #[test]
    fn create_args_format_mount_options_by_readonly() {
        let mut c = cfg("web", "nginx");
        c.volumes = vec![
            VolumeBinding {
                source: "/data".into(),
                target: "/var/data".into(),
                read_only: false,
            },
            VolumeBinding {
                source: "/etc/conf".into(),
                target: "/conf".into(),
                read_only: true,
            },
        ];
        let args = build_create_args("img", &c);
        assert!(
            args.iter()
                .any(|a| a == "type=bind,src=/data,dst=/var/data,options=rbind:rw")
        );
        assert!(
            args.iter()
                .any(|a| a == "type=bind,src=/etc/conf,dst=/conf,options=rbind:ro")
        );
    }

    // ---- log_uri ----

    #[test]
    fn log_uri_prefixes_file_scheme() {
        assert_eq!(
            log_uri(std::path::Path::new("/var/log/c1/stdout.log")),
            "file:///var/log/c1/stdout.log"
        );
    }

    // ---- extract_container_id / extract_exit_code ----

    #[test]
    fn extract_container_id_from_json_payload() {
        let line = r#"2024-01-01 /tasks/exit {"container_id":"abc123","exit_status":137}"#;
        assert_eq!(extract_container_id(line), Some("abc123".to_string()));
    }

    #[test]
    fn extract_container_id_absent_returns_none() {
        assert_eq!(extract_container_id("2024 /tasks/start {}"), None);
    }

    #[test]
    fn extract_exit_code_from_json_payload() {
        let line = r#"2024-01-01 /tasks/exit {"container_id":"abc123","exit_status":137}"#;
        assert_eq!(extract_exit_code(line), Some(137));
    }

    #[test]
    fn extract_exit_code_absent_returns_none() {
        assert_eq!(extract_exit_code("2024 /tasks/start {}"), None);
    }

    // ---- path builders (offline; ContainerdRuntime::new does no I/O) ----

    #[test]
    fn log_paths_are_under_data_dir() {
        let dir = tempfile::tempdir().unwrap();
        let rt = ContainerdRuntime::new(dir.path().to_str().unwrap()).unwrap();
        assert_eq!(rt.log_dir("c1"), dir.path().join("logs").join("c1"));
        assert_eq!(
            rt.stdout_log_path("c1"),
            dir.path().join("logs").join("c1").join("stdout.log")
        );
        assert_eq!(
            rt.stderr_log_path("c1"),
            dir.path().join("logs").join("c1").join("stderr.log")
        );
    }
}
