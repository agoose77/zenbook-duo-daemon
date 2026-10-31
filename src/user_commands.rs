use std::{
    collections::HashMap,
    ffi::OsStr,
    sync::Arc,
    time::{Duration, Instant},
};

use log::{debug, warn};
use tokio::{process::Command, sync::Mutex};
use users::{get_user_by_uid, os::unix::UserExt};

const SESSION_CACHE_TTL: Duration = Duration::from_secs(10);

#[derive(Clone, Debug)]
pub struct UserSession {
    pub uid: u32,
    pub gid: u32,
    pub username: String,
    pub home: String,
}

#[derive(Debug)]
pub struct UserCommandOutput {
    pub stdout: String,
    pub stderr: String,
    pub success: bool,
}

#[derive(Clone, Default)]
pub struct UserCommandRunner {
    cached_session: Arc<Mutex<Option<CachedSession>>>,
}

struct CachedSession {
    session: UserSession,
    detected_at: Instant,
}

impl UserCommandRunner {
    pub async fn run_shell_command(&self, command: &str) -> Result<UserCommandOutput, String> {
        self.run_program("sh", &["-c".to_string(), command.to_string()])
            .await
    }

    pub async fn run_program(
        &self,
        program: &str,
        args: &[String],
    ) -> Result<UserCommandOutput, String> {
        let session = self.active_session().await?;
        run_as_session(&session, program, args).await
    }

    async fn active_session(&self) -> Result<UserSession, String> {
        {
            let cached = self.cached_session.lock().await;
            if let Some(cached) = cached.as_ref() {
                if cached.detected_at.elapsed() < SESSION_CACHE_TTL {
                    return Ok(cached.session.clone());
                }
            }
        }

        let session = detect_active_session()
            .await
            .ok_or_else(|| "Failed to detect an active graphical user session".to_string())?;

        let mut cached = self.cached_session.lock().await;
        *cached = Some(CachedSession {
            session: session.clone(),
            detected_at: Instant::now(),
        });
        Ok(session)
    }
}

async fn run_as_session(
    session: &UserSession,
    program: &str,
    args: &[String],
) -> Result<UserCommandOutput, String> {
    debug!(
        "Running user command as {} (uid {}): {} {:?}",
        session.username, session.uid, program, args
    );

    let env = merged_session_environment(session).await;
    let output = command_as_session(session, program, env)
        .args(args)
        .output()
        .await
        .map_err(|e| format!("Failed to run {} as {}: {}", program, session.username, e))?;

    Ok(UserCommandOutput {
        stdout: String::from_utf8_lossy(&output.stdout).trim().to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        success: output.status.success(),
    })
}

fn command_as_session(
    session: &UserSession,
    program: &str,
    env: HashMap<String, String>,
) -> Command {
    let mut command = Command::new(program);
    command
        .uid(session.uid)
        .gid(session.gid)
        .env_clear()
        .envs(env);
    command
}

async fn merged_session_environment(session: &UserSession) -> HashMap<String, String> {
    let user_systemd_env = user_systemd_environment(session).await.unwrap_or_default();
    let user_systemd_path = user_systemd_env.get("PATH").cloned();
    let mut env = session_environment(session, user_systemd_path);

    for (key, value) in user_systemd_env {
        if matches!(
            key.as_str(),
            "HOME"
                | "LOGNAME"
                | "SHELL"
                | "USER"
                | "XDG_RUNTIME_DIR"
                | "DBUS_SESSION_BUS_ADDRESS"
                | "PATH"
        ) {
            continue;
        }

        env.insert(key, value);
    }

    env
}

async fn user_systemd_environment(session: &UserSession) -> Option<HashMap<String, String>> {
    let output = command_as_session(session, "systemctl", session_environment(session, None))
        .args(["--user", "show-environment"])
        .output()
        .await
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Some(
        stdout
            .lines()
            .filter_map(|line| line.split_once('='))
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect(),
    )
}

