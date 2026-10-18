//! Running one integration command against Zotero over its socket.
//!
//! [`run_command`] connects, sends the command, then answers each of Zotero's calls through a
//! callback until `Document_complete` (or until Zotero closes the connection, which it does when
//! the user cancels). The callback form lets the app answer on its UI thread;
//! [`run_on_session`] answers directly against a session (CLI, tests).
//!
//! Any process on this machine can listen on Zotero's port before Zotero does, so a session is
//! bounded ([`Limits`]) and can be ended from another thread ([`Cancel`]).

use std::net::{Shutdown, SocketAddr, TcpStream};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use serde_json::Value;
use wordcraft_engine::Session;

use crate::wire::{self, Call};
use crate::{Bridge, Host, ZoteroError};

/// Zotero's integration commands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    /// Add a citation, or edit the one at the caret.
    AddEditCitation,
    /// Add a bibliography, or edit the one at the caret.
    AddEditBibliography,
    /// Insert a Zotero note.
    AddNote,
    /// Update every citation and the bibliography.
    Refresh,
    /// Unlink citations: keep their text, drop the codes.
    RemoveCodes,
    /// Document preferences: citation style, language, note type.
    SetDocPrefs,
}

impl Command {
    pub const ALL: [Command; 6] =
        [Command::AddEditCitation, Command::AddEditBibliography, Command::AddNote, Command::Refresh, Command::RemoveCodes, Command::SetDocPrefs];

    /// The name on the wire.
    pub fn wire_name(self) -> &'static str {
        match self {
            Command::AddEditCitation => "addEditCitation",
            Command::AddEditBibliography => "addEditBibliography",
            Command::AddNote => "addNote",
            Command::Refresh => "refresh",
            Command::RemoveCodes => "removeCodes",
            Command::SetDocPrefs => "setDocPrefs",
        }
    }

    /// By wire name (any case).
    pub fn from_name(name: &str) -> Option<Command> {
        Command::ALL.into_iter().find(|c| c.wire_name().eq_ignore_ascii_case(name))
    }
}

/// Connection settings.
#[derive(Clone, Debug)]
pub struct Options {
    pub addr: SocketAddr,
    /// How long to wait for Zotero to accept the connection.
    pub connect_timeout: Duration,
    /// How long to wait for Zotero's next call. Zotero's dialogs wait for the user, so this is
    /// long; it only ends a session Zotero abandoned without closing the connection (the user
    /// ends one sooner with [`Cancel`]).
    pub idle_timeout: Duration,
    /// How long sending one reply may take; a peer that stops reading ends the session.
    pub write_timeout: Duration,
    pub limits: Limits,
}

impl Default for Options {
    fn default() -> Options {
        Options {
            addr: SocketAddr::from(([127, 0, 0, 1], wire::PORT)),
            connect_timeout: Duration::from_secs(3),
            idle_timeout: Duration::from_secs(30 * 60),
            write_timeout: Duration::from_secs(60),
            limits: Limits::default(),
        }
    }
}

/// Bounds on one session. The defaults leave ample room for Zotero refreshing a long thesis (a
/// few calls per citation, citation codes of a few KB, a bibliography of a few hundred bytes of
/// rich text per entry) and stop a process squatting on Zotero's port from feeding WordCraft
/// without end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Frames Zotero may send in one session.
    pub calls: usize,
    /// Payload bytes Zotero may send in one session.
    pub bytes: usize,
    /// Largest call whose text is parsed as RTF or HTML into the document (`Field_setText`,
    /// `Document_insertText`); a larger one is answered with an error.
    pub rich_text: usize,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits { calls: 100_000, bytes: 128 * 1024 * 1024, rich_text: 4 * 1024 * 1024 }
    }
}

/// Calls whose text is parsed as rich text where they are answered (the app's UI thread).
const RICH_TEXT_CALLS: [&str; 2] = ["Field_setText", "Document_insertText"];

/// Ends a running session from another thread: [`Cancel::cancel`] shuts the socket down, which
/// wakes the session wherever it waits. Clones share one switch.
#[derive(Clone, Debug, Default)]
pub struct Cancel(Arc<Mutex<CancelState>>);

#[derive(Debug, Default)]
struct CancelState {
    cancelled: bool,
    /// The session's own socket (shared, not a duplicated handle, so shutting it down always
    /// reaches the connection the session uses).
    stream: Option<Arc<TcpStream>>,
}

impl Cancel {
    pub fn new() -> Cancel {
        Cancel::default()
    }

    /// End the session: now, or as soon as it connects.
    pub fn cancel(&self) {
        let mut st = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        st.cancelled = true;
        if let Some(s) = st.stream.take() {
            let _ = s.shutdown(Shutdown::Both);
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).cancelled
    }

    /// Watch `stream`; false when the session was already cancelled.
    fn attach(&self, stream: &Arc<TcpStream>) -> bool {
        let mut st = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if st.cancelled {
            return false;
        }
        st.stream = Some(Arc::clone(stream));
        true
    }
}

