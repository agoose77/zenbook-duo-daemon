use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use log::{debug, warn};
use tokio::sync::Mutex;

use crate::{config::TouchpadConfig, user_commands::UserCommandRunner};

const MIN_TOP_SEEK_BATCH_SECONDS: f64 = 3.0;

#[derive(Clone)]
pub struct MprisTracker {
    runner: UserCommandRunner,
    state: Arc<Mutex<MprisState>>,
}

#[derive(Default)]
struct MprisState {
    players: HashMap<String, PlayerSnapshot>,
    target_player: Option<String>,
    pending_top_seek_player: Option<String>,
    pending_top_seek_seconds: f64,
}

#[derive(Clone, Default)]
struct PlayerSnapshot {
    status: Option<String>,
    position: Option<f64>,
    length_seconds: Option<f64>,
}

impl MprisTracker {
    pub fn start(config: &TouchpadConfig, runner: UserCommandRunner) -> Self {
        let tracker = Self {
            runner,
            state: Arc::new(Mutex::new(MprisState::default())),
        };

        tracker.spawn_player_list_task(config.player_list_interval_seconds);
        tracker.spawn_player_poll_task(config.player_position_interval_seconds);
        tracker
    }

    pub async fn seek_by_seconds(&self, seconds: f64) {
        let Some(target) = self.target_player().await else {
            debug!("Ignoring seek request because there is no target MPRIS player");
            return;
        };

        self.seek_relative(&target, seconds).await;
    }

    pub async fn seek_by_fraction_and_speed(&self, fraction: f64, speed: f64) {
        let Some(target) = self.target_player().await else {
            debug!("Ignoring top-border seek because there is no target MPRIS player");
            return;
        };

        let Some(length) = self.player_length_seconds(&target).await else {
            debug!(
                "Ignoring top-border seek because {} has no track length",
                target
            );
            return;
        };

        let speed_multiplier = speed.abs().clamp(0.25, 3.0);
        let offset = length * fraction * speed_multiplier;
        if offset.abs() < 1.0 {
            return;
        }

        self.seek_top_batched(&target, offset).await;
    }

    pub async fn next(&self) {
        self.player_command("next").await;
    }

    pub async fn previous(&self) {
        self.player_command("previous").await;
    }

    #[allow(dead_code)]
    pub async fn play_pause(&self) {
        self.player_command("play-pause").await;
    }

    fn spawn_player_list_task(&self, interval_seconds: u64) {
        let tracker = self.clone();
        tokio::spawn(async move {
            let interval_seconds = interval_seconds.max(1);

            loop {
                tracker.refresh_players().await;
                tokio::time::sleep(Duration::from_secs(interval_seconds)).await;
            }
        });
    }

    fn spawn_player_poll_task(&self, interval_seconds: u64) {
        let tracker = self.clone();
        tokio::spawn(async move {
            let interval_seconds = interval_seconds.max(1);

            loop {
                tracker.poll_players().await;
                tokio::time::sleep(Duration::from_secs(interval_seconds)).await;
            }
        });
    }

    async fn refresh_players(&self) {
        let output = self.playerctl(&["-l"]).await;
        let players: Vec<String> = output
            .filter(|output| output.success)
            .map(|output| {
                output
                    .stdout
                    .lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                    .map(ToOwned::to_owned)
                    .collect()
            })
            .unwrap_or_default();

        let player_set: HashSet<String> = players.into_iter().collect();
        let mut state = self.state.lock().await;
        state
            .players
            .retain(|player, _snapshot| player_set.contains(player));
        for player in player_set {
            state.players.entry(player).or_default();
        }

        if let Some(target) = state.target_player.as_ref() {
            if !state.players.contains_key(target) {
                state.target_player = None;
            }
        }
    }

    async fn poll_players(&self) {
        let players: Vec<String> = {
            let state = self.state.lock().await;
            state.players.keys().cloned().collect()
        };

        for player in players {
            let status = self.player_status(&player).await;
            let position = self.player_position(&player).await;
            let length_seconds = self.query_player_length_seconds(&player).await;

            let mut state = self.state.lock().await;
            let previous_status = state
                .players
                .get(&player)
                .and_then(|snapshot| snapshot.status.clone());
            let status_changed = previous_status.is_some() && previous_status != status;

            if let Some(snapshot) = state.players.get_mut(&player) {
                snapshot.status = status.clone();
                snapshot.position = position;
                if length_seconds.is_some() {
                    snapshot.length_seconds = length_seconds;
                }
            }

            if status_changed {
                debug!("MPRIS target changed to {} after status change", player);
                state.target_player = Some(player.clone());
            } else if state.target_player.is_none() && status.as_deref() == Some("Playing") {
                debug!(
                    "MPRIS target initialized to currently playing player {}",
                    player
                );
                state.target_player = Some(player.clone());
            }
        }
    }

    async fn target_player(&self) -> Option<String> {
        let state = self.state.lock().await;
        if let Some(target) = state.target_player.as_ref() {
            return Some(target.clone());
        }

        state.players.iter().find_map(|(player, snapshot)| {
            (snapshot.status.as_deref() == Some("Playing")).then(|| player.clone())
        })
    }

