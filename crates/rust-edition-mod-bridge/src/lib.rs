//! Small, opt-in local link between a game client and a separate helper.
//!
//! The helper runs as a child process. It sees only read-only snapshots, and
//! can return only a short status line. Every frame has a fixed size limit,
//! and every request must get a reply before its deadline. The bounded queue
//! holds the game's 10-step catch-up burst plus two nearby state frames.

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    ffi::OsString,
    fmt, fs,
    io::{self, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    ops::{Deref, DerefMut},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

pub const PROTOCOL_VERSION: u8 = 2;
pub const MAX_CLIENT_FRAME_BYTES: usize = 4096;
pub const MAX_HELPER_FRAME_BYTES: usize = 1024;
/// Holds up to 10 catch-up ticks and two lifecycle state frames.
pub const MAX_OUTSTANDING: usize = 12;
pub const MAX_VALUE_BYTES: usize = 256;
pub const MAX_DIMENSION_BYTES: usize = 128;
pub const MAX_FRAMEBUFFER_DIMENSION: u32 = 32_768;
pub const STARTUP_DEADLINE: Duration = Duration::from_secs(120);
pub const TICK_DEADLINE: Duration = Duration::from_secs(1);

/// Child-process settings. All three paths must be absolute, and the config
/// file and scratch directory must already exist.
#[derive(Clone, Debug)]
pub struct SpawnConfig {
    pub helper: PathBuf,
    pub config: PathBuf,
    pub scratch_dir: PathBuf,
    /// Arguments for a small native test helper. Normal launches leave this
    /// empty; no shell is used to start the process.
    pub helper_args: Vec<OsString>,
}

impl SpawnConfig {
    pub fn new(
        helper: impl Into<PathBuf>,
        config: impl Into<PathBuf>,
        scratch_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            helper: helper.into(),
            config: config.into(),
            scratch_dir: scratch_dir.into(),
            helper_args: Vec::new(),
        }
    }
}

/// A small view of the active player's state. It contains no inventory,
/// world blocks, commands, or other data the helper could change.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlayerSnapshot {
    pub x: f64,
    pub y: f64,
    pub z: f64,
    pub yaw: f32,
    pub pitch: f32,
    pub health: f32,
    pub dimension: String,
}

/// Read-only world clock values.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClockSnapshot {
    pub game_time: i64,
    pub day_time: i64,
}

/// The pixel dimensions of the window's current renderer framebuffer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FramebufferSnapshot {
    pub width: u32,
    pub height: u32,
}

impl FramebufferSnapshot {
    fn validate(self) -> Result<(), BridgeError> {
        if self.width > MAX_FRAMEBUFFER_DIMENSION || self.height > MAX_FRAMEBUFFER_DIMENSION {
            return Err(BridgeError::Protocol(format!(
                "framebuffer dimensions exceed {MAX_FRAMEBUFFER_DIMENSION} pixels"
            )));
        }
        Ok(())
    }
}

/// Why the game sent a state update that did not finish a client tick.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StateReason {
    Initial,
    Title,
    WorldClosed,
}

/// The kind of client information the helper received.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrameKind {
    Tick,
    State,
}

/// Read-only client state shared by tick and lifecycle frames.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    pub world_open: bool,
    pub player: Option<PlayerSnapshot>,
    pub clock: Option<ClockSnapshot>,
    pub framebuffer: FramebufferSnapshot,
}

impl Snapshot {
    pub fn validate(&self) -> Result<(), BridgeError> {
        self.framebuffer.validate()?;
        if let Some(player) = &self.player {
            if !player.x.is_finite()
                || !player.y.is_finite()
                || !player.z.is_finite()
                || !player.yaw.is_finite()
                || !player.pitch.is_finite()
                || !player.health.is_finite()
            {
                return Err(BridgeError::Protocol(
                    "player snapshot contains a non-finite number".into(),
                ));
            }
            if player.dimension.len() > MAX_DIMENSION_BYTES {
                return Err(BridgeError::Protocol(format!(
                    "dimension name exceeds {MAX_DIMENSION_BYTES} bytes"
                )));
            }
        }
        if !self.world_open && (self.player.is_some() || self.clock.is_some()) {
            return Err(BridgeError::Protocol(
                "a closed world must not include player or clock data".into(),
            ));
        }
        if self.player.is_none() && self.clock.is_some() {
            return Err(BridgeError::Protocol(
                "clock data requires an open player snapshot".into(),
            ));
        }
        Ok(())
    }
}

/// Returns the exact JSON body used for a completed client tick.
pub fn snapshot_json(seq: u64, snapshot: &Snapshot) -> Result<String, BridgeError> {
    frame_json(
        HostFrame::Tick {
            v: PROTOCOL_VERSION,
            seq,
            world_open: snapshot.world_open,
            player: snapshot.player.clone(),
            clock: snapshot.clock.clone(),
            framebuffer: snapshot.framebuffer,
        },
        snapshot,
    )
}

/// Returns the exact JSON body used for an initial, title, or world-close
/// state update. These frames never mean that a client tick completed.
pub fn state_json(
    seq: u64,
    reason: StateReason,
    snapshot: &Snapshot,
) -> Result<String, BridgeError> {
    reason.validate(snapshot)?;
    frame_json(
        HostFrame::State {
            v: PROTOCOL_VERSION,
            seq,
            reason,
            world_open: snapshot.world_open,
            player: snapshot.player.clone(),
            clock: snapshot.clock.clone(),
            framebuffer: snapshot.framebuffer,
        },
        snapshot,
    )
}

impl StateReason {
    fn validate(self, snapshot: &Snapshot) -> Result<(), BridgeError> {
        if matches!(self, Self::Title | Self::WorldClosed) && snapshot.world_open {
            return Err(BridgeError::Protocol(format!(
                "a {} state must describe a closed world",
                match self {
                    Self::Title => "title",
                    Self::WorldClosed => "world-closed",
                    Self::Initial => unreachable!(),
                }
            )));
        }
        Ok(())
    }
}

fn frame_json(frame: HostFrame, snapshot: &Snapshot) -> Result<String, BridgeError> {
    snapshot.validate()?;
    let json = serde_json::to_string(&frame)
        .map_err(|error| BridgeError::Protocol(format!("could not encode JSON: {error}")))?;
    if json.len() > MAX_CLIENT_FRAME_BYTES {
        return Err(BridgeError::Protocol(format!(
            "client frame is {} bytes; limit is {MAX_CLIENT_FRAME_BYTES}",
            json.len()
        )));
    }
    Ok(json)
}

/// Frames sent from the game to the helper. `deny_unknown_fields` keeps both
/// sides from quietly disagreeing about a newer message shape.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum HostFrame {
    Authenticated {
        v: u8,
    },
    Ready {
        v: u8,
    },
    Tick {
        v: u8,
        seq: u64,
        world_open: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        player: Option<PlayerSnapshot>,
        #[serde(skip_serializing_if = "Option::is_none")]
        clock: Option<ClockSnapshot>,
        framebuffer: FramebufferSnapshot,
    },
    State {
        v: u8,
        seq: u64,
        reason: StateReason,
        world_open: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        player: Option<PlayerSnapshot>,
        #[serde(skip_serializing_if = "Option::is_none")]
        clock: Option<ClockSnapshot>,
        framebuffer: FramebufferSnapshot,
    },
    Shutdown {
        v: u8,
    },
}

/// Frames sent from the helper to the game.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum HelperFrame {
    Hello {
        v: u8,
        token: String,
    },
    Ready {
        v: u8,
    },
    TickResult {
        v: u8,
        seq: u64,
        status: ResultStatus,
        #[serde(skip_serializing_if = "Option::is_none")]
        value: Option<String>,
    },
    ShutdownAck {
        v: u8,
    },
    Error {
        v: u8,
        value: String,
    },
}

/// Whether a helper completed the callback successfully.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResultStatus {
    Ok,
    Failed,
}

/// A validated request received by a helper process.
#[derive(Clone, Debug, PartialEq)]
pub enum HelperRequest {
    Tick {
        seq: u64,
        snapshot: Snapshot,
    },
    State {
        seq: u64,
        reason: StateReason,
        snapshot: Snapshot,
    },
    Shutdown,
}

/// The helper-side view of the bridge. It connects to the game using the
/// private environment values set for that child process.
pub struct HelperConnection {
    stream: TcpStream,
    next_sequence: u64,
    pending_sequence: Option<u64>,
    ready: bool,
    failed: bool,
}