/// How a session ended.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Outcome {
    /// Calls Zotero made.
    pub calls: usize,
    /// Zotero finished with `Document_complete` (false: it closed the connection, e.g. the
    /// user cancelled its dialog).
    pub completed: bool,
    /// Calls we answered with an error.
    pub errors: Vec<String>,
    /// The session was ended with [`Cancel`].
    pub cancelled: bool,
}

/// Run `command`, answering every call with `answer`.
pub fn run_command(opts: &Options, command: Command, answer: &mut dyn FnMut(&Call) -> Result<Value, String>) -> Result<Outcome, ZoteroError> {
    run_cancellable(opts, command, &Cancel::new(), answer)
}

/// [`run_command`] that `cancel` can end from another thread. A cancelled session is `Ok` with
/// [`Outcome::cancelled`] set, whatever state it left the connection in.
pub fn run_cancellable(
    opts: &Options,
    command: Command,
    cancel: &Cancel,
    answer: &mut dyn FnMut(&Call) -> Result<Value, String>,
) -> Result<Outcome, ZoteroError> {
    let mut out = Outcome::default();
    if cancel.is_cancelled() {
        out.cancelled = true;
        return Ok(out);
    }
    let stream = TcpStream::connect_timeout(&opts.addr, opts.connect_timeout).map_err(|e| ZoteroError::NotRunning(e.to_string()))?;
    let _ = stream.set_nodelay(true);
    stream.set_read_timeout(Some(opts.idle_timeout)).map_err(|e| ZoteroError::Io(e.to_string()))?;
    stream.set_write_timeout(Some(opts.write_timeout)).map_err(|e| ZoteroError::Io(e.to_string()))?;
    let stream = Arc::new(stream);
    if !cancel.attach(&stream) {
        out.cancelled = true;
        return Ok(out);
    }
    // `&TcpStream` reads and writes, so the session and `cancel` share one socket.
    let mut io: &TcpStream = &stream;
    let r =
        wire::write_frame(&mut io, 0, &wire::command_payload(command.wire_name())).and_then(|()| serve_into(&mut io, &opts.limits, &mut out, answer));
    if cancel.is_cancelled() {
        out.cancelled = true;
        out.completed = false;
        return Ok(out);
    }
    r.map(|()| out)
}

/// Answer calls on an open connection until the session ends or goes past `limits`.
pub fn serve<S: std::io::Read + std::io::Write>(
    stream: &mut S,
    limits: &Limits,
    answer: &mut dyn FnMut(&Call) -> Result<Value, String>,
) -> Result<Outcome, ZoteroError> {
    let mut out = Outcome::default();
    serve_into(stream, limits, &mut out, answer).map(|()| out)
}

fn serve_into<S: std::io::Read + std::io::Write>(
    stream: &mut S,
    limits: &Limits,
    out: &mut Outcome,
    answer: &mut dyn FnMut(&Call) -> Result<Value, String>,
) -> Result<(), ZoteroError> {
    let (mut frames, mut bytes) = (0usize, 0usize);
    while let Some(frame) = wire::read_frame(stream)? {
        frames = frames.saturating_add(1);
        bytes = bytes.saturating_add(frame.payload.len());
        if frames > limits.calls {
            return Err(ZoteroError::Protocol(format!("more than {} calls in one command; the session was ended", limits.calls)));
        }
        if bytes > limits.bytes {
            return Err(ZoteroError::Protocol(format!("more than {} bytes in one command; the session was ended", limits.bytes)));
        }
        if frame.txid == 0 {
            // Not a call; nothing to answer.
            continue;
        }
        out.calls += 1;
        let (reply, done) = match Call::parse(&frame.payload) {
            Ok(call) if RICH_TEXT_CALLS.contains(&call.method.as_str()) && frame.payload.len() > limits.rich_text => {
                (Err(format!("{}: {} bytes of text is over the {}-byte limit", call.method, frame.payload.len(), limits.rich_text)), false)
            }
            Ok(call) => {
                log::debug!("Zotero → {}", call.method);
                let r = answer(&call);
                (r, call.method == "Document_complete")
            }
            Err(e) => (Err(e), false),
        };
        if let Err(e) = &reply {
            log::warn!("Zotero call failed: {e}");
            out.errors.push(e.clone());
        }
        wire::write_frame(stream, frame.txid, &wire::encode_reply(&reply))?;
        if done {
            out.completed = true;
            break;
        }
    }
    Ok(())
}

/// Run `command` against `session` directly.
pub fn run_on_session(
    opts: &Options,
    command: Command,
    bridge: &mut Bridge,
    session: &mut Session,
    host: &mut dyn Host,
) -> Result<Outcome, ZoteroError> {
    run_command(opts, command, &mut |call| bridge.handle(session, host, call))
}