    async fn player_command(&self, command: &str) {
        let Some(target) = self.target_player().await else {
            debug!(
                "Ignoring {} because there is no target MPRIS player",
                command
            );
            return;
        };

        self.playerctl(&["-p", &target, command]).await;
    }

    async fn seek_relative(&self, player: &str, seconds: f64) {
        let offset = format_relative_position_offset(seconds);
        self.playerctl(&["-p", player, "position", &offset]).await;
    }

    async fn seek_top_batched(&self, player: &str, seconds: f64) {
        let Some(offset) = self.accumulate_top_seek(player, seconds).await else {
            return;
        };

        self.seek_relative(player, offset).await;
    }

    async fn accumulate_top_seek(&self, player: &str, seconds: f64) -> Option<f64> {
        let mut state = self.state.lock().await;
        if state.pending_top_seek_player.as_deref() != Some(player) {
            state.pending_top_seek_player = Some(player.to_string());
            state.pending_top_seek_seconds = 0.0;
        }

        state.pending_top_seek_seconds += seconds;
        if state.pending_top_seek_seconds.abs() < MIN_TOP_SEEK_BATCH_SECONDS {
            return None;
        }

        Some(std::mem::take(&mut state.pending_top_seek_seconds))
    }

    async fn player_status(&self, player: &str) -> Option<String> {
        self.playerctl(&["-p", player, "status"])
            .await
            .filter(|output| output.success)
            .map(|output| output.stdout)
            .filter(|value| !value.is_empty())
    }

    async fn player_position(&self, player: &str) -> Option<f64> {
        self.playerctl(&["-p", player, "position"])
            .await
            .filter(|output| output.success)
            .and_then(|output| output.stdout.parse::<f64>().ok())
    }

    async fn player_length_seconds(&self, player: &str) -> Option<f64> {
        if let Some(length_seconds) = self.query_player_length_seconds(player).await {
            let mut state = self.state.lock().await;
            if let Some(snapshot) = state.players.get_mut(player) {
                snapshot.length_seconds = Some(length_seconds);
            }
            return Some(length_seconds);
        }

        let state = self.state.lock().await;
        let cached_length = state
            .players
            .get(player)
            .and_then(|snapshot| snapshot.length_seconds);
        if cached_length.is_some() {
            debug!("Using cached MPRIS length for {}", player);
        }
        cached_length
    }

    async fn query_player_length_seconds(&self, player: &str) -> Option<f64> {
        self.playerctl(&["-p", player, "metadata", "mpris:length"])
            .await
            .filter(|output| output.success)
            .and_then(|output| output.stdout.parse::<f64>().ok())
            .map(|microseconds| (microseconds / 1_000_000.0).floor())
            .filter(|seconds| *seconds > 0.0)
    }

    async fn playerctl(&self, args: &[&str]) -> Option<crate::user_commands::UserCommandOutput> {
        let args = args.iter().map(|arg| arg.to_string()).collect::<Vec<_>>();
        debug!("playerctl {}", format_playerctl_command(&args));
        match self.runner.run_program("playerctl", &args).await {
            Ok(output) => {
                if !output.success && !output.stderr.is_empty() {
                    debug!("playerctl {:?} failed: {}", args, output.stderr);
                }
                Some(output)
            }
            Err(e) => {
                warn!("{}", e);
                None
            }
        }
    }
}

fn format_playerctl_command(args: &[String]) -> String {
    args.iter()
        .map(|arg| {
            if arg.contains(' ') {
                format!("\"{arg}\"")
            } else {
                arg.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn format_relative_position_offset(seconds: f64) -> String {
    let direction = if seconds.is_sign_negative() { '-' } else { '+' };
    let seconds = seconds.abs().round().max(1.0);
    format!("{seconds:.0}{direction}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_playerctl_relative_position_offsets() {
        assert_eq!(format_relative_position_offset(10.2), "10+");
        assert_eq!(format_relative_position_offset(-9.8), "10-");
        assert_eq!(format_relative_position_offset(0.2), "1+");
    }

    #[test]
    fn formats_playerctl_command_for_debug_output() {
        assert_eq!(
            format_playerctl_command(&[
                "-p".to_string(),
                "firefox".to_string(),
                "position".to_string(),
                "10+".to_string(),
            ]),
            "-p firefox position 10+"
        );
        assert_eq!(
            format_playerctl_command(&[
                "-p".to_string(),
                "my player".to_string(),
                "next".to_string(),
            ]),
            "-p \"my player\" next"
        );
    }

    #[tokio::test]
    async fn batches_top_seek_offsets_until_minimum_skip() {
        let tracker = MprisTracker {
            runner: UserCommandRunner::default(),
            state: Arc::new(Mutex::new(MprisState::default())),
        };

        assert_eq!(tracker.accumulate_top_seek("firefox", 1.2).await, None);
        assert_eq!(tracker.accumulate_top_seek("firefox", 1.7).await, None);
        assert_eq!(tracker.accumulate_top_seek("firefox", 0.2).await, Some(3.1));
        assert_eq!(tracker.accumulate_top_seek("firefox", -2.0).await, None);
        assert_eq!(
            tracker.accumulate_top_seek("firefox", -1.0).await,
            Some(-3.0)
        );
    }
}