impl HelperConnection {
    /// Reads the private address and token from the child environment, then
    /// completes the shared v2 handshake.
    pub fn connect_from_env() -> Result<Self, BridgeError> {
        let address = std::env::var(CHILD_ADDRESS_ENV)
            .map_err(|_| BridgeError::InvalidConfig(format!("missing {CHILD_ADDRESS_ENV}")))?;
        let token = std::env::var(CHILD_TOKEN_ENV)
            .map_err(|_| BridgeError::InvalidConfig(format!("missing {CHILD_TOKEN_ENV}")))?;
        let address: SocketAddr = address.parse().map_err(|_| {
            BridgeError::InvalidConfig("helper address is not a socket address".into())
        })?;
        if !address.ip().is_loopback() {
            return Err(BridgeError::InvalidConfig(
                "helper address must use loopback".into(),
            ));
        }
        let deadline = Instant::now() + STARTUP_DEADLINE;
        let mut stream = TcpStream::connect_timeout(&address, STARTUP_DEADLINE)
            .map_err(|error| BridgeError::Io(format!("could not connect to game: {error}")))?;
        stream
            .set_nodelay(true)
            .map_err(|error| BridgeError::Io(format!("could not prepare game socket: {error}")))?;
        write_frame(
            &mut stream,
            &HelperFrame::Hello {
                v: PROTOCOL_VERSION,
                token,
            },
            MAX_CLIENT_FRAME_BYTES,
            deadline,
        )?;
        match read_frame::<HostFrame>(&mut stream, MAX_CLIENT_FRAME_BYTES, deadline)? {
            HostFrame::Authenticated { v } if v == PROTOCOL_VERSION => Ok(Self {
                stream,
                next_sequence: 1,
                pending_sequence: None,
                ready: false,
                failed: false,
            }),
            HostFrame::Authenticated { v } => Err(BridgeError::Protocol(format!(
                "unsupported protocol version {v}"
            ))),
            _ => Err(BridgeError::Protocol(
                "game sent the wrong startup message".into(),
            )),
        }
    }

    /// Announces that the Java/Fabric runtime has finished loading. The
    /// helper is not ready for ticks until the game confirms this step.
    pub fn mark_ready(&mut self) -> Result<(), BridgeError> {
        if self.failed {
            return Err(BridgeError::Stopped);
        }
        if self.ready {
            return Err(BridgeError::Protocol(
                "helper was marked ready twice".into(),
            ));
        }
        write_frame(
            &mut self.stream,
            &HelperFrame::Ready {
                v: PROTOCOL_VERSION,
            },
            MAX_CLIENT_FRAME_BYTES,
            Instant::now() + STARTUP_DEADLINE,
        )?;
        match read_frame::<HostFrame>(
            &mut self.stream,
            MAX_CLIENT_FRAME_BYTES,
            Instant::now() + STARTUP_DEADLINE,
        )? {
            HostFrame::Ready { v } if v == PROTOCOL_VERSION => {
                self.ready = true;
                Ok(())
            }
            HostFrame::Ready { v } => Err(BridgeError::Protocol(format!(
                "unsupported protocol version {v}"
            ))),
            _ => Err(BridgeError::Protocol(
                "game sent the wrong ready acknowledgement".into(),
            )),
        }
    }

    /// Reports a bounded startup or runtime failure to the game. The full
    /// cause can stay in the helper's private scratch log.
    pub fn fail(&mut self, reason: impl AsRef<str>) -> Result<(), BridgeError> {
        if self.failed {
            return Err(BridgeError::Stopped);
        }
        let value = bounded_text(reason.as_ref(), MAX_VALUE_BYTES);
        write_frame(
            &mut self.stream,
            &HelperFrame::Error {
                v: PROTOCOL_VERSION,
                value,
            },
            MAX_HELPER_FRAME_BYTES,
            Instant::now()
                + if self.ready {
                    TICK_DEADLINE
                } else {
                    STARTUP_DEADLINE
                },
        )?;
        self.failed = true;
        Ok(())
    }

    /// Waits for one tick, state update, or shutdown message. Sequence numbers
    /// must arrive in order and every snapshot must pass shared validation.
    pub fn receive(&mut self) -> Result<HelperRequest, BridgeError> {
        if self.failed {
            return Err(BridgeError::Stopped);
        }
        if !self.ready {
            return Err(BridgeError::NotReady);
        }
        if self.pending_sequence.is_some() {
            return Err(BridgeError::Protocol(
                "helper requested the next tick before replying to the last one".into(),
            ));
        }
        match read_frame_after_first_byte::<HostFrame>(
            &mut self.stream,
            MAX_CLIENT_FRAME_BYTES,
            TICK_DEADLINE,
        )? {
            HostFrame::Tick {
                v,
                seq,
                world_open,
                player,
                clock,
                framebuffer,
            } if v == PROTOCOL_VERSION => {
                if seq != self.next_sequence {
                    return Err(BridgeError::Protocol(format!(
                        "request sequence {seq} did not match expected {}",
                        self.next_sequence
                    )));
                }
                let Some(next_sequence) = seq.checked_add(1) else {
                    return Err(BridgeError::SequenceExhausted);
                };
                let snapshot = Snapshot {
                    world_open,
                    player,
                    clock,
                    framebuffer,
                };
                snapshot.validate()?;
                self.next_sequence = next_sequence;
                self.pending_sequence = Some(seq);
                Ok(HelperRequest::Tick { seq, snapshot })
            }
            HostFrame::Tick { v, .. } => Err(BridgeError::Protocol(format!(
                "unsupported protocol version {v}"
            ))),
            HostFrame::State {
                v,
                seq,
                reason,
                world_open,
                player,
                clock,
                framebuffer,
            } if v == PROTOCOL_VERSION => {
                if seq != self.next_sequence {
                    return Err(BridgeError::Protocol(format!(
                        "request sequence {seq} did not match expected {}",
                        self.next_sequence
                    )));
                }
                let Some(next_sequence) = seq.checked_add(1) else {
                    return Err(BridgeError::SequenceExhausted);
                };
                let snapshot = Snapshot {
                    world_open,
                    player,
                    clock,
                    framebuffer,
                };
                snapshot.validate()?;
                reason.validate(&snapshot)?;
                self.next_sequence = next_sequence;
                self.pending_sequence = Some(seq);
                Ok(HelperRequest::State {
                    seq,
                    reason,
                    snapshot,
                })
            }
            HostFrame::State { v, .. } => Err(BridgeError::Protocol(format!(
                "unsupported protocol version {v}"
            ))),
            HostFrame::Shutdown { v } if v == PROTOCOL_VERSION => {
                write_frame(
                    &mut self.stream,
                    &HelperFrame::ShutdownAck {
                        v: PROTOCOL_VERSION,
                    },
                    MAX_HELPER_FRAME_BYTES,
                    Instant::now() + TICK_DEADLINE,
                )?;
                Ok(HelperRequest::Shutdown)
            }
            HostFrame::Shutdown { v } => Err(BridgeError::Protocol(format!(
                "unsupported protocol version {v}"
            ))),
            HostFrame::Authenticated { .. } | HostFrame::Ready { .. } => Err(
                BridgeError::Protocol("game sent a startup message after it was ready".into()),
            ),
        }
    }

    /// Sends the read-only result for the most recent tick or state frame.
    pub fn send_result(
        &mut self,
        seq: u64,
        status: ResultStatus,
        value: Option<String>,
    ) -> Result<(), BridgeError> {
        if self.failed {
            return Err(BridgeError::Stopped);
        }
        if !self.ready {
            return Err(BridgeError::NotReady);
        }
        if self.pending_sequence != Some(seq) {
            return Err(BridgeError::Protocol(
                "reply sequence did not match the last request".into(),
            ));
        }
        if value
            .as_ref()
            .is_some_and(|value| value.len() > MAX_VALUE_BYTES)
        {
            return Err(BridgeError::Protocol(format!(
                "reply text exceeds {MAX_VALUE_BYTES} bytes"
            )));
        }
        write_frame(
            &mut self.stream,
            &HelperFrame::TickResult {
                v: PROTOCOL_VERSION,
                seq,
                status,
                value,
            },
            MAX_HELPER_FRAME_BYTES,
            Instant::now() + TICK_DEADLINE,
        )?;
        self.pending_sequence = None;
        Ok(())
    }
}

/// A reply to one tick or state frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TickResult {
    pub seq: u64,
    pub kind: FrameKind,
    pub state_reason: Option<StateReason>,
    pub status: ResultStatus,
    pub value: Option<String>,
    /// Exact JSON body sent with this tick. This is local metadata, not a wire field.
    pub snapshot_json: String,
}

