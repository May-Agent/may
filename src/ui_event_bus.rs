use std::fmt;
use std::str::FromStr;

use tokio::sync::broadcast;

const UI_EVENT_BUS_CAPACITY: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum UiEventTopic {
    Config = 1 << 0,
    RequestLogs = 1 << 1,
    NotificationMailbox = 1 << 2,
    NetworkAccessLogs = 1 << 3,
    ContainerPrepare = 1 << 4,
    BrowserStatus = 1 << 5,
    SessionRuntime = 1 << 6,
    SessionRuntimes = 1 << 7,
    ProfileSessions = 1 << 8,
    RemoteAccess = 1 << 9,
    BrowserLifecycle = 1 << 10,
}

impl UiEventTopic {
    const fn bit(self) -> u16 {
        self as u16
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Config => "config",
            Self::RequestLogs => "request_logs",
            Self::NotificationMailbox => "notification_mailbox",
            Self::NetworkAccessLogs => "network_access_logs",
            Self::ContainerPrepare => "container_prepare",
            Self::BrowserStatus => "browser_status",
            Self::SessionRuntime => "session_runtime",
            Self::SessionRuntimes => "session_runtimes",
            Self::ProfileSessions => "profile_sessions",
            Self::RemoteAccess => "remote_access",
            Self::BrowserLifecycle => "browser_lifecycle",
        }
    }
}

impl FromStr for UiEventTopic {
    type Err = UnknownUiEventTopic;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "config" => Ok(Self::Config),
            "request_logs" => Ok(Self::RequestLogs),
            "notification_mailbox" => Ok(Self::NotificationMailbox),
            "network_access_logs" => Ok(Self::NetworkAccessLogs),
            "container_prepare" => Ok(Self::ContainerPrepare),
            "browser_status" => Ok(Self::BrowserStatus),
            "session_runtime" => Ok(Self::SessionRuntime),
            "session_runtimes" => Ok(Self::SessionRuntimes),
            "profile_sessions" => Ok(Self::ProfileSessions),
            "remote_access" => Ok(Self::RemoteAccess),
            "browser_lifecycle" => Ok(Self::BrowserLifecycle),
            _ => Err(UnknownUiEventTopic(value.to_string())),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct UiEventTopics(u16);

impl UiEventTopics {
    pub const EMPTY: Self = Self(0);
    pub const CONFIG: Self = Self(UiEventTopic::Config.bit());
    pub const REQUEST_LOGS: Self = Self(UiEventTopic::RequestLogs.bit());
    pub const NOTIFICATION_MAILBOX: Self = Self(UiEventTopic::NotificationMailbox.bit());
    pub const NETWORK_ACCESS_LOGS: Self = Self(UiEventTopic::NetworkAccessLogs.bit());
    pub const CONTAINER_PREPARE: Self = Self(UiEventTopic::ContainerPrepare.bit());
    pub const BROWSER_STATUS: Self = Self(UiEventTopic::BrowserStatus.bit());
    pub const SESSION_RUNTIME: Self = Self(UiEventTopic::SessionRuntime.bit());
    pub const SESSION_RUNTIMES: Self = Self(UiEventTopic::SessionRuntimes.bit());
    pub const PROFILE_SESSIONS: Self = Self(UiEventTopic::ProfileSessions.bit());
    pub const REMOTE_ACCESS: Self = Self(UiEventTopic::RemoteAccess.bit());
    pub const BROWSER_LIFECYCLE: Self = Self(UiEventTopic::BrowserLifecycle.bit());
    pub const ALL: Self = Self(
        Self::CONFIG.0
            | Self::REQUEST_LOGS.0
            | Self::NOTIFICATION_MAILBOX.0
            | Self::NETWORK_ACCESS_LOGS.0
            | Self::CONTAINER_PREPARE.0
            | Self::BROWSER_STATUS.0
            | Self::SESSION_RUNTIME.0
            | Self::SESSION_RUNTIMES.0
            | Self::PROFILE_SESSIONS.0
            | Self::REMOTE_ACCESS.0
            | Self::BROWSER_LIFECYCLE.0,
    );

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub const fn intersects(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub fn from_csv(csv: &str) -> Result<Self, UnknownUiEventTopic> {
        let mut topics = Self::EMPTY;
        for raw in csv.split(',') {
            let value = raw.trim();
            if value.is_empty() {
                continue;
            }
            topics = topics.union(Self::from(UiEventTopic::from_str(value)?));
        }
        Ok(topics)
    }
}

impl From<UiEventTopic> for UiEventTopics {
    fn from(value: UiEventTopic) -> Self {
        Self(value.bit())
    }
}

impl std::ops::BitAnd for UiEventTopics {
    type Output = Self;

    fn bitand(self, rhs: Self) -> Self::Output {
        Self(self.0 & rhs.0)
    }
}

impl std::ops::BitAndAssign for UiEventTopics {
    fn bitand_assign(&mut self, rhs: Self) {
        self.0 &= rhs.0;
    }
}

impl std::ops::BitOr for UiEventTopics {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        self.union(rhs)
    }
}

impl std::ops::BitOrAssign for UiEventTopics {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

impl std::ops::BitOr<UiEventTopic> for UiEventTopics {
    type Output = Self;

    fn bitor(self, rhs: UiEventTopic) -> Self::Output {
        self.union(Self::from(rhs))
    }
}

impl std::ops::BitOr<UiEventTopic> for UiEventTopic {
    type Output = UiEventTopics;

    fn bitor(self, rhs: UiEventTopic) -> Self::Output {
        UiEventTopics::from(self).union(UiEventTopics::from(rhs))
    }
}

#[derive(Debug, Clone)]
pub struct UiEventBus {
    tx: broadcast::Sender<UiEventTopics>,
}

impl UiEventBus {
    #[must_use]
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(UI_EVENT_BUS_CAPACITY);
        Self { tx }
    }

    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<UiEventTopics> {
        self.tx.subscribe()
    }

    pub fn publish(&self, topics: UiEventTopics) {
        if topics.is_empty() {
            return;
        }
        let _ = self.tx.send(topics);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownUiEventTopic(String);

impl fmt::Display for UnknownUiEventTopic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown UI event topic: {}", self.0)
    }
}

impl std::error::Error for UnknownUiEventTopic {}
