//! Wire protocol for rcargo daemon (`rcargod`).
//!
//! v1 (ssh+rsync) does not need this — it shells out to system tools and reads
//! pipes. v2 (`rcargod` over ssh stdio or WebTransport) uses these types as a
//! line-delimited JSON stream: one [`Request`] in, many [`Event`]s back ending
//! with [`Event::Exit`].

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Protocol version this crate speaks. Bumped on breaking wire changes.
pub const PROTO_VERSION: u32 = 1;

/// One request from client to daemon. Daemon executes it in the project root
/// and streams [`Event`]s back until it sends [`Event::Exit`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// Run `cargo <args>` in the project tree identified by `project_key`.
    Build {
        project_key: String,
        args: Vec<String>,
        #[serde(default)]
        env: HashMap<String, String>,
    },
    /// Ping for connectivity / version negotiation.
    Hello { client_version: u32 },
    /// Shut down the connection cleanly.
    Bye,
}

/// One event streamed from daemon back to client.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    /// stdout chunk (a line, possibly without trailing `\n`).
    Stdout { data: String },
    /// stderr chunk.
    Stderr { data: String },
    /// Hello response with server's supported version.
    HelloAck { server_version: u32 },
    /// Process exited; stream is over.
    Exit { code: i32 },
    /// Out-of-band error before/instead of exit.
    Error { message: String },
}

impl Event {
    /// Encode as one JSON line (no trailing newline — caller adds it).
    pub fn to_line(&self) -> String {
        serde_json::to_string(self).expect("Event serialization is infallible")
    }
}

impl Request {
    pub fn to_line(&self) -> String {
        serde_json::to_string(self).expect("Request serialization is infallible")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_roundtrip() {
        let r = Request::Build {
            project_key: "abc123".into(),
            args: vec!["test".into(), "-p".into(), "foo".into()],
            env: HashMap::new(),
        };
        let line = r.to_line();
        let parsed: Request = serde_json::from_str(&line).unwrap();
        match parsed {
            Request::Build { args, .. } => assert_eq!(args, vec!["test", "-p", "foo"]),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn event_exit_roundtrip() {
        let e = Event::Exit { code: 0 };
        let line = e.to_line();
        let parsed: Event = serde_json::from_str(&line).unwrap();
        assert!(matches!(parsed, Event::Exit { code: 0 }));
    }
}