/// Current state of the helper process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BridgeStatus {
    Starting,
    Ready,
    Failed(BridgeError),
    Stopped,
}

impl BridgeStatus {
    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready)
    }
}

/// An expected bridge error. Error text never includes the private token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BridgeError {
    InvalidConfig(String),
    NotReady,
    QueueFull,
    Stopped,
    Cancelled,
    Timeout(&'static str),
    ChildExited(Option<i32>),
    HelperFailed(String),
    Protocol(String),
    Io(String),
    SequenceExhausted,
}

impl fmt::Display for BridgeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfig(message) => write!(f, "invalid helper configuration: {message}"),
            Self::NotReady => f.write_str("helper is still starting"),
            Self::QueueFull => f.write_str("helper did not keep up with client frames"),
            Self::Stopped => f.write_str("helper has stopped"),
            Self::Cancelled => f.write_str("helper startup or request was cancelled"),
            Self::Timeout(phase) => write!(f, "helper timed out during {phase}"),
            Self::ChildExited(code) => write!(
                f,
                "helper exited before it was ready or replied (status {code:?})"
            ),
            Self::HelperFailed(message) => write!(f, "helper failed: {message}"),
            Self::Protocol(message) => write!(f, "helper sent an invalid message: {message}"),
            Self::Io(message) => write!(f, "helper communication failed: {message}"),
            Self::SequenceExhausted => f.write_str("helper frame sequence ran out"),
        }
    }
}

impl std::error::Error for BridgeError {}

struct QueuedFrame {
    seq: u64,
    kind: FrameKind,
    state_reason: Option<StateReason>,
    snapshot: Snapshot,
    snapshot_json: String,
}

/// Owns the helper process and its one bounded worker thread.
pub struct Bridge {
    status: Arc<Mutex<BridgeStatus>>,
    cancel: Arc<AtomicBool>,
    requests: SyncSender<QueuedFrame>,
    replies: Receiver<TickResult>,
    outstanding: Arc<AtomicUsize>,
    next_sequence: u64,
    worker: Option<JoinHandle<()>>,
}

impl Bridge {
    /// Starts an absolute-path helper and returns while it performs its
    /// handshake. The game can keep drawing while `status()` is `Starting`.
    pub fn spawn(config: SpawnConfig) -> Result<Self, BridgeError> {
        let helper = absolute_file(&config.helper, "helper executable")?;
        let config_path = absolute_file(&config.config, "helper config")?;
        let scratch_dir = absolute_dir(&config.scratch_dir, "helper scratch directory")?;
        let child_scratch = ChildScratch::create(&scratch_dir)?;
        let token = random_token()?;
        let listener = TcpListener::bind(("127.0.0.1", 0)).map_err(|error| {
            BridgeError::Io(format!("could not open loopback listener: {error}"))
        })?;
        listener.set_nonblocking(true).map_err(|error| {
            BridgeError::Io(format!("could not prepare loopback listener: {error}"))
        })?;
        let address = listener.local_addr().map_err(|error| {
            BridgeError::Io(format!("could not read loopback address: {error}"))
        })?;

        let mut command = Command::new(&helper);
        command
            .args(&config.helper_args)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .current_dir(&child_scratch.root)
            .env("HOME", &child_scratch.home)
            .env("USERPROFILE", &child_scratch.profile)
            .env("TMPDIR", &child_scratch.temp)
            .env("TEMP", &child_scratch.temp)
            .env("TMP", &child_scratch.temp)
            .env("XDG_DATA_HOME", &child_scratch.xdg_data)
            .env("XDG_CONFIG_HOME", &child_scratch.xdg_config)
            .env("HORIZON_HOME", &child_scratch.game_home)
            .env("HORIZON_LAUNCHER_HOME", &child_scratch.launcher_home)
            .env("HORIZON_LAUNCHER_NO_SELF_UPDATE", "1")
            .env("HORIZON_NO_AUDIO", "1")
            .env("HORIZON_TELEMETRY_URL", "off")
            .env("HORIZON_NO_HEARTBEAT", "1")
            .env("HORIZON_NO_DISCORD", "1")
            .env(CHILD_ADDRESS_ENV, address.to_string())
            .env(CHILD_TOKEN_ENV, &token)
            .env(CHILD_CONFIG_ENV, &config_path);
        // Windows needs these to locate system DLLs. All other inherited
        // environment values stay cleared; paths and secrets come from the
        // explicit bridge config.
        #[cfg(windows)]
        for key in ["SystemRoot", "WINDIR"] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        let child = command
            .spawn()
            .map_err(|error| BridgeError::Io(format!("could not start helper: {error}")))?;
        let startup_deadline = Instant::now() + STARTUP_DEADLINE;
        let child = ChildGuard {
            child,
            _scratch: child_scratch,
        };

        let status = Arc::new(Mutex::new(BridgeStatus::Starting));
        let cancel = Arc::new(AtomicBool::new(false));
        let outstanding = Arc::new(AtomicUsize::new(0));
        let (request_tx, request_rx) = mpsc::sync_channel(MAX_OUTSTANDING);
        let (reply_tx, reply_rx) = mpsc::sync_channel(MAX_OUTSTANDING);
        let worker_status = status.clone();
        let worker_cancel = cancel.clone();
        let worker_outstanding = outstanding.clone();
        let worker = thread::Builder::new()
            .name("mod-bridge".into())
            .spawn(move || {
                run_worker(
                    listener,
                    token,
                    child,
                    startup_deadline,
                    request_rx,
                    reply_tx,
                    worker_status,
                    worker_cancel,
                    worker_outstanding,
                );
            })
            .map_err(|error| BridgeError::Io(format!("could not start helper worker: {error}")))?;

        Ok(Self {
            status,
            cancel,
            requests: request_tx,
            replies: reply_rx,
            outstanding,
            next_sequence: 1,
            worker: Some(worker),
        })
    }

    pub fn status(&self) -> BridgeStatus {
        self.status
            .lock()
            .map(|status| status.clone())
            .unwrap_or_else(|_| {
                BridgeStatus::Failed(BridgeError::Io("helper status lock failed".into()))
            })
    }

    pub fn is_ready(&self) -> bool {
        self.status().is_ready()
    }

    pub fn failure(&self) -> Option<BridgeError> {
        match self.status() {
            BridgeStatus::Failed(error) => Some(error),
            _ => None,
        }
    }

    /// Compatibility alias for `try_send_tick`.
    pub fn try_send(&mut self, snapshot: Snapshot) -> Result<u64, BridgeError> {
        self.try_send_tick(snapshot)
    }

    /// Enqueues one completed client tick and returns its sequence. Calling
    /// this before the helper is ready does not queue a request or count a tick.
    pub fn try_send_tick(&mut self, snapshot: Snapshot) -> Result<u64, BridgeError> {
        self.try_send_frame(FrameKind::Tick, None, snapshot)
    }

    /// Enqueues one synthetic state update that must not dispatch a client
    /// tick event in the helper.
    pub fn try_send_state(
        &mut self,
        reason: StateReason,
        snapshot: Snapshot,
    ) -> Result<u64, BridgeError> {
        self.try_send_frame(FrameKind::State, Some(reason), snapshot)
    }

    fn try_send_frame(
        &mut self,
        kind: FrameKind,
        state_reason: Option<StateReason>,
        snapshot: Snapshot,
    ) -> Result<u64, BridgeError> {
        match self.status() {
            BridgeStatus::Starting => return Err(BridgeError::NotReady),
            BridgeStatus::Ready => {}
            BridgeStatus::Failed(error) => return Err(error),
            BridgeStatus::Stopped => return Err(BridgeError::Stopped),
        }
        if let Err(error) = snapshot.validate() {
            self.fail_fast(error.clone());
            return Err(error);
        }
        let seq = self.next_sequence;
        let json_result = match (kind, state_reason) {
            (FrameKind::Tick, None) => snapshot_json(seq, &snapshot),
            (FrameKind::State, Some(reason)) => state_json(seq, reason, &snapshot),
            _ => unreachable!("frame kind and state reason must agree"),
        };
        let snapshot_json = match json_result {
            Ok(json) => json,
            Err(error) => {
                self.fail_fast(error.clone());
                return Err(error);
            }
        };
        let Some(next_sequence) = seq.checked_add(1) else {
            let error = BridgeError::SequenceExhausted;
            self.fail_fast(error.clone());
            return Err(error);
        };
        let mut outstanding = self.outstanding.load(Ordering::Acquire);
        loop {
            if outstanding >= MAX_OUTSTANDING {
                self.fail_fast(BridgeError::QueueFull);
                return Err(BridgeError::QueueFull);
            }
            match self.outstanding.compare_exchange_weak(
                outstanding,
                outstanding + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(current) => outstanding = current,
            }
        }
        let request = QueuedFrame {
            seq,
            kind,
            state_reason,
            snapshot,
            snapshot_json,
        };
        match self.requests.try_send(request) {
            Ok(()) => {
                self.next_sequence = next_sequence;
                Ok(seq)
            }
            Err(TrySendError::Full(_)) => {
                self.outstanding.fetch_sub(1, Ordering::AcqRel);
                self.fail_fast(BridgeError::QueueFull);
                Err(BridgeError::QueueFull)
            }
            Err(TrySendError::Disconnected(_)) => {
                self.outstanding.fetch_sub(1, Ordering::AcqRel);
                let error = self.failure().unwrap_or(BridgeError::Stopped);
                Err(error)
            }
        }
    }

