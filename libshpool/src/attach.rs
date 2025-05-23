// Copyright 2023 Google LLC
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::{env, fmt, io, path::PathBuf, thread, time};

use anyhow::{anyhow, bail, Context};
use shpool_protocol::{
    AttachHeader, AttachReplyHeader, ConnectHeader, DetachReply, DetachRequest, ResizeReply,
    ResizeRequest, SessionMessageReply, SessionMessageRequest, SessionMessageRequestPayload,
    TtySize,
};
use tracing::{error, info, warn};

use super::{config, duration, protocol, protocol::ClientResult, test_hooks, tty::TtySizeExt as _};

const MAX_FORCE_RETRIES: usize = 20;

pub fn run(
    config_manager: config::Manager,
    name: String,
    force: bool,
    ttl: Option<String>,
    cmd: Option<String>,
    socket: PathBuf,
) -> anyhow::Result<()> {
    info!("\n\n======================== STARTING ATTACH ============================\n\n");
    test_hooks::emit("attach-startup");

    if name.is_empty() {
        eprintln!("blank session names are not allowed");
        return Ok(());
    }
    if name.contains(char::is_whitespace) {
        eprintln!("whitespace is not allowed in session names");
        return Ok(());
    }

    SignalHandler::new(name.clone(), socket.clone()).spawn()?;

    let ttl = match &ttl {
        Some(src) => match duration::parse(src.as_str()) {
            Ok(d) => Some(d),
            Err(e) => {
                bail!("could not parse ttl: {:?}", e);
            }
        },
        None => None,
    };

    let mut detached = false;
    let mut tries = 0;
    while let Err(err) = do_attach(&config_manager, name.as_str(), &ttl, &cmd, &socket) {
        match err.downcast() {
            Ok(BusyError) if !force => {
                eprintln!("session '{name}' already has a terminal attached");
                return Ok(());
            }
            Ok(BusyError) => {
                if !detached {
                    let mut client = dial_client(&socket)?;
                    client
                        .write_connect_header(ConnectHeader::Detach(DetachRequest {
                            sessions: vec![name.clone()],
                        }))
                        .context("writing detach request header")?;
                    let detach_reply: DetachReply = client.read_reply().context("reading reply")?;
                    if !detach_reply.not_found_sessions.is_empty() {
                        warn!("could not find session '{}' to detach it", name);
                    }

                    detached = true;
                }
                thread::sleep(time::Duration::from_millis(100));

                if tries > MAX_FORCE_RETRIES {
                    eprintln!("session '{name}' already has a terminal which remains attached even after attempting to detach it");
                    return Err(anyhow!("could not detach session, forced attach failed"));
                }
                tries += 1;
            }
            Err(err) => return Err(err),
        }
    }

    Ok(())
}

#[derive(Debug)]
struct BusyError;
impl fmt::Display for BusyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "BusyError")
    }
}
impl std::error::Error for BusyError {}

fn do_attach(
    config: &config::Manager,
    name: &str,
    ttl: &Option<time::Duration>,
    cmd: &Option<String>,
    socket: &PathBuf,
) -> anyhow::Result<()> {
    let mut client = dial_client(socket)?;

    let tty_size = match TtySize::from_fd(0) {
        Ok(s) => s,
        Err(e) => {
            warn!("stdin is not a tty, using default size (err: {e:?})");
            TtySize { rows: 24, cols: 80, xpixel: 0, ypixel: 0 }
        }
    };

    let forward_env = config.get().forward_env.clone();
    let mut local_env_keys = vec!["TERM", "DISPLAY", "LANG", "SSH_AUTH_SOCK"];
    if let Some(fenv) = &forward_env {
        for var in fenv.iter() {
            local_env_keys.push(var);
        }
    }

    client
        .write_connect_header(ConnectHeader::Attach(AttachHeader {
            name: String::from(name),
            local_tty_size: tty_size,
            local_env: local_env_keys
                .into_iter()
                .filter_map(|var| {
                    let val = env::var(var).context("resolving var").ok()?;
                    Some((String::from(var), val))
                })
                .collect::<Vec<_>>(),
            ttl_secs: ttl.map(|d| d.as_secs()),
            cmd: cmd.clone(),
        }))
        .context("writing attach header")?;

    let attach_resp: AttachReplyHeader = client.read_reply().context("reading attach reply")?;
    info!("attach_resp.status={:?}", attach_resp.status);

    {
        use shpool_protocol::AttachStatus::*;
        match attach_resp.status {
            Busy => {
                return Err(BusyError.into());
            }
            Forbidden(reason) => {
                eprintln!("forbidden: {reason}");
                return Err(anyhow!("forbidden: {reason}"));
            }
            Attached { warnings } => {
                for warning in warnings.into_iter() {
                    eprintln!("shpool: warn: {warning}");
                }
                info!("attached to an existing session: '{}'", name);
            }
            Created { warnings } => {
                for warning in warnings.into_iter() {
                    eprintln!("shpool: warn: {warning}");
                }
                info!("created a new session: '{}'", name);
            }
            UnexpectedError(err) => {
                return Err(anyhow!("BUG: unexpected error attaching to '{}': {}", name, err));
            }
        }
    }

    match client.pipe_bytes() {
        Ok(exit_status) => std::process::exit(exit_status),
        Err(e) => Err(e),
    }
}