fn session_environment(
    session: &UserSession,
    user_systemd_path: Option<String>,
) -> HashMap<String, String> {
    let mut env = HashMap::new();
    env.insert("USER".to_string(), session.username.clone());
    env.insert("LOGNAME".to_string(), session.username.clone());
    env.insert("HOME".to_string(), session.home.clone());
    env.insert("SHELL".to_string(), "/bin/sh".to_string());
    env.insert(
        "XDG_RUNTIME_DIR".to_string(),
        format!("/run/user/{}", session.uid),
    );
    env.insert(
        "DBUS_SESSION_BUS_ADDRESS".to_string(),
        format!("unix:path=/run/user/{}/bus", session.uid),
    );
    env.insert("PATH".to_string(), session_path(session, user_systemd_path));
    env
}

fn session_path(session: &UserSession, user_systemd_path: Option<String>) -> String {
    let mut paths = vec![
        format!("/etc/profiles/per-user/{}/bin", session.username),
        format!("{}/.nix-profile/bin", session.home),
        "/run/current-system/sw/bin".to_string(),
        "/nix/var/nix/profiles/default/bin".to_string(),
        "/usr/local/bin".to_string(),
        "/usr/bin".to_string(),
        "/bin".to_string(),
    ];

    if let Some(user_systemd_path) = user_systemd_path {
        paths.extend(
            user_systemd_path
                .split(':')
                .filter(|path| !path.is_empty())
                .map(ToOwned::to_owned),
        );
    }

    paths.join(":")
}

async fn detect_active_session() -> Option<UserSession> {
    if let Some(session) = detect_loginctl_session().await {
        return Some(session);
    }

    if let Some(session) = detect_sudo_user() {
        return Some(session);
    }

    detect_runtime_dir_user().await
}

async fn detect_loginctl_session() -> Option<UserSession> {
    let output = Command::new("loginctl")
        .args(["list-sessions", "--no-legend"])
        .output()
        .await
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut active_fallback = None;

    for line in stdout.lines() {
        let Some(session_id) = line.split_whitespace().next() else {
            continue;
        };

        let Some(properties) = loginctl_session_properties(session_id).await else {
            continue;
        };

        let is_active = properties.get("Active").map(String::as_str) == Some("yes");
        let uid = properties
            .get("User")
            .and_then(|value| value.parse::<u32>().ok())
            .filter(|uid| *uid != 0);

        if !is_active {
            continue;
        }

        let Some(uid) = uid else {
            continue;
        };

        let session = session_from_uid(uid)?;
        let session_type = properties.get("Type").map(String::as_str).unwrap_or("");
        if session_type == "wayland" || session_type == "x11" {
            return Some(session);
        }

        active_fallback.get_or_insert(session);
    }

    active_fallback
}

async fn loginctl_session_properties(session_id: &str) -> Option<HashMap<String, String>> {
    let output = Command::new("loginctl")
        .args([
            "show-session",
            session_id,
            "-p",
            "Active",
            "-p",
            "Type",
            "-p",
            "State",
            "-p",
            "User",
            "--no-pager",
        ])
        .output()
        .await
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Some(
        stdout
            .lines()
            .filter_map(|line| line.split_once('='))
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect(),
    )
}

fn detect_sudo_user() -> Option<UserSession> {
    std::env::var("SUDO_UID")
        .ok()
        .and_then(|uid| uid.parse::<u32>().ok())
        .filter(|uid| *uid != 0)
        .and_then(session_from_uid)
}

async fn detect_runtime_dir_user() -> Option<UserSession> {
    let mut entries = tokio::fs::read_dir("/run/user").await.ok()?;

    while let Ok(Some(entry)) = entries.next_entry().await {
        let Some(uid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
            .filter(|uid| *uid != 0)
        else {
            continue;
        };

        if tokio::fs::try_exists(format!("/run/user/{uid}/bus"))
            .await
            .unwrap_or(false)
        {
            if let Some(session) = session_from_uid(uid) {
                warn!(
                    "Falling back to /run/user session detection for {}",
                    session.username
                );
                return Some(session);
            }
        }
    }

    None
}

fn session_from_uid(uid: u32) -> Option<UserSession> {
    let user = get_user_by_uid(uid)?;
    let username = os_str_to_string(user.name())?;
    let home = user.home_dir().to_string_lossy().to_string();

    Some(UserSession {
        uid: user.uid(),
        gid: user.primary_group_id(),
        username,
        home,
    })
}

fn os_str_to_string(value: &OsStr) -> Option<String> {
    value.to_str().map(ToOwned::to_owned)
}
