//! Headless multiplayer client for `tools/test_dedicated_server_parity.py`:
//! with `--server` and `HORIZON_NET_REPORT` set, the client joins without a
//! window, waits until it stands in the world with the chunks around it,
//! sends a chat message and `/list`, collects the chat it is shown, leaves
//! and writes a JSON report (position, chunks, chat lines, whether the
//! server kept it connected throughout). With `HORIZON_NET_QUIET` (lines
//! split by `;`) it then sends them as one batch of a mod's quiet commands
//! (`horizon_protocol::quiet`, `tools/quiet_commands_e2e.py`) and waits two
//! seconds for any chat they bring. The opt-in secure-chat run uses
//! `HORIZON_NET_SECURE_CHAT_COUNT` (21–64, usually 25) and an optional
//! `HORIZON_NET_SECURE_CHAT_INTERVAL_MS` (50–1000, default 250). It sends
//! spaced signed lines, then a signed `/tell` to itself, and reports echoes,
//! standalone `ChatAck` packets, and offsets carried in outgoing chat.
use super::{Game, remote::ChatSendEvidence};
use horizon_client::Result;
use serde_json::json;
use std::{
    collections::HashSet,
    path::Path,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const STRESS_COUNT_ENV: &str = "HORIZON_NET_SECURE_CHAT_COUNT";
const STRESS_INTERVAL_ENV: &str = "HORIZON_NET_SECURE_CHAT_INTERVAL_MS";

#[derive(Clone, Copy)]
struct SecureChatStressConfig {
    count: usize,
    interval: Duration,
}

impl SecureChatStressConfig {
    fn from_env() -> std::result::Result<Option<Self>, String> {
        let count = std::env::var(STRESS_COUNT_ENV).ok();
        let interval = std::env::var(STRESS_INTERVAL_ENV).ok();
        parse_stress_config(count.as_deref(), interval.as_deref())
    }

    fn work_budget(self) -> Duration {
        Duration::from_secs(45) + self.interval * self.count as u32
    }
}

fn parse_stress_config(
    count: Option<&str>,
    interval_ms: Option<&str>,
) -> std::result::Result<Option<SecureChatStressConfig>, String> {
    let Some(count) = count else {
        return if interval_ms.is_some() {
            Err(format!("{STRESS_INTERVAL_ENV} needs {STRESS_COUNT_ENV}"))
        } else {
            Ok(None)
        };
    };
    let count = count
        .parse::<usize>()
        .ok()
        .filter(|n| (21..=64).contains(n))
        .ok_or_else(|| format!("{STRESS_COUNT_ENV} must be between 21 and 64"))?;
    let interval = interval_ms
        .unwrap_or("250")
        .parse::<u64>()
        .ok()
        .filter(|ms| (50..=1_000).contains(ms))
        .ok_or_else(|| format!("{STRESS_INTERVAL_ENV} must be between 50 and 1000"))?;
    Ok(Some(SecureChatStressConfig {
        count,
        interval: Duration::from_millis(interval),
    }))
}

struct SecureChatStressRun {
    config: SecureChatStressConfig,
    prefix: String,
    command_marker: String,
    message_markers: Vec<String>,
    sent: usize,
    signed: usize,
    echoed: HashSet<usize>,
    ack_packets: usize,
    last_seen_offsets: Vec<i32>,
    next_send: Instant,
    deadline: Option<Instant>,
    command_sent: bool,
    command_argument_names: Vec<String>,
    signed_command_arguments: usize,
    command_last_seen_offset: Option<i32>,
    command_echoed: bool,
}

impl SecureChatStressRun {
    fn new(config: SecureChatStressConfig) -> Self {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let prefix = format!("horizon-secure-{}-{now_ms}", std::process::id());
        Self {
            config,
            command_marker: format!("{prefix}-tell"),
            message_markers: (0..config.count)
                .map(|i| format!("{prefix}-chat-{i:02}"))
                .collect(),
            prefix,
            sent: 0,
            signed: 0,
            echoed: HashSet::new(),
            ack_packets: 0,
            last_seen_offsets: Vec::new(),
            next_send: Instant::now(),
            deadline: None,
            command_sent: false,
            command_argument_names: Vec::new(),
            signed_command_arguments: 0,
            command_last_seen_offset: None,
            command_echoed: false,
        }
    }

    fn report(&self) -> serde_json::Value {
        json!({
            "requested_messages": self.config.count,
            "messages_sent": self.sent,
            "signed_messages": self.signed,
            "message_echoes": self.echoed.len(),
            "interval_ms": self.config.interval.as_millis(),
            "standalone_chat_ack_packets_sent": self.ack_packets,
            "outgoing_chat_last_seen_offsets": self.last_seen_offsets,
            "outgoing_chat_acknowledged_messages": self
                .last_seen_offsets
                .iter()
                .map(|offset| i64::from(*offset))
                .sum::<i64>()
                + i64::from(self.command_last_seen_offset.unwrap_or(0)),
            "message_prefix": self.prefix,
            "command": {
                "target": "self",
                "message_marker": self.command_marker,
                "sent": self.command_sent,
                "argument_names": self.command_argument_names,
                "signed_argument_count": self.signed_command_arguments,
                "last_seen_offset": self.command_last_seen_offset,
                "reply_echoed": self.command_echoed,
            },
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StressStep {
    WaitingForWorld,
    SendingMessages,
    WaitingForMessages,
    SendingCommand,
    WaitingForCommand,
    Done,
}

impl Game<'_> {
    pub(super) fn run_net_harness(&mut self, report: &Path) -> Result<()> {
        let stress = match SecureChatStressConfig::from_env() {
            Ok(stress) => stress,
            Err(error) => {
                if self.remote.is_some() {
                    self.leave_server(None)?;
                }
                std::fs::write(
                    report,
                    json!({ "error": format!("invalid secure chat stress settings: {error}") })
                        .to_string(),
                )?;
                return Ok(());
            }
        };
        if let Some(config) = stress {
            return self.run_secure_chat_stress_harness(report, config);
        }
        self.run_default_net_harness(report)
    }

    fn run_secure_chat_stress_harness(
        &mut self,
        report: &Path,
        config: SecureChatStressConfig,
    ) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(120) + config.work_budget();
        let mut next_tick = Instant::now();
        let mut spawned_at = None;
        let mut quiet_since = Instant::now();
        let mut last_chunks = 0;
        let mut position = None;
        let mut chunks = 0;
        let mut stress = SecureChatStressRun::new(config);
        let mut step = StressStep::WaitingForWorld;
        let mut error = None;

        while Instant::now() < deadline {
            self.update_world();
            if self.remote.is_none() {
                error = Some(match self.title.as_ref().map(|t| &t.mode) {
                    Some(horizon_client::title::Mode::Multiplayer(
                        horizon_client::multiplayer::Screen::Disconnected { reason, .. },
                    )) => format!("disconnected: {reason}"),
                    _ => "disconnected".to_owned(),
                });
                break;
            }
            if Instant::now() >= next_tick {
                next_tick += Duration::from_millis(50);
                self.tick()?;
            }
            let now = Instant::now();
            if let Some(p) = &self.player {
                spawned_at.get_or_insert(now);
                position.get_or_insert([p.position.x, p.position.y, p.position.z]);
            }
            let chunk_count = self.world.chunks.len();
            if chunk_count != last_chunks {
                last_chunks = chunk_count;
                quiet_since = now;
            }
            let lines: Vec<String> = self
                .remote
                .as_ref()
                .map(|r| r.chat.iter().map(|(line, _)| line.clone()).collect())
                .unwrap_or_default();
            for (index, marker) in stress.message_markers.iter().enumerate().take(stress.sent) {
                if lines.iter().any(|line| line.contains(marker)) {
                    stress.echoed.insert(index);
                }
            }
            if stress.command_sent
                && lines
                    .iter()
                    .any(|line| line.contains(&stress.command_marker))
            {
                stress.command_echoed = true;
            }
            stress.ack_packets = self
                .remote
                .as_ref()
                .map_or(stress.ack_packets, |r| r.session.chat_ack_count());

            match step {
                StressStep::WaitingForWorld
                    if spawned_at.is_some()
                        && chunk_count > 0
                        && now - quiet_since > Duration::from_secs(3) =>
                {
                    chunks = chunk_count;
                    stress.deadline = Some(now + config.work_budget());
                    stress.next_send = now;
                    step = StressStep::SendingMessages;
                }
                StressStep::SendingMessages => {
                    if stress.deadline.is_some_and(|limit| now >= limit) {
                        error = Some(format!(
                            "timed out while sending secure-chat messages: {} of {} sent",
                            stress.sent, config.count
                        ));
                        break;
                    }
                    if now >= stress.next_send {
                        let marker = stress.message_markers[stress.sent].clone();
                        match self.send_chat_with_evidence(&marker) {
                            Some(ChatSendEvidence::Message {
                                signed,
                                last_seen_offset,
                            }) => {
                                stress.sent += 1;
                                stress.last_seen_offsets.push(last_seen_offset);
                                if signed {
                                    stress.signed += 1;
                                } else {
                                    error = Some(format!(
                                        "secure-chat message {} of {} was sent without a signature",
                                        stress.sent, config.count
                                    ));
                                    break;
                                }
                                stress.next_send = now + config.interval;
                                if stress.sent == config.count {
                                    step = StressStep::WaitingForMessages;
                                }
                            }
                            _ => {
                                error = Some(format!(
                                    "secure-chat message {} of {} could not be sent",
                                    stress.sent + 1,
                                    config.count
                                ));
                                break;
                            }
                        }
                    }
                }
                StressStep::WaitingForMessages if stress.echoed.len() == config.count => {
                    step = StressStep::SendingCommand;
                }
                StressStep::WaitingForMessages
                    if stress.deadline.is_some_and(|limit| now >= limit) =>
                {
                    error = Some(format!(
                        "timed out waiting for chat echoes: {} of {} arrived",
                        stress.echoed.len(),
                        config.count
                    ));
                    break;
                }
                StressStep::SendingCommand => {
                    let Some(target) = self.remote.as_ref().map(|r| r.profile_name.clone()) else {
                        error = Some("could not read the joined player's name for /tell".into());
                        break;
                    };
                    let command = format!("/tell {target} {}", stress.command_marker);
                    match self.send_chat_with_evidence(&command) {
                        Some(ChatSendEvidence::Command {
                            argument_names,
                            signed_arguments,
                            last_seen_offset,
                        }) => {
                            stress.command_sent = true;
                            stress.command_argument_names = argument_names;
                            stress.signed_command_arguments = signed_arguments;
                            stress.command_last_seen_offset = last_seen_offset;
                            if stress.command_argument_names != ["message"]
                                || stress.signed_command_arguments != 1
                            {
                                error = Some(format!(
                                    "/tell to self did not sign exactly its message argument (names: {:?}, signatures: {})",
                                    stress.command_argument_names, stress.signed_command_arguments
                                ));
                                break;
                            }
                            step = StressStep::WaitingForCommand;
                        }
                        _ => {
                            error = Some("self-targeted /tell could not be sent".into());
                            break;
                        }
                    }
                }
                StressStep::WaitingForCommand if stress.command_echoed => {
                    step = StressStep::Done;
                    break;
                }
                StressStep::WaitingForCommand
                    if stress.deadline.is_some_and(|limit| now >= limit) =>
                {
                    error = Some("timed out waiting for the signed self-/tell reply".into());
                    break;
                }
                StressStep::Done => break,
                _ => {}
            }
            std::thread::sleep(Duration::from_millis(5));
        }

        if step != StressStep::Done && error.is_none() {
            error = Some(match step {
                StressStep::WaitingForWorld => {
                    "timed out waiting for spawn and stable chunks".into()
                }
                StressStep::SendingMessages => format!(
                    "timed out while sending secure-chat messages: {} of {} sent",
                    stress.sent, config.count
                ),
                StressStep::WaitingForMessages => format!(
                    "timed out waiting for chat echoes: {} of {} arrived",
                    stress.echoed.len(),
                    config.count
                ),
                StressStep::SendingCommand => {
                    "timed out before sending the self-/tell command".into()
                }
                StressStep::WaitingForCommand => {
                    "timed out waiting for the signed self-/tell reply".into()
                }
                StressStep::Done => unreachable!(),
            });
        }
        stress.ack_packets = self
            .remote
            .as_ref()
            .map_or(stress.ack_packets, |r| r.session.chat_ack_count());
        let chat: Vec<String> = self
            .remote
            .as_ref()
            .map(|r| r.chat.iter().map(|(line, _)| line.clone()).collect())
            .unwrap_or_default();
        if self.remote.is_some() {
            self.leave_server(None)?;
        }
        let connected_throughout = error.is_none();
        std::fs::write(
            report,
            json!({
                "position": position,
                "chunks": chunks,
                "chat": chat,
                "connected_throughout": connected_throughout,
                "secure_chat_stress": stress.report(),
                "error": error,
            })
            .to_string(),
        )?;
        Ok(())
    }

    fn run_default_net_harness(&mut self, report: &Path) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(120);
        let mut next = Instant::now();
        let mut spawned_at: Option<Instant> = None;
        let mut last_chunks = 0;
        let mut quiet_since = Instant::now();
        let mut step = 0;
        let mut step_at = Instant::now();
        let mut chat_at = 0;
        let mut position = None;
        let quiet: Vec<String> = std::env::var("HORIZON_NET_QUIET")
            .map(|v| v.split(';').map(str::to_owned).collect())
            .unwrap_or_default();
        let mut chunks = 0;
        let mut error = None;
        while Instant::now() < deadline {
            self.update_world();
            if self.remote.is_none() {
                error = Some(match self.title.as_ref().map(|t| &t.mode) {
                    Some(horizon_client::title::Mode::Multiplayer(
                        horizon_client::multiplayer::Screen::Disconnected { reason, .. },
                    )) => format!("disconnected: {reason}"),
                    _ => "disconnected".to_owned(),
                });
                break;
            }
            if Instant::now() >= next {
                next += Duration::from_millis(50);
                self.tick()?;
            }
            let now = Instant::now();
            if let Some(p) = &self.player {
                spawned_at.get_or_insert(now);
                position.get_or_insert([p.position.x, p.position.y, p.position.z]);
            }
            let count = self.world.chunks.len();
            if count != last_chunks {
                last_chunks = count;
                quiet_since = now;
            }
            // Each step waits for what it sent to be answered (its chat line,
            // the /list reply), however busy the machine, up to 15 s.
            let shown = self.remote.as_ref().map_or(0, |r| r.chat.len());
            let answered = shown > chat_at || now - step_at > Duration::from_secs(15);
            match step {
                0 if spawned_at.is_some()
                    && count > 0
                    && now - quiet_since > Duration::from_secs(3) =>
                {
                    chunks = count;
                    self.send_chat("hello from the horizon client");
                    step = 1;
                    step_at = now;
                    chat_at = shown;
                }
                1 if answered => {
                    self.send_chat("/list");
                    step = 2;
                    step_at = now;
                    chat_at = shown;
                }
                2 if answered && !quiet.is_empty() => {
                    for data in horizon_protocol::quiet::encode("net_harness", &quiet) {
                        self.send(horizon_protocol::packets::play::Serverbound::CustomPayload(
                            horizon_protocol::packets::configuration::CustomPayload {
                                channel: horizon_protocol::quiet::CHANNEL.into(),
                                data,
                            },
                        ));
                    }
                    step = 4;
                    step_at = now;
                }
                4 if now - step_at > Duration::from_secs(2) => {
                    step = 3;
                    break;
                }
                2 if answered => {
                    step = 3;
                    break;
                }
                _ => {}
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        if step < 3 && error.is_none() {
            error = Some(format!("timed out at step {step}"));
        }
        let chat: Vec<String> = self
            .remote
            .as_ref()
            .map(|r| r.chat.iter().map(|(t, _)| t.clone()).collect())
            .unwrap_or_default();
        if self.remote.is_some() {
            self.leave_server(None)?;
        }
        std::fs::write(
            report,
            json!({
                "position": position,
                "chunks": chunks,
                "chat": chat,
                "error": error,
            })
            .to_string(),
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::parse_stress_config;
    use std::time::Duration;

    #[test]
    fn secure_chat_stress_settings_are_opt_in_and_bounded() {
        assert!(parse_stress_config(None, None).unwrap().is_none());
        let default = parse_stress_config(Some("25"), None).unwrap().unwrap();
        assert_eq!(default.count, 25);
        assert_eq!(default.interval, Duration::from_millis(250));
        let custom = parse_stress_config(Some("21"), Some("1000"))
            .unwrap()
            .unwrap();
        assert_eq!(custom.count, 21);
        assert_eq!(custom.interval, Duration::from_secs(1));
        assert!(parse_stress_config(Some("20"), None).is_err());
        assert!(parse_stress_config(Some("65"), None).is_err());
        assert!(parse_stress_config(Some("25"), Some("49")).is_err());
        assert!(parse_stress_config(Some("25"), Some("1001")).is_err());
        assert!(parse_stress_config(None, Some("250")).is_err());
    }
}