fn dial_client(socket: &PathBuf) -> anyhow::Result<protocol::Client> {
    match protocol::Client::new(socket) {
        Ok(ClientResult::JustClient(c)) => Ok(c),
        Ok(ClientResult::VersionMismatch { warning, client }) => {
            eprintln!("warning: {warning}, try restarting your daemon");
            eprintln!("hit enter to continue anyway or ^C to exit");

            let _ = io::stdin()
                .lines()
                .next()
                .context("waiting for a continue through a version mismatch")?;

            Ok(client)
        }
        Err(err) => {
            let io_err = err.downcast::<io::Error>()?;
            if io_err.kind() == io::ErrorKind::NotFound {
                eprintln!("could not connect to daemon");
            }
            Err(io_err).context("connecting to daemon")
        }
    }
}

//
// Signal Handling
//

struct SignalHandler {
    session_name: String,
    socket: PathBuf,
}

impl SignalHandler {
    fn new(session_name: String, socket: PathBuf) -> Self {
        SignalHandler { session_name, socket }
    }

    fn spawn(self) -> anyhow::Result<()> {
        use signal_hook::{consts::*, iterator::*};

        let sigs = vec![SIGWINCH];
        let mut signals = Signals::new(sigs).context("creating signal iterator")?;

        thread::spawn(move || {
            for signal in &mut signals {
                let res = match signal {
                    SIGWINCH => self.handle_sigwinch(),
                    sig => {
                        error!("unknown signal: {}", sig);
                        panic!("unknown signal: {sig}");
                    }
                };
                if let Err(e) = res {
                    error!("signal handler error: {:?}", e);
                }
            }
        });

        Ok(())
    }

    fn handle_sigwinch(&self) -> anyhow::Result<()> {
        info!("handle_sigwinch: enter");
        let mut client = match protocol::Client::new(&self.socket)? {
            ClientResult::JustClient(c) => c,
            // At this point, we've already warned the user and they
            // chose to continue anyway, so we shouldn't bother them
            // again.
            ClientResult::VersionMismatch { client, .. } => client,
        };

        let tty_size = TtySize::from_fd(0).context("getting tty size")?;
        info!("handle_sigwinch: tty_size={:?}", tty_size);

        // write the request on a new, seperate connection
        client
            .write_connect_header(ConnectHeader::SessionMessage(SessionMessageRequest {
                session_name: self.session_name.clone(),
                payload: SessionMessageRequestPayload::Resize(ResizeRequest {
                    tty_size: tty_size.clone(),
                }),
            }))
            .context("writing resize request")?;

        let reply: SessionMessageReply =
            client.read_reply().context("reading session message reply")?;
        match reply {
            SessionMessageReply::NotFound => {
                warn!(
                    "handle_sigwinch: sent resize for session '{}', but the daemon has no record of that session",
                    self.session_name
                );
            }
            SessionMessageReply::Resize(ResizeReply::Ok) => {
                info!("handle_sigwinch: resized session '{}' to {:?}", self.session_name, tty_size);
            }
            reply => {
                warn!("handle_sigwinch: unexpected resize reply: {:?}", reply);
            }
        }

        Ok(())
    }
}
