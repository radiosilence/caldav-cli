use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("Not configured. Run `caldav-cli auth --username <email>` first.")]
    NotAuthenticated,

    /// Authentication was rejected by the server.
    ///
    /// The inner value is a `&'static str` by design — using a static literal
    /// ensures no call site can accidentally pass the password itself or
    /// another secret into this variant where it would then surface in output.
    #[error("Invalid credentials: {0}")]
    InvalidCredentials(&'static str),

    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("CalDAV error: {method} {url} failed - {status}: {body}")]
    Dav {
        method: String,
        url: String,
        status: u16,
        body: String,
    },

    #[error("Calendar not found: {0}")]
    CalendarNotFound(String),

    #[error("Event not found: {0}")]
    EventNotFound(String),

    /// Discovery walks current-user-principal → calendar-home-set. If either
    /// step yields nothing the server URL or credentials are usually wrong.
    #[error("CalDAV discovery failed: could not find {0}. Check the server URL and credentials.")]
    Discovery(&'static str),

    #[error("Invalid date/time {input:?}: {reason}")]
    InvalidDateTime { input: String, reason: String },

    #[error("Config error: {0}")]
    Config(String),

    #[error("Rate limited. Try again later.")]
    RateLimited,

    #[error("Server error: {0}")]
    Server(String),
}

pub type Result<T> = std::result::Result<T, Error>;