    /// Returns the next completed reply, if one is waiting.
    pub fn try_recv(&self) -> Option<TickResult> {
        match self.replies.try_recv() {
            Ok(reply) => {
                self.outstanding.fetch_sub(1, Ordering::AcqRel);
                Some(reply)
            }
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => None,
        }
    }

    /// Stops I/O, asks a ready helper to exit, then kills and reaps it if it
    /// does not finish promptly. `Drop` uses the same path.
    pub fn shutdown(&mut self) {
        self.cancel.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }

    fn fail_fast(&self, error: BridgeError) {
        set_failed(&self.status, error);
        self.cancel.store(true, Ordering::Release);
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// The child environment uses these names so a small helper can be started
/// without command-line secrets or a shell.
pub const CHILD_ADDRESS_ENV: &str = "RUST_EDITION_MOD_BRIDGE_ADDR";
pub const CHILD_TOKEN_ENV: &str = "RUST_EDITION_MOD_BRIDGE_TOKEN";
pub const CHILD_CONFIG_ENV: &str = "RUST_EDITION_MOD_BRIDGE_CONFIG";

/// Writes a length-prefixed JSON frame. The fixed deadline covers the whole
/// frame, including every partial write.
pub fn write_frame<T: Serialize>(
    stream: &mut TcpStream,
    value: &T,
    max_bytes: usize,
    deadline: Instant,
) -> Result<(), BridgeError> {
    let payload = serde_json::to_vec(value)
        .map_err(|error| BridgeError::Protocol(format!("could not encode JSON: {error}")))?;
    if payload.is_empty() || payload.len() > max_bytes || payload.len() > u32::MAX as usize {
        return Err(BridgeError::Protocol(format!(
            "frame is {} bytes; limit is {max_bytes}",
            payload.len()
        )));
    }
    stream
        .set_nonblocking(true)
        .map_err(|error| BridgeError::Io(format!("could not prepare socket: {error}")))?;
    let mut framed = Vec::with_capacity(4 + payload.len());
    framed.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    framed.extend_from_slice(&payload);
    write_exact_until(stream, &framed, deadline, None)
}

/// Reads one length-prefixed JSON frame. The fixed deadline covers the
/// header and body together; a peer cannot extend it by trickling bytes.
pub fn read_frame<T: DeserializeOwned>(
    stream: &mut TcpStream,
    max_bytes: usize,
    deadline: Instant,
) -> Result<T, BridgeError> {
    read_frame_until(stream, max_bytes, deadline, None)
}

/// Waits without a deadline for a new frame to begin, then gives its header
/// and body one fixed completion deadline. This lets helpers idle while the
/// game is paused without allowing a peer to trickle a frame forever.
pub fn read_frame_after_first_byte<T: DeserializeOwned>(
    stream: &mut TcpStream,
    max_bytes: usize,
    frame_timeout: Duration,
) -> Result<T, BridgeError> {
    stream
        .set_nonblocking(true)
        .map_err(|error| BridgeError::Io(format!("could not prepare socket: {error}")))?;
    let mut header = [0_u8; 4];
    loop {
        match stream.read(&mut header[..1]) {
            Ok(0) => {
                return Err(BridgeError::Io(
                    "game closed the socket while waiting for a request".into(),
                ));
            }
            Ok(_) => break,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => pause_poll(),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => {
                return Err(BridgeError::Io(format!(
                    "could not read helper frame: {error}"
                )));
            }
        }
    }
    let deadline = Instant::now() + frame_timeout;
    read_exact_until(stream, &mut header[1..], deadline, None)?;
    decode_frame_body(stream, header, max_bytes, deadline, None)
}

fn read_frame_until<T: DeserializeOwned>(
    stream: &mut TcpStream,
    max_bytes: usize,
    deadline: Instant,
    cancel: Option<&AtomicBool>,
) -> Result<T, BridgeError> {
    stream
        .set_nonblocking(true)
        .map_err(|error| BridgeError::Io(format!("could not prepare socket: {error}")))?;
    let mut header = [0_u8; 4];
    read_exact_until(stream, &mut header, deadline, cancel)?;
    decode_frame_body(stream, header, max_bytes, deadline, cancel)
}

fn decode_frame_body<T: DeserializeOwned>(
    stream: &mut TcpStream,
    header: [u8; 4],
    max_bytes: usize,
    deadline: Instant,
    cancel: Option<&AtomicBool>,
) -> Result<T, BridgeError> {
    let size = u32::from_be_bytes(header) as usize;
    if size == 0 || size > max_bytes {
        return Err(BridgeError::Protocol(format!(
            "frame length {size} is outside 1..={max_bytes}"
        )));
    }
    let mut payload = vec![0; size];
    read_exact_until(stream, &mut payload, deadline, cancel)?;
    serde_json::from_slice(&payload)
        .map_err(|error| BridgeError::Protocol(format!("invalid JSON frame: {error}")))
}

fn write_exact_until(
    stream: &mut TcpStream,
    bytes: &[u8],
    deadline: Instant,
    cancel: Option<&AtomicBool>,
) -> Result<(), BridgeError> {
    let mut written = 0;
    while written < bytes.len() {
        check_deadline(deadline, cancel, "writing a frame")?;
        match stream.write(&bytes[written..]) {
            Ok(0) => {
                return Err(BridgeError::Io(
                    "helper closed the socket while writing".into(),
                ));
            }
            Ok(count) => written += count,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => pause_poll(),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => {
                return Err(BridgeError::Io(format!(
                    "could not write helper frame: {error}"
                )));
            }
        }
    }
    Ok(())
}

fn read_exact_until(
    stream: &mut TcpStream,
    bytes: &mut [u8],
    deadline: Instant,
    cancel: Option<&AtomicBool>,
) -> Result<(), BridgeError> {
    let mut read = 0;
    while read < bytes.len() {
        check_deadline(deadline, cancel, "reading a frame")?;
        match stream.read(&mut bytes[read..]) {
            Ok(0) => {
                return Err(BridgeError::Io(
                    "helper closed the socket while reading".into(),
                ));
            }
            Ok(count) => read += count,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => pause_poll(),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => {
                return Err(BridgeError::Io(format!(
                    "could not read helper frame: {error}"
                )));
            }
        }
    }
    Ok(())
}

fn check_deadline(
    deadline: Instant,
    cancel: Option<&AtomicBool>,
    phase: &'static str,
) -> Result<(), BridgeError> {
    if cancel.is_some_and(|flag| flag.load(Ordering::Acquire)) {
        return Err(BridgeError::Cancelled);
    }
    if Instant::now() >= deadline {
        return Err(BridgeError::Timeout(phase));
    }
    Ok(())
}

fn pause_poll() {
    thread::sleep(Duration::from_millis(2));
}

fn run_worker(
    listener: TcpListener,
    token: String,
    mut child: ChildGuard,
    startup_deadline: Instant,
    requests: Receiver<QueuedFrame>,
    replies: SyncSender<TickResult>,
    status: Arc<Mutex<BridgeStatus>>,
    cancel: Arc<AtomicBool>,
    _outstanding: Arc<AtomicUsize>,
) {
    let stream = loop {
        if cancel.load(Ordering::Acquire) {
            finish_child(&mut child, &status, false, None);
            return;
        }
        match child.try_wait() {
            Ok(Some(exit)) => {
                set_failed(&status, BridgeError::ChildExited(exit.code()));
                finish_child(&mut child, &status, false, None);
                return;
            }
            Err(error) => {
                set_failed(
                    &status,
                    BridgeError::Io(format!("could not check helper process: {error}")),
                );
                finish_child(&mut child, &status, false, None);
                return;
            }
            Ok(None) => {}
        }
        if Instant::now() >= startup_deadline {
            set_failed(&status, BridgeError::Timeout("starting"));
            finish_child(&mut child, &status, false, None);
            return;
        }
        match listener.accept() {
            Ok((stream, peer)) if peer.ip().is_loopback() => break stream,
            Ok((_stream, _peer)) => {
                set_failed(
                    &status,
                    BridgeError::Protocol("connection did not come from loopback".into()),
                );
                finish_child(&mut child, &status, false, None);
                return;
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => pause_poll(),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => {
                set_failed(
                    &status,
                    BridgeError::Io(format!("could not accept helper connection: {error}")),
                );
                finish_child(&mut child, &status, false, None);
                return;
            }
        }
    };

    let mut stream = stream;
    if let Err(error) = stream.set_nonblocking(true) {
        set_failed(
            &status,
            BridgeError::Io(format!("could not prepare helper socket: {error}")),
        );
        finish_child(&mut child, &status, false, Some(&mut stream));
        return;
    }
    let hello = match read_frame_until::<HelperFrame>(
        &mut stream,
        MAX_HELPER_FRAME_BYTES,
        startup_deadline,
        Some(&cancel),
    ) {
        Ok(frame) => frame,
        Err(error) => {
            if error != BridgeError::Cancelled {
                set_failed(&status, error);
            }
            let graceful = !cancel.load(Ordering::Acquire);
            finish_child(&mut child, &status, graceful, Some(&mut stream));
            return;
        }
    };
    match hello {
        HelperFrame::Hello { v, token: supplied }
            if v == PROTOCOL_VERSION && constant_time_eq(supplied.as_bytes(), token.as_bytes()) => {
        }
        HelperFrame::Hello { v, .. } if v != PROTOCOL_VERSION => {
            set_failed(
                &status,
                BridgeError::Protocol(format!("unsupported protocol version {v}")),
            );
            finish_child(&mut child, &status, false, Some(&mut stream));
            return;
        }
        _ => {
            set_failed(
                &status,
                BridgeError::Protocol("first helper message did not authenticate".into()),
            );
            finish_child(&mut child, &status, false, Some(&mut stream));
            return;
        }
    }
    if let Err(error) = write_frame(
        &mut stream,
        &HostFrame::Authenticated {
            v: PROTOCOL_VERSION,
        },
        MAX_CLIENT_FRAME_BYTES,
        startup_deadline,
    ) {
        set_failed(&status, error);
        finish_child(&mut child, &status, false, Some(&mut stream));
        return;
    }
    let helper_ready = match read_frame_until::<HelperFrame>(
        &mut stream,
        MAX_HELPER_FRAME_BYTES,
        startup_deadline,
        Some(&cancel),
    ) {
        Ok(frame) => frame,
        Err(error) => {
            if error != BridgeError::Cancelled {
                set_failed(&status, error);
            }
            finish_child(&mut child, &status, false, Some(&mut stream));
            return;
        }
    };
    match helper_ready {
        HelperFrame::Ready { v } if v == PROTOCOL_VERSION => {}
        HelperFrame::Ready { v } => {
            set_failed(
                &status,
                BridgeError::Protocol(format!("unsupported protocol version {v}")),
            );
            finish_child(&mut child, &status, false, Some(&mut stream));
            return;
        }
        HelperFrame::Error { v, value } if v == PROTOCOL_VERSION => {
            let error = if value.len() > MAX_VALUE_BYTES {
                BridgeError::Protocol(format!("helper error text exceeds {MAX_VALUE_BYTES} bytes"))
            } else {
                BridgeError::HelperFailed(value)
            };
            set_failed(&status, error);
            finish_child(&mut child, &status, false, Some(&mut stream));
            return;
        }
        HelperFrame::Error { v, .. } => {
            set_failed(
                &status,
                BridgeError::Protocol(format!("unsupported protocol version {v}")),
            );
            finish_child(&mut child, &status, false, Some(&mut stream));
            return;
        }
        _ => {
            set_failed(
                &status,
                BridgeError::Protocol("helper sent the wrong readiness message".into()),
            );
            finish_child(&mut child, &status, false, Some(&mut stream));
            return;
        }
    }
    if let Err(error) = write_frame(
        &mut stream,
        &HostFrame::Ready {
            v: PROTOCOL_VERSION,
        },
        MAX_CLIENT_FRAME_BYTES,
        startup_deadline,
    ) {
        set_failed(&status, error);
        finish_child(&mut child, &status, false, Some(&mut stream));
        return;
    }
    set_status(&status, BridgeStatus::Ready);

    loop {
        if cancel.load(Ordering::Acquire) {
            let should_gracefully_stop =
                !matches!(status_snapshot(&status), BridgeStatus::Failed(_));
            finish_child(
                &mut child,
                &status,
                should_gracefully_stop,
                Some(&mut stream),
            );
            return;
        }
        let request = match requests.recv_timeout(Duration::from_millis(10)) {
            Ok(request) => request,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                match read_idle_helper_error(&mut stream) {
                    Ok(Some(error)) => {
                        set_failed(&status, error);
                        finish_child(&mut child, &status, false, Some(&mut stream));
                        return;
                    }
                    Err(error) => {
                        set_failed(&status, error);
                        finish_child(&mut child, &status, false, Some(&mut stream));
                        return;
                    }
                    Ok(None) => {}
                }
                match child.try_wait() {
                    Ok(Some(exit)) => {
                        set_failed(&status, BridgeError::ChildExited(exit.code()));
                        finish_child(&mut child, &status, false, Some(&mut stream));
                        return;
                    }
                    Ok(None) => continue,
                    Err(error) => {
                        set_failed(
                            &status,
                            BridgeError::Io(format!("could not check helper process: {error}")),
                        );
                        finish_child(&mut child, &status, false, Some(&mut stream));
                        return;
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                finish_child(&mut child, &status, true, Some(&mut stream));
                return;
            }
        };
        let deadline = Instant::now() + TICK_DEADLINE;
        let frame = match (request.kind, request.state_reason) {
            (FrameKind::Tick, None) => HostFrame::Tick {
                v: PROTOCOL_VERSION,
                seq: request.seq,
                world_open: request.snapshot.world_open,
                player: request.snapshot.player,
                clock: request.snapshot.clock,
                framebuffer: request.snapshot.framebuffer,
            },
            (FrameKind::State, Some(reason)) => HostFrame::State {
                v: PROTOCOL_VERSION,
                seq: request.seq,
                reason,
                world_open: request.snapshot.world_open,
                player: request.snapshot.player,
                clock: request.snapshot.clock,
                framebuffer: request.snapshot.framebuffer,
            },
            _ => unreachable!("frame kind and state reason must agree"),
        };
        if let Err(error) = write_frame_with_cancel(
            &mut stream,
            &frame,
            MAX_CLIENT_FRAME_BYTES,
            deadline,
            &cancel,
        ) {
            if error != BridgeError::Cancelled {
                set_failed(&status, error);
            }
            let graceful = !cancel.load(Ordering::Acquire)
                && !matches!(status_snapshot(&status), BridgeStatus::Failed(_));
            finish_child(&mut child, &status, graceful, Some(&mut stream));
            return;
        }
        let reply = match read_frame_until::<HelperFrame>(
            &mut stream,
            MAX_HELPER_FRAME_BYTES,
            deadline,
            Some(&cancel),
        ) {
            Ok(reply) => reply,
            Err(error) => {
                if error != BridgeError::Cancelled {
                    set_failed(&status, error);
                }
                let graceful = !cancel.load(Ordering::Acquire)
                    && !matches!(status_snapshot(&status), BridgeStatus::Failed(_));
                finish_child(&mut child, &status, graceful, Some(&mut stream));
                return;
            }
        };
        let reply = match reply {
            HelperFrame::Error { v, value } if v == PROTOCOL_VERSION => {
                let error = if value.len() > MAX_VALUE_BYTES {
                    BridgeError::Protocol(format!(
                        "helper error text exceeds {MAX_VALUE_BYTES} bytes"
                    ))
                } else {
                    BridgeError::HelperFailed(value)
                };
                set_failed(&status, error);
                finish_child(&mut child, &status, false, Some(&mut stream));
                return;
            }
            HelperFrame::Error { v, .. } => {
                set_failed(
                    &status,
                    BridgeError::Protocol(format!("unsupported protocol version {v}")),
                );
                finish_child(&mut child, &status, false, Some(&mut stream));
                return;
            }
            HelperFrame::TickResult {
                v,
                seq,
                status: result_status,
                value,
            } if v == PROTOCOL_VERSION && seq == request.seq => {
                if value
                    .as_ref()
                    .is_some_and(|value| value.len() > MAX_VALUE_BYTES)
                {
                    set_failed(
                        &status,
                        BridgeError::Protocol(format!(
                            "reply text exceeds {MAX_VALUE_BYTES} bytes"
                        )),
                    );
                    finish_child(&mut child, &status, false, Some(&mut stream));
                    return;
                }
                TickResult {
                    seq,
                    kind: request.kind,
                    state_reason: request.state_reason,
                    status: result_status,
                    value,
                    snapshot_json: request.snapshot_json,
                }
            }
            HelperFrame::TickResult { v, .. } if v != PROTOCOL_VERSION => {
                set_failed(
                    &status,
                    BridgeError::Protocol(format!("unsupported protocol version {v}")),
                );
                finish_child(&mut child, &status, false, Some(&mut stream));
                return;
            }
            HelperFrame::TickResult { seq, .. } => {
                set_failed(
                    &status,
                    BridgeError::Protocol(format!(
                        "reply sequence {seq} did not match request {}",
                        request.seq
                    )),
                );
                finish_child(&mut child, &status, false, Some(&mut stream));
                return;
            }
            _ => {
                set_failed(
                    &status,
                    BridgeError::Protocol("helper sent the wrong message after a tick".into()),
                );
                finish_child(&mut child, &status, false, Some(&mut stream));
                return;
            }
        };
        if let Err(error) = replies.try_send(reply) {
            let error = match error {
                TrySendError::Full(_) => BridgeError::QueueFull,
                TrySendError::Disconnected(_) => BridgeError::Stopped,
            };
            set_failed(&status, error);
            finish_child(&mut child, &status, false, Some(&mut stream));
            return;
        }
    }
}

fn write_frame_with_cancel<T: Serialize>(
    stream: &mut TcpStream,
    value: &T,
    max_bytes: usize,
    deadline: Instant,
    cancel: &AtomicBool,
) -> Result<(), BridgeError> {
    let payload = serde_json::to_vec(value)
        .map_err(|error| BridgeError::Protocol(format!("could not encode JSON: {error}")))?;
    if payload.is_empty() || payload.len() > max_bytes || payload.len() > u32::MAX as usize {
        return Err(BridgeError::Protocol(format!(
            "frame is {} bytes; limit is {max_bytes}",
            payload.len()
        )));
    }
    let mut framed = Vec::with_capacity(4 + payload.len());
    framed.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    framed.extend_from_slice(&payload);
    write_exact_until(stream, &framed, deadline, Some(cancel))
}

fn read_idle_helper_error(stream: &mut TcpStream) -> Result<Option<BridgeError>, BridgeError> {
    let mut first = [0_u8; 1];
    match stream.peek(&mut first) {
        Ok(0) => Err(BridgeError::Io("helper closed the socket".into())),
        Ok(_) => match read_frame_after_first_byte::<HelperFrame>(
            stream,
            MAX_HELPER_FRAME_BYTES,
            TICK_DEADLINE,
        )? {
            HelperFrame::Error { v, value } if v == PROTOCOL_VERSION => {
                if value.len() > MAX_VALUE_BYTES {
                    Err(BridgeError::Protocol(format!(
                        "helper error text exceeds {MAX_VALUE_BYTES} bytes"
                    )))
                } else {
                    Ok(Some(BridgeError::HelperFailed(value)))
                }
            }
            HelperFrame::Error { v, .. } => Err(BridgeError::Protocol(format!(
                "unsupported protocol version {v}"
            ))),
            _ => Err(BridgeError::Protocol(
                "helper sent a message without a pending tick".into(),
            )),
        },
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(None),
        Err(error) if error.kind() == io::ErrorKind::Interrupted => Ok(None),
        Err(error) => Err(BridgeError::Io(format!(
            "could not check helper socket: {error}"
        ))),
    }
}

struct ChildScratch {
    root: PathBuf,
    home: PathBuf,
    profile: PathBuf,
    temp: PathBuf,
    xdg_data: PathBuf,
    xdg_config: PathBuf,
    game_home: PathBuf,
    launcher_home: PathBuf,
}

impl ChildScratch {
    fn create(base: &Path) -> Result<Self, BridgeError> {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let mut made = None;
        for _ in 0..16 {
            let id = NEXT.fetch_add(1, Ordering::Relaxed);
            let root = base.join(format!(
                ".rust-edition-mod-bridge-{}-{id}",
                std::process::id()
            ));
            match create_private_dir(&root) {
                Ok(()) => {
                    made = Some(root);
                    break;
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(BridgeError::InvalidConfig(format!(
                        "could not create private helper scratch: {error}"
                    )));
                }
            }
        }
        let root = made.ok_or_else(|| {
            BridgeError::InvalidConfig(
                "could not create a unique private helper scratch directory".into(),
            )
        })?;
        let child_path = |name: &str| root.join(name);
        let scratch = Self {
            home: child_path("home"),
            profile: child_path("profile"),
            temp: child_path("tmp"),
            xdg_data: child_path("xdg-data"),
            xdg_config: child_path("xdg-config"),
            game_home: child_path("horizon"),
            launcher_home: child_path("launcher"),
            root,
        };
        for path in [
            &scratch.home,
            &scratch.profile,
            &scratch.temp,
            &scratch.xdg_data,
            &scratch.xdg_config,
            &scratch.game_home,
            &scratch.launcher_home,
        ] {
            if let Err(error) = fs::create_dir(path) {
                let _ = fs::remove_dir_all(&scratch.root);
                return Err(BridgeError::InvalidConfig(format!(
                    "could not prepare private helper folders: {error}"
                )));
            }
        }
        Ok(scratch)
    }
}

impl Drop for ChildScratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn create_private_dir(path: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}

struct ChildGuard {
    child: Child,
    _scratch: ChildScratch,
}

impl Deref for ChildGuard {
    type Target = Child;

    fn deref(&self) -> &Self::Target {
        &self.child
    }
}

impl DerefMut for ChildGuard {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.child
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn finish_child(
    child: &mut Child,
    status: &Arc<Mutex<BridgeStatus>>,
    graceful: bool,
    mut stream: Option<&mut TcpStream>,
) {
    if graceful {
        if let Some(stream) = stream.as_mut() {
            let deadline = Instant::now() + Duration::from_millis(200);
            let sent = write_frame(
                stream,
                &HostFrame::Shutdown {
                    v: PROTOCOL_VERSION,
                },
                MAX_CLIENT_FRAME_BYTES,
                deadline,
            )
            .is_ok();
            if sent {
                let acknowledged = read_frame::<HelperFrame>(
                    stream,
                    MAX_HELPER_FRAME_BYTES,
                    deadline,
                )
                .is_ok_and(|frame| {
                    matches!(frame, HelperFrame::ShutdownAck { v } if v == PROTOCOL_VERSION)
                });
                if acknowledged {
                    while Instant::now() < deadline {
                        match child.try_wait() {
                            Ok(Some(_)) => break,
                            Ok(None) => pause_poll(),
                            Err(_) => break,
                        }
                    }
                }
            }
        }
    }
    if !matches!(child.try_wait(), Ok(Some(_))) {
        let _ = child.kill();
    }
    let _ = child.wait();
    if !matches!(status_snapshot(status), BridgeStatus::Failed(_)) {
        set_status(status, BridgeStatus::Stopped);
    }
}

fn absolute_file(path: &Path, label: &str) -> Result<PathBuf, BridgeError> {
    if !path.is_absolute() {
        return Err(BridgeError::InvalidConfig(format!(
            "{label} path must be absolute"
        )));
    }
    let canonical = fs::canonicalize(path).map_err(|error| {
        BridgeError::InvalidConfig(format!("{label} path cannot be read: {error}"))
    })?;
    if !canonical.is_file() {
        return Err(BridgeError::InvalidConfig(format!(
            "{label} path is not a file"
        )));
    }
    Ok(canonical)
}

fn absolute_dir(path: &Path, label: &str) -> Result<PathBuf, BridgeError> {
    if !path.is_absolute() {
        return Err(BridgeError::InvalidConfig(format!(
            "{label} path must be absolute"
        )));
    }
    let canonical = fs::canonicalize(path).map_err(|error| {
        BridgeError::InvalidConfig(format!("{label} path cannot be opened: {error}"))
    })?;
    if !canonical.is_dir() {
        return Err(BridgeError::InvalidConfig(format!(
            "{label} path is not a directory"
        )));
    }
    Ok(canonical)
}

fn random_token() -> Result<String, BridgeError> {
    let mut token = [0_u8; 32];
    getrandom::fill(&mut token).map_err(|error| {
        BridgeError::Io(format!("could not create a private helper token: {error}"))
    })?;
    let mut text = String::with_capacity(token.len() * 2);
    for byte in token {
        use fmt::Write as _;
        let _ = write!(text, "{byte:02x}");
    }
    Ok(text)
}

fn bounded_text(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let suffix = "…";
    let prefix_limit = max_bytes.saturating_sub(suffix.len());
    let mut end = prefix_limit.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{suffix}", &text[..end])
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let mut difference = left.len() ^ right.len();
    for index in 0..left.len().max(right.len()) {
        difference |= usize::from(
            left.get(index).copied().unwrap_or(0) ^ right.get(index).copied().unwrap_or(0),
        );
    }
    difference == 0
}

fn set_status(status: &Arc<Mutex<BridgeStatus>>, next: BridgeStatus) {
    if let Ok(mut status) = status.lock() {
        *status = next;
    }
}

fn set_failed(status: &Arc<Mutex<BridgeStatus>>, error: BridgeError) {
    if let Ok(mut status) = status.lock() {
        if !matches!(*status, BridgeStatus::Stopped) {
            *status = BridgeStatus::Failed(error);
        }
    }
}

fn status_snapshot(status: &Arc<Mutex<BridgeStatus>>) -> BridgeStatus {
    status
        .lock()
        .map(|status| status.clone())
        .unwrap_or_else(|_| {
            BridgeStatus::Failed(BridgeError::Io("helper status lock failed".into()))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        net::TcpListener,
        sync::atomic::{AtomicU64, Ordering},
    };

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(1);
            let root = std::env::temp_dir().join(format!(
                "rust-edition-bridge-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&root).unwrap();
            Self(root)
        }

        fn helper(&self, mode: &str) -> Bridge {
            let config = self.0.join("helper-config.txt");
            fs::write(&config, format!("mock:{mode}")).unwrap();
            let mut spawn = SpawnConfig::new(std::env::current_exe().unwrap(), config, &self.0);
            spawn.helper_args = ["--exact", "tests::mock_helper_child_entry", "--nocapture"]
                .into_iter()
                .map(OsString::from)
                .collect();
            Bridge::spawn(spawn).unwrap()
        }

        fn marker(&self) -> PathBuf {
            self.0.join("helper-stopped")
        }

        fn batch_marker(&self) -> PathBuf {
            self.0.join("batch-processed")
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn wait_for_status(bridge: &Bridge, matches: impl Fn(&BridgeStatus) -> bool) -> BridgeStatus {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let status = bridge.status();
            if matches(&status) {
                return status;
            }
            assert!(Instant::now() < deadline, "helper stayed in {status:?}");
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn wait_for_reply(bridge: &Bridge) -> TickResult {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(reply) = bridge.try_recv() {
                return reply;
            }
            assert!(
                !matches!(bridge.status(), BridgeStatus::Failed(_)),
                "helper failed before replying: {:?}",
                bridge.status()
            );
            assert!(Instant::now() < deadline, "helper did not reply in time");
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn mock_helper_child_entry() {
        let Some(config) = std::env::var_os(CHILD_CONFIG_ENV) else {
            return;
        };
        let Ok(contents) = fs::read_to_string(&config) else {
            return;
        };
        let Some(mode) = contents.strip_prefix("mock:") else {
            return;
        };
        let Ok(mut helper) = HelperConnection::connect_from_env() else {
            return;
        };
        if mode == "startup-failure" {
            let _ = helper.fail("Fabric initialization failed");
            return;
        }
        if mode == "slow-ready" {
            thread::sleep(Duration::from_millis(150));
        }
        if mode == "pre-ready-sleep" {
            thread::sleep(Duration::from_secs(30));
            return;
        }
        if mode == "env-check" {
            let isolated = [
                "HOME",
                "USERPROFILE",
                "TMPDIR",
                "TEMP",
                "TMP",
                "XDG_DATA_HOME",
                "XDG_CONFIG_HOME",
                "HORIZON_HOME",
                "HORIZON_LAUNCHER_HOME",
            ]
            .into_iter()
            .all(|key| std::env::var_os(key).is_some_and(|value| PathBuf::from(value).is_dir()));
            let flags = std::env::var("HORIZON_NO_AUDIO").as_deref() == Ok("1")
                && std::env::var("HORIZON_NO_HEARTBEAT").as_deref() == Ok("1")
                && std::env::var("HORIZON_NO_DISCORD").as_deref() == Ok("1")
                && std::env::var("HORIZON_TELEMETRY_URL").as_deref() == Ok("off")
                && std::env::var("HORIZON_LAUNCHER_NO_SELF_UPDATE").as_deref() == Ok("1");
            #[cfg(unix)]
            let no_inherited_path = std::env::var_os("PATH").is_none();
            #[cfg(windows)]
            let no_inherited_path = std::env::var_os("PATH").is_none();
            if !isolated || !flags || !no_inherited_path {
                let _ = helper.fail("helper environment was not isolated");
                return;
            }
        }
        if helper.mark_ready().is_err() {
            return;
        }
        if mode == "idle-failure" {
            let _ = helper.fail("runtime callback failed while idle");
            return;
        }
        if mode == "delayed-batch" {
            let Some(parent) = Path::new(&config).parent() else {
                return;
            };
            let release = parent.join("release-batch");
            let deadline = Instant::now() + Duration::from_secs(5);
            while !release.exists() {
                if Instant::now() >= deadline {
                    let _ = helper.fail("delayed queue test was not released");
                    return;
                }
                thread::sleep(Duration::from_millis(5));
            }
        }
        let mut completed_requests = 0;
        loop {
            match helper.receive() {
                Ok(HelperRequest::Shutdown) => {
                    if let Some(parent) = Path::new(&config).parent() {
                        let _ = fs::write(parent.join("helper-stopped"), "stopped");
                    }
                    return;
                }
                Ok(HelperRequest::Tick { seq, snapshot }) => {
                    if mode == "stall-tick" {
                        thread::sleep(Duration::from_secs(30));
                        return;
                    }
                    if mode == "tick-failure" {
                        let _ = helper.fail("runtime callback failed");
                        return;
                    }
                    let Ok(value) = snapshot_json(seq, &snapshot) else {
                        return;
                    };
                    if helper
                        .send_result(seq, ResultStatus::Ok, Some(value))
                        .is_err()
                    {
                        return;
                    }
                    completed_requests += 1;
                    if mode == "delayed-batch" && completed_requests == MAX_OUTSTANDING {
                        if let Some(parent) = Path::new(&config).parent() {
                            let _ = fs::write(parent.join("batch-processed"), "done");
                        }
                    }
                }
                Ok(HelperRequest::State {
                    seq,
                    reason,
                    snapshot,
                }) => {
                    let Ok(value) = state_json(seq, reason, &snapshot) else {
                        return;
                    };
                    if helper
                        .send_result(seq, ResultStatus::Ok, Some(value))
                        .is_err()
                    {
                        return;
                    }
                    completed_requests += 1;
                    if mode == "delayed-batch" && completed_requests == MAX_OUTSTANDING {
                        if let Some(parent) = Path::new(&config).parent() {
                            let _ = fs::write(parent.join("batch-processed"), "done");
                        }
                    }
                }
                Err(_) => return,
            }
        }
    }

    #[test]
    fn snapshots_are_strict_finite_and_have_stable_tick_json() {
        let snapshot = Snapshot::default();
        assert_eq!(
            snapshot_json(7, &snapshot).unwrap(),
            r#"{"kind":"tick","v":2,"seq":7,"world_open":false,"framebuffer":{"width":0,"height":0}}"#
        );
        assert_eq!(
            state_json(8, StateReason::WorldClosed, &snapshot).unwrap(),
            r#"{"kind":"state","v":2,"seq":8,"reason":"world_closed","world_open":false,"framebuffer":{"width":0,"height":0}}"#
        );
        let open_world = Snapshot {
            world_open: true,
            ..Snapshot::default()
        };
        assert!(state_json(9, StateReason::Title, &open_world).is_err());
        assert!(state_json(9, StateReason::WorldClosed, &open_world).is_err());
        assert!(state_json(9, StateReason::Initial, &open_world).is_ok());
        let bad: Result<HostFrame, _> =
            serde_json::from_str(r#"{"kind":"ready","v":1,"extra":true}"#);
        assert!(bad.is_err());

        let bad_player = PlayerSnapshot {
            x: f64::NAN,
            y: 0.0,
            z: 0.0,
            yaw: 0.0,
            pitch: 0.0,
            health: 20.0,
            dimension: "minecraft:overworld".into(),
        };
        assert!(
            Snapshot {
                world_open: true,
                player: Some(bad_player),
                clock: None,
                ..Snapshot::default()
            }
            .validate()
            .is_err()
        );
        let long_dimension = PlayerSnapshot {
            x: 0.0,
            y: 0.0,
            z: 0.0,
            yaw: 0.0,
            pitch: 0.0,
            health: 20.0,
            dimension: "x".repeat(MAX_DIMENSION_BYTES + 1),
        };
        assert!(
            Snapshot {
                world_open: true,
                player: Some(long_dimension),
                clock: None,
                ..Snapshot::default()
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn startup_waits_for_bootstrap_and_round_trips_the_exact_input() {
        let dir = TestDir::new();
        let mut bridge = dir.helper("slow-ready");
        assert_eq!(bridge.status(), BridgeStatus::Starting);
        assert_eq!(
            bridge.try_send(Snapshot::default()),
            Err(BridgeError::NotReady)
        );
        wait_for_status(&bridge, BridgeStatus::is_ready);

        let snapshot = Snapshot::default();
        let seq = bridge.try_send(snapshot.clone()).unwrap();
        let reply = wait_for_reply(&bridge);
        assert_eq!(seq, 1);
        assert_eq!(reply.seq, seq);
        assert_eq!(reply.status, ResultStatus::Ok);
        assert_eq!(reply.snapshot_json, snapshot_json(seq, &snapshot).unwrap());
        assert_eq!(reply.value.as_deref(), Some(reply.snapshot_json.as_str()));

        bridge.shutdown();
        assert_eq!(bridge.status(), BridgeStatus::Stopped);
        assert!(
            dir.marker().exists(),
            "helper did not exit after shutdown ack"
        );
    }

    #[test]
    fn state_frames_share_sequence_and_keep_their_kind() {
        let dir = TestDir::new();
        let mut bridge = dir.helper("echo");
        wait_for_status(&bridge, BridgeStatus::is_ready);
        let snapshot = Snapshot::default();

        let initial = bridge
            .try_send_state(StateReason::Initial, snapshot.clone())
            .unwrap();
        let tick = bridge.try_send_tick(snapshot.clone()).unwrap();
        let closed = bridge
            .try_send_state(StateReason::WorldClosed, snapshot.clone())
            .unwrap();
        assert_eq!([initial, tick, closed], [1, 2, 3]);

        let replies = (0..3).map(|_| wait_for_reply(&bridge)).collect::<Vec<_>>();
        assert_eq!(
            (replies[0].kind, replies[0].state_reason),
            (FrameKind::State, Some(StateReason::Initial))
        );
        assert_eq!(
            replies[0].value.as_deref(),
            Some(
                state_json(initial, StateReason::Initial, &snapshot)
                    .unwrap()
                    .as_str()
            )
        );
        assert_eq!(
            (replies[1].kind, replies[1].state_reason),
            (FrameKind::Tick, None)
        );
        assert_eq!(
            replies[1].value.as_deref(),
            Some(snapshot_json(tick, &snapshot).unwrap().as_str())
        );
        assert_eq!(
            (replies[2].kind, replies[2].state_reason),
            (FrameKind::State, Some(StateReason::WorldClosed))
        );
        assert_eq!(
            replies[2].value.as_deref(),
            Some(
                state_json(closed, StateReason::WorldClosed, &snapshot)
                    .unwrap()
                    .as_str()
            )
        );
        bridge.shutdown();
    }

    #[test]
    fn child_environment_is_private_and_disables_background_services() {
        let dir = TestDir::new();
        let mut bridge = dir.helper("env-check");
        wait_for_status(&bridge, BridgeStatus::is_ready);
        bridge.shutdown();
        assert!(dir.marker().exists());
    }

    #[test]
    fn helper_reports_startup_and_runtime_failures() {
        let startup = TestDir::new();
        let bridge = startup.helper("startup-failure");
        assert_eq!(
            wait_for_status(&bridge, |status| matches!(status, BridgeStatus::Failed(_))),
            BridgeStatus::Failed(BridgeError::HelperFailed(
                "Fabric initialization failed".into()
            ))
        );

        let runtime = TestDir::new();
        let bridge = runtime.helper("idle-failure");
        assert_eq!(
            wait_for_status(&bridge, |status| matches!(status, BridgeStatus::Failed(_))),
            BridgeStatus::Failed(BridgeError::HelperFailed(
                "runtime callback failed while idle".into()
            ))
        );
    }

    #[test]
    fn queue_full_and_slow_callbacks_fail_stop() {
        let dir = TestDir::new();
        let mut bridge = dir.helper("stall-tick");
        wait_for_status(&bridge, BridgeStatus::is_ready);
        for expected in 1..=MAX_OUTSTANDING as u64 {
            assert_eq!(bridge.try_send(Snapshot::default()).unwrap(), expected);
        }
        assert_eq!(
            bridge.try_send(Snapshot::default()),
            Err(BridgeError::QueueFull)
        );
        assert_eq!(
            wait_for_status(&bridge, |status| matches!(status, BridgeStatus::Failed(_))),
            BridgeStatus::Failed(BridgeError::QueueFull)
        );
        bridge.shutdown();

        let dir = TestDir::new();
        let mut bridge = dir.helper("stall-tick");
        wait_for_status(&bridge, BridgeStatus::is_ready);
        bridge.try_send(Snapshot::default()).unwrap();
        assert_eq!(
            wait_for_status(&bridge, |status| matches!(status, BridgeStatus::Failed(_))),
            BridgeStatus::Failed(BridgeError::Timeout("reading a frame"))
        );
    }

    #[test]
    fn delayed_helper_accepts_ten_ticks_and_two_state_frames_then_rejects_more() {
        let dir = TestDir::new();
        let mut bridge = dir.helper("delayed-batch");
        wait_for_status(&bridge, BridgeStatus::is_ready);
        let snapshot = Snapshot::default();

        assert_eq!(
            bridge
                .try_send_state(StateReason::Initial, snapshot.clone())
                .unwrap(),
            1
        );
        for expected in 2..=11 {
            assert_eq!(bridge.try_send_tick(snapshot.clone()).unwrap(), expected);
        }
        assert_eq!(
            bridge
                .try_send_state(StateReason::WorldClosed, snapshot.clone())
                .unwrap(),
            12
        );

        fs::write(dir.0.join("release-batch"), "go").unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while !dir.batch_marker().exists() {
            assert!(
                Instant::now() < deadline,
                "helper did not process all 10 ticks and 2 state frames"
            );
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(bridge.try_send_tick(snapshot), Err(BridgeError::QueueFull));
        assert_eq!(
            wait_for_status(&bridge, |status| matches!(status, BridgeStatus::Failed(_))),
            BridgeStatus::Failed(BridgeError::QueueFull)
        );
        bridge.shutdown();
    }

    #[test]
    fn dropping_during_runtime_boot_cancels_startup_without_waiting_sixty_seconds() {
        let dir = TestDir::new();
        let mut bridge = dir.helper("pre-ready-sleep");
        thread::sleep(Duration::from_millis(100));
        let start = Instant::now();
        bridge.shutdown();
        assert!(start.elapsed() < Duration::from_secs(1));
        assert_eq!(bridge.status(), BridgeStatus::Stopped);
    }

    #[test]
    fn a_partial_frame_cannot_reset_its_absolute_deadline() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let writer = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let payload = serde_json::to_vec(&HelperFrame::Ready {
                v: PROTOCOL_VERSION,
            })
            .unwrap();
            stream
                .write_all(&(payload.len() as u32).to_be_bytes())
                .unwrap();
            for byte in payload {
                thread::sleep(Duration::from_millis(15));
                if stream.write_all(&[byte]).is_err() {
                    break;
                }
            }
        });
        let mut stream = TcpStream::connect(address).unwrap();
        let error = read_frame::<HelperFrame>(
            &mut stream,
            MAX_HELPER_FRAME_BYTES,
            Instant::now() + Duration::from_millis(50),
        )
        .unwrap_err();
        assert_eq!(error, BridgeError::Timeout("reading a frame"));
        writer.join().unwrap();
    }
}
