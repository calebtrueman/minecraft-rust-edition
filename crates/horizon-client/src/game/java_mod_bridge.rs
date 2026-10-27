//! Optional Java/Fabric helper for the game window.
//!
//! This is only a read-only tick feed. The helper sees a small player and
//! clock snapshot and may return a short status value.

use super::Game;
use horizon_client::text::Text;
use rust_edition_mod_bridge::{
    Bridge, BridgeError, BridgeStatus, ClockSnapshot, PlayerSnapshot, ResultStatus, Snapshot,
    SpawnConfig,
};
use std::{env, path::PathBuf};

const HELPER_ENV: &str = "HORIZON_MOD_BRIDGE_HELPER";
const CONFIG_ENV: &str = "HORIZON_MOD_BRIDGE_CONFIG";
const SCRATCH_ENV: &str = "HORIZON_MOD_BRIDGE_SCRATCH";

impl Game<'_> {
    /// Starts the optional helper only when all of its explicit settings exist.
    pub(super) fn start_java_mod_bridge(&mut self) {
        if !self.options.execution_role.is_client() {
            return;
        }
        let helper = env::var_os(HELPER_ENV);
        let config = env::var_os(CONFIG_ENV);
        let scratch = env::var_os(SCRATCH_ENV);
        match (helper, config) {
            (None, None) if scratch.is_none() => return,
            (Some(helper), Some(config)) => {
                let config = PathBuf::from(config);
                let scratch_dir = scratch
                    .map(PathBuf::from)
                    .unwrap_or_else(|| env::temp_dir());
                match Bridge::spawn(SpawnConfig::new(PathBuf::from(helper), config, scratch_dir)) {
                    Ok(bridge) => {
                        self.java_mod_bridge = Some(bridge);
                        self.java_mod_bridge_has_sent_snapshot = false;
                        self.report_java_mod_bridge_status(BridgeStatus::Starting);
                    }
                    Err(error) => self.report_java_mod_bridge_status(BridgeStatus::Failed(error)),
                }
            }
            _ => self.report_java_mod_bridge_status(BridgeStatus::Failed(
                BridgeError::InvalidConfig(format!(
                    "set both {HELPER_ENV} and {CONFIG_ENV}; {SCRATCH_ENV} is optional"
                )),
            )),
        }
    }

    /// Sends one read-only state snapshot after each ready helper tick.
    pub(super) fn tick_java_mod_bridge(&mut self) {
        if self.java_mod_bridge.is_none() {
            return;
        }
        self.poll_java_mod_bridge();
        let snapshot = self.java_mod_bridge_snapshot();
        self.send_java_mod_bridge_snapshot(snapshot);
    }

    /// Polls the worker outside game ticks, including while the window is at title.
    pub(super) fn poll_java_mod_bridge(&mut self) {
        if self.java_mod_bridge.is_none() {
            return;
        }
        let (status, replies) = {
            let Some(bridge) = self.java_mod_bridge.as_mut() else {
                return;
            };
            let mut replies = Vec::new();
            while let Some(reply) = bridge.try_recv() {
                replies.push(reply);
            }
            (bridge.status(), replies)
        };
        for reply in replies {
            if self.control.is_some() {
                let status = match reply.status {
                    ResultStatus::Ok => "ok",
                    ResultStatus::Failed => "failed",
                };
                self.events.java_mod_reply(
                    self.ticks,
                    reply.seq,
                    status,
                    reply.snapshot_json.clone(),
                    reply.value.clone(),
                );
            }
            if let Some(value) = reply.value {
                let line = format!("Java mod helper: {value}");
                match reply.status {
                    ResultStatus::Ok => log::debug!("{line}"),
                    ResultStatus::Failed => {
                        log::warn!("{line}");
                        self.system_message(Text::literal(line));
                    }
                }
            }
        }
        self.report_java_mod_bridge_status(status.clone());
    }

    /// Sends the current state once the helper is ready, even if the window is
    /// still at title. Pre-ready ticks are not replayed.
    pub(super) fn send_initial_java_mod_bridge_snapshot(&mut self) {
        if self.java_mod_bridge.is_none() || self.java_mod_bridge_has_sent_snapshot {
            return;
        }
        let ready = self.java_mod_bridge.as_ref().is_some_and(Bridge::is_ready);
        if !ready {
            return;
        }
        let snapshot = self.java_mod_bridge_snapshot();
        self.send_java_mod_bridge_snapshot(snapshot);
    }

    /// Queues one no-player snapshot when a world closes, if the helper is ready.
    /// If startup is still running, the outer loop sends the current state later.
    pub(super) fn close_java_mod_bridge_world(&mut self) {
        if self.java_mod_bridge.is_none() {
            return;
        }
        self.poll_java_mod_bridge();
        self.send_java_mod_bridge_snapshot(Snapshot::default());
    }

    fn send_java_mod_bridge_snapshot(&mut self, snapshot: Snapshot) {
        let Some(bridge) = self.java_mod_bridge.as_mut() else {
            return;
        };
        if !bridge.is_ready() {
            return;
        }
        let sent = match bridge.try_send(snapshot) {
            Ok(_) => true,
            Err(BridgeError::NotReady) => false,
            Err(error) => {
                log::warn!("Java mod bridge stopped: {error}");
                false
            }
        };
        let status = {
            let Some(bridge) = self.java_mod_bridge.as_ref() else {
                return;
            };
            bridge.status()
        };
        if sent {
            self.java_mod_bridge_has_sent_snapshot = true;
        }
        self.report_java_mod_bridge_status(status);
    }

    /// Stops I/O before the rest of the window exits.
    pub(super) fn stop_java_mod_bridge(&mut self) {
        if let Some(mut bridge) = self.java_mod_bridge.take() {
            bridge.shutdown();
        }
    }

    fn java_mod_bridge_snapshot(&self) -> Snapshot {
        let world_open = self.title.is_none();
        let player = if world_open {
            self.player.as_ref()
        } else {
            None
        }
        .map(|player| PlayerSnapshot {
            x: player.position.x,
            y: player.position.y,
            z: player.position.z,
            yaw: self.yaw,
            pitch: self.pitch,
            health: player.health.health(),
            dimension: self.dim.id().to_owned(),
        });
        let clock = player.as_ref().map(|_| ClockSnapshot {
            game_time: self.world.game_time,
            day_time: self.world.day_time,
        });
        Snapshot {
            world_open,
            player,
            clock,
        }
    }

    fn report_java_mod_bridge_status(&mut self, status: BridgeStatus) {
        if self.java_mod_bridge_reported.as_ref() == Some(&status) {
            return;
        }
        let line = match &status {
            BridgeStatus::Starting => "Java mod bridge is starting".to_owned(),
            BridgeStatus::Ready => "Java mod bridge is ready".to_owned(),
            BridgeStatus::Failed(error) => format!("Java mod bridge failed: {error}"),
            BridgeStatus::Stopped => "Java mod bridge stopped".to_owned(),
        };
        match &status {
            BridgeStatus::Starting | BridgeStatus::Ready | BridgeStatus::Stopped => {
                log::info!("{line}")
            }
            BridgeStatus::Failed(_) => log::warn!("{line}"),
        }
        self.system_message(Text::literal(line));
        self.java_mod_bridge_reported = Some(status);
    }
}
