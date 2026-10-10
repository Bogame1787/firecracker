//! Client for the block daemon's control socket.
//!
//! The protocol is one JSON object per line, one reply per request. The daemon
//! serves requests one at a time, so a call blocks until the work is done
//! rather than returning once it is queued.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use serde::Deserialize;

/// A revert can take hundreds of milliseconds and the VM is paused throughout,
/// so there is no value in giving up early.
const TIMEOUT: Duration = Duration::from_secs(300);

/// Errors reaching or commanding the block daemon.
#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum BlkControlError {
    /// Cannot reach the block daemon at {0}: {1}
    Connect(String, std::io::Error),
    /// Cannot talk to the block daemon: {0}
    Io(std::io::Error),
    /// Cannot parse the block daemon's reply: {0}
    Decode(serde_json::Error),
    /// The block daemon refused the request: {0}
    Refused(String),
    /// The disk was reverted but the host page cache was not invalidated, so the guest may still read the previous contents
    NotInvalidated,
}

#[derive(Debug, Deserialize)]
struct Reply {
    ok: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    result: Option<Outcome>,
}

#[derive(Debug, Deserialize)]
struct Outcome {
    /// Whether the daemon dropped the host page cache after acting. Separate
    /// from `ok` because a revert whose invalidation failed has left stale
    /// bytes reachable, which is a failed revert.
    #[serde(default)]
    invalidated: bool,
    #[serde(default)]
    micros: u64,
}

/// Sends one request, requiring that the daemon both acted and invalidated.
fn call(socket: &Path, op: &str) -> Result<u64, BlkControlError> {
    let stream = UnixStream::connect(socket)
        .map_err(|e| BlkControlError::Connect(socket.display().to_string(), e))?;
    stream
        .set_read_timeout(Some(TIMEOUT))
        .map_err(BlkControlError::Io)?;
    stream
        .set_write_timeout(Some(TIMEOUT))
        .map_err(BlkControlError::Io)?;

    let mut out = stream.try_clone().map_err(BlkControlError::Io)?;
    out.write_all(format!("{{\"op\":\"{op}\"}}\n").as_bytes())
        .map_err(BlkControlError::Io)?;
    out.flush().map_err(BlkControlError::Io)?;

    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .map_err(BlkControlError::Io)?;

    let reply: Reply = serde_json::from_str(line.trim()).map_err(BlkControlError::Decode)?;
    if !reply.ok {
        return Err(BlkControlError::Refused(
            reply.error.unwrap_or_else(|| "no reason given".to_string()),
        ));
    }
    let outcome = reply.result.ok_or_else(|| {
        BlkControlError::Refused("reply said ok but carried no result".to_string())
    })?;
    if !outcome.invalidated {
        return Err(BlkControlError::NotInvalidated);
    }
    Ok(outcome.micros)
}

/// Makes the backing file's current contents the state a reset returns to.
///
/// Must happen in the same paused window as the memory snapshot. The guest
/// caches disk contents in its own RAM and a reset rewinds that cache, so a
/// reference taken later leaves the guest holding metadata for blocks the disk
/// no longer has.
pub fn mark(socket: &Path) -> Result<u64, BlkControlError> {
    call(socket, "mark")
}

/// Reverts the backing file to the reference point.
pub fn reset(socket: &Path) -> Result<u64, BlkControlError> {
    call(socket, "reset")
}
