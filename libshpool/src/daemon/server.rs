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

use std::{
    collections::HashMap,
    env, fs, io, net,
    ops::Add,
    os,
    os::unix::{
        fs::PermissionsExt,
        io::AsRawFd,
        net::{UnixListener, UnixStream},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process,
    sync::{Arc, Mutex},
    thread, time,
    time::{Duration, Instant},
};

use anyhow::{anyhow, Context};
use nix::unistd::Pid;
use tracing::{error, info, instrument, span, trace, warn, Level};

use super::{
    super::{consts, protocol, test_hooks, tty},
    config, etc_environment, shell, ttl_reaper, user,
};
use crate::daemon::exit_notify::ExitNotifier;

const STDERR_FD: i32 = 2;
const DEFAULT_INITIAL_SHELL_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";
const DEFAULT_OUTPUT_SPOOL_LINES: usize = 500;

#[derive(Debug)]
pub struct Server {
    config: config::Config,
    /// A map from shell session names to session descriptors.
    /// We wrap this in Arc<Mutex<_>> so that we can get at the
    /// table from different threads such as the SIGWINCH thread
    /// that is spawned during the attach process, and so that
    /// handle_conn can delegate to worker threads and quickly allow
    /// the main thread to become available to accept new connections.
    shells: Arc<Mutex<HashMap<String, Box<shell::Session>>>>,
    runtime_dir: PathBuf,
    register_new_reapable_session: crossbeam_channel::Sender<(String, Instant)>,
}

impl Server {
    #[instrument(skip_all)]
    pub fn new(config: config::Config, runtime_dir: PathBuf) -> Arc<Self> {
        let shells = Arc::new(Mutex::new(HashMap::new()));
        // buffered so that we are unlikely to block when setting up a
        // new session
        let (new_sess_tx, new_sess_rx) = crossbeam_channel::bounded(10);
        let shells_tab = Arc::clone(&shells);
        thread::spawn(move || {
            if let Err(e) = ttl_reaper::run(new_sess_rx, shells_tab) {
                warn!("ttl reaper exited with error: {:?}", e);
            }
        });

        Arc::new(Server { config, shells, runtime_dir, register_new_reapable_session: new_sess_tx })
    }

    #[instrument(skip_all)]
    pub fn serve(server: Arc<Self>, listener: UnixListener) -> anyhow::Result<()> {
        test_hooks::emit("daemon-about-to-listen");
        let mut conn_counter = 0;
        for stream in listener.incoming() {
            info!("socket got a new connection");
            match stream {
                Ok(stream) => {
                    conn_counter += 1;
                    let conn_id = conn_counter;
                    let server = Arc::clone(&server);
                    thread::spawn(move || {
                        if let Err(err) = server.handle_conn(stream, conn_id) {
                            error!("handling new connection: {:?}", err)
                        }
                    });
                }
                Err(err) => {
                    error!("accepting stream: {:?}", err);
                }
            }
        }

        Ok(())
    }

    #[instrument(skip_all, fields(cid = conn_id))]
    fn handle_conn(&self, mut stream: UnixStream, conn_id: usize) -> anyhow::Result<()> {
        // We want to avoid timing out while blocking the main thread.
        stream
            .set_read_timeout(Some(consts::SOCK_STREAM_TIMEOUT))
            .context("setting read timout on inbound session")?;

        let header = parse_connect_header(&mut stream).context("parsing connect header")?;

        let warnings = match check_peer(&stream) {
            Ok(warnings) => warnings,
            Err(err) => {
                if let protocol::ConnectHeader::Attach(_) = header {
                    write_reply(
                        &mut stream,
                        protocol::AttachReplyHeader {
                            status: protocol::AttachStatus::Forbidden(format!("{:?}", err)),
                        },
                    )?;
                }
                stream.shutdown(net::Shutdown::Both).context("closing stream")?;
                return Err(err);
            }
        };
        info!("checked peer with warnings: {:?}", warnings);

        // Unset the read timeout before we pass things off to a
        // worker thread because it is perfectly fine for there to
        // be no new data for long periods of time when the users
        // is connected to a shell session.
        stream.set_read_timeout(None).context("unsetting read timout on inbound session")?;

        match header {
            protocol::ConnectHeader::Attach(h) => self.handle_attach(stream, conn_id, h, warnings),
            protocol::ConnectHeader::Detach(r) => self.handle_detach(stream, r),
            protocol::ConnectHeader::Kill(r) => self.handle_kill(stream, r),
            protocol::ConnectHeader::List => self.handle_list(stream),
            protocol::ConnectHeader::SessionMessage(header) => {
                self.handle_session_message(stream, header)
            }
        }
    }

    #[instrument(skip_all)]
    fn handle_attach(
        &self,
        mut stream: UnixStream,
        conn_id: usize,
        header: protocol::AttachHeader,
        warnings: Vec<String>,
    ) -> anyhow::Result<()> {
        let (child_exit_notifier, inner_to_stream, status) = {
            // we unwrap to propagate the poison as an unwind
            let mut shells = self.shells.lock().unwrap();
            info!("locked shells table");

            let mut status = protocol::AttachStatus::Attached { warnings: warnings.clone() };
            if let Some(session) = shells.get(&header.name) {
                info!("found entry for '{}'", header.name);
                if let Ok(mut inner) = session.inner.try_lock() {
                    info!("session '{}': locked inner", header.name);
                    // We have an existing session in our table, but the subshell
                    // proc might have exited in the meantime, for example if the
                    // user typed `exit` right before the connection dropped there
                    // could be a zombie entry in our session table. We need to
                    // re-check whether the subshell has exited before taking this over.
                    //
                    // N.B. this is still technically a race, but in practice it does
                    // not ever cause problems, and there is no real way to avoid some
                    // sort of race without just always creating a new session when
                    // a shell exits, which would break `exit` typed at the shell prompt.
                    match session.child_exit_notifier.wait(Some(time::Duration::from_millis(0))) {
                        None => {
                            // the channel is still open so the subshell is still running
                            info!("taking over existing session inner");
                            inner.client_stream = Some(stream.try_clone()?);

                            if inner
                                .reader_join_h
                                .as_ref()
                                .map(|h| h.is_finished())
                                .unwrap_or(false)
                            {
                                warn!(
                                    "child_exited chan unclosed, but reader thread has exited, clobbering with new subshell"
                                );
                                status = protocol::AttachStatus::Created { warnings };
                            }

                            // status is already attached
                        }
                        Some(exit_status) => {
                            // the channel is closed so we know the subshell exited
                            info!(
                                "stale inner={:?}, (child exited with status {}) clobbering with new subshell",
                                inner, exit_status
                            );
                            status = protocol::AttachStatus::Created { warnings };
                        }
                    }

                    if inner.reader_join_h.as_ref().map(|h| h.is_finished()).unwrap_or(false) {
                        info!("reader thread finished, joining");
                        if let Some(h) = inner.reader_join_h.take() {
                            h.join()
                                .map_err(|e| anyhow!("joining reader on reattach: {:?}", e))?
                                .context("within reader thread on reattach")?;
                        }
                        assert!(matches!(status, protocol::AttachStatus::Created { .. }));
                    }

                    // fallthrough to bidi streaming
                } else {
                    info!("busy shell session, doing nothing");
                    // The stream is busy, so we just inform the client and close the stream.
                    write_reply(
                        &mut stream,
                        protocol::AttachReplyHeader { status: protocol::AttachStatus::Busy },
                    )?;
                    stream.shutdown(net::Shutdown::Both).context("closing stream")?;
                    return Ok(());
                }
            } else {
                info!("no existing '{}' session, creating new one", &header.name);
                status = protocol::AttachStatus::Created { warnings };
            }

            if matches!(status, protocol::AttachStatus::Created { .. }) {
                info!("creating new subshell");
                let session = self.spawn_subshell(conn_id, stream, &header)?;

                shells.insert(header.name.clone(), Box::new(session));
                // fallthrough to bidi streaming
            }

            // return a reference to the inner session so that
            // we can work with it without the global session
            // table lock held
            if let Some(session) = shells.get(&header.name) {
                (
                    Some(Arc::clone(&session.child_exit_notifier)),
                    Some(Arc::clone(&session.inner)),
                    status,
                )
            } else {
                (None, None, status)
            }
        };
        info!("released lock on shells table");

        self.link_ssh_auth_sock(&header).context("linking SSH_AUTH_SOCK")?;

        if let (Some(child_exit_notifier), Some(inner)) = (child_exit_notifier, inner_to_stream) {
            let mut child_done = false;
            let mut inner = inner.lock().unwrap();
            let client_stream = match inner.client_stream.as_mut() {
                Some(s) => s,
                None => {
                    return Err(anyhow!("no client stream, should be impossible"));
                }
            };

            let reply_status = write_reply(client_stream, protocol::AttachReplyHeader { status });
            if let Err(e) = reply_status {
                error!("error writing reply status: {:?}", e);
            }

            info!("starting bidi stream loop");
            match inner.bidi_stream(conn_id, header.local_tty_size.clone(), child_exit_notifier) {
                Ok(done) => {
                    child_done = done;
                }
                Err(e) => {
                    error!("error shuffling bytes: {:?}", e);
                }
            }
            info!("bidi stream loop finished");

            if child_done {
                info!("'{}' exited, removing from session table", header.name);
                let mut shells = self.shells.lock().unwrap();
                shells.remove(&header.name);
            }
            if child_done {
                // The child shell has exited, so the reader thread should
                // attempt to read from its stdout and get an error, causing
                // it to exit. That means we should be safe to join. We use
                // a seperate if statement to avoid holding the shells lock
                // while we join the old thread.
                if let Some(h) = inner.reader_join_h.take() {
                    h.join()
                        .map_err(|e| anyhow!("joining reader after child exit: {:?}", e))?
                        .context("within reader thread after child exit")?;
                }
            }

            info!("finished attach streaming section");
        } else {
            error!("internal error: failed to fetch just inserted session");
        }

        Ok(())
    }

    #[instrument(skip_all)]
    fn link_ssh_auth_sock(&self, header: &protocol::AttachHeader) -> anyhow::Result<()> {
        if self.config.nosymlink_ssh_auth_sock.unwrap_or(false) {
            return Ok(());
        }

        if let Some(ssh_auth_sock) = header.local_env_get("SSH_AUTH_SOCK") {
            let symlink = self.ssh_auth_sock_symlink(PathBuf::from(&header.name));
            fs::create_dir_all(symlink.parent().ok_or(anyhow!("no symlink parent dir"))?)
                .context("could not create directory for SSH_AUTH_SOCK symlink")?;

            let sessions_dir =
                symlink.parent().and_then(|d| d.parent()).ok_or(anyhow!("no sessions dir"))?;
            let sessions_meta = fs::metadata(sessions_dir).context("stating sessions dir")?;

            // set RWX bits for user and no one else
            let mut sessions_perm = sessions_meta.permissions();
            if sessions_perm.mode() != 0o700 {
                sessions_perm.set_mode(0o700);
                fs::set_permissions(sessions_dir, sessions_perm)
                    .context("locking down permissions for sessions dir")?;
            }

            let _ = fs::remove_file(&symlink); // clean up the link if it exists already
            os::unix::fs::symlink(ssh_auth_sock, &symlink).context(format!(
                "could not symlink '{:?}' to point to '{:?}'",
                symlink, ssh_auth_sock
            ))?;
        } else {
            info!("no SSH_AUTH_SOCK in client env, leaving it unlinked");
        }

        Ok(())
    }

    #[instrument(skip_all)]
    fn handle_detach(
        &self,
        mut stream: UnixStream,
        request: protocol::DetachRequest,
    ) -> anyhow::Result<()> {
        let mut not_found_sessions = vec![];
        let mut not_attached_sessions = vec![];
        {
            trace!("about to lock shells table 3");
            let shells = self.shells.lock().unwrap();
            trace!("locked shells table 3");
            for session in request.sessions.into_iter() {
                if let Some(s) = shells.get(&session) {
                    let reader_ctl = s.reader_ctl.lock().unwrap();
                    reader_ctl
                        .client_connection
                        .send(shell::ClientConnectionMsg::Disconnect)
                        .context("sending client detach to reader")?;
                    let status = reader_ctl
                        .client_connection_ack
                        .recv()
                        .context("getting client conn ack")?;
                    info!("detached session({}), status = {:?}", session, status);
                    if let shell::ClientConnectionStatus::DetachNone = status {
                        not_attached_sessions.push(session);
                    }
                } else {
                    not_found_sessions.push(String::from(session));
                }
            }
        }

        write_reply(
            &mut stream,
            protocol::DetachReply { not_found_sessions, not_attached_sessions },
        )
        .context("writing detach reply")?;

        Ok(())
    }

    #[instrument(skip_all)]
    fn handle_kill(
        &self,
        mut stream: UnixStream,
        request: protocol::KillRequest,
    ) -> anyhow::Result<()> {
        let mut not_found_sessions = vec![];
        {
            let mut shells = self.shells.lock().unwrap();

            let mut to_remove = Vec::with_capacity(request.sessions.len());
            for session in request.sessions.into_iter() {
                if let Some(s) = shells.get(&session) {
                    s.kill().context("killing shell proc")?;

                    // we don't need to wait since the dedicated reaping thread is active
                    // even when a tty is not attached
                    to_remove.push(session);
                } else {
                    not_found_sessions.push(session);
                }
            }

            for session in to_remove.iter() {
                shells.remove(session);
            }
            if to_remove.len() > 0 {
                test_hooks::emit("daemon-handle-kill-removed-shells");
            }
        }

        write_reply(&mut stream, protocol::KillReply { not_found_sessions })
            .context("writing kill reply")?;

        Ok(())
    }

    #[instrument(skip_all)]
    fn handle_list(&self, mut stream: UnixStream) -> anyhow::Result<()> {
        let shells = self.shells.lock().unwrap();

        let sessions: anyhow::Result<Vec<protocol::Session>> = shells
            .iter()
            .map(|(k, v)| {
                Ok(protocol::Session {
                    name: k.to_string(),
                    started_at_unix_ms: v.started_at.duration_since(time::UNIX_EPOCH)?.as_millis()
                        as i64,
                })
            })
            .collect();
        let sessions = sessions.context("collecting running session metadata")?;

        write_reply(&mut stream, protocol::ListReply { sessions })?;

        Ok(())
    }

    #[instrument(skip_all)]
    fn handle_session_message(
        &self,
        mut stream: UnixStream,
        header: protocol::SessionMessageRequest,
    ) -> anyhow::Result<()> {
        // create a slot to store our reply so we can do
        // our IO without the lock held.
        let reply = {
            let shells = self.shells.lock().unwrap();
            if let Some(session) = shells.get(&header.session_name) {
                match header.payload {
                    protocol::SessionMessageRequestPayload::Resize(resize_request) => {
                        let reader_ctl = session.reader_ctl.lock().unwrap();
                        reader_ctl
                            .tty_size_change
                            .send(resize_request.tty_size)
                            .context("sending tty size change to reader")?;
                        reader_ctl.tty_size_change_ack.recv().context("recving tty size ack")?;
                        protocol::SessionMessageReply::Resize(protocol::ResizeReply::Ok)
                    }
                    protocol::SessionMessageRequestPayload::Detach => {
                        let reader_ctl = session.reader_ctl.lock().unwrap();
                        reader_ctl
                            .client_connection
                            .send(shell::ClientConnectionMsg::Disconnect)
                            .context("sending client detach to reader")?;
                        let status = reader_ctl
                            .client_connection_ack
                            .recv()
                            .context("getting client conn ack")?;
                        info!("detached session({}), status = {:?}", header.session_name, status);
                        protocol::SessionMessageReply::Detach(
                            protocol::SessionMessageDetachReply::Ok,
                        )
                    }
                }
            } else {
                protocol::SessionMessageReply::NotFound
            }
        };

        write_reply(&mut stream, reply).context("handle_session_message: writing reply")?;

        Ok(())
    }

    /// Spawn a subshell and return the sessession descriptor for it. The
    /// session is wrapped in an Arc so the inner session can hold a Weak
    /// back-reference to the session.
    #[instrument(skip_all)]
    fn spawn_subshell(
        &self,
        conn_id: usize,
        client_stream: UnixStream,
        header: &protocol::AttachHeader,
    ) -> anyhow::Result<shell::Session> {
        let user_info = user::info()?;
        let shell = if let Some(s) = &self.config.shell {
            s.clone()
        } else {
            user_info.default_shell.clone()
        };
        info!("user_info={:?}", user_info);

        // Build up the command we will exec while allocation is still chill.
        // We will exec this command after a fork, so we want to just inherit
        // stdout/stderr/stdin. The pty crate automatically `dup2`s the file
        // descriptors for us.
        let mut cmd = process::Command::new(&shell);
        cmd.current_dir(user_info.home_dir.clone())
            .stdin(process::Stdio::inherit())
            .stdout(process::Stdio::inherit())
            .stderr(process::Stdio::inherit())
            // The env should mostly be set up by the shell sourcing
            // rc files and whatnot, so we will start things off with
            // an environment that is blank except for a few vars we inject
            // to avoid breakage and vars the user has asked us to inject.
            .env_clear()
            .env("HOME", user_info.home_dir)
            .env(
                "PATH",
                self.config
                    .initial_path
                    .as_ref()
                    .map(|x| x.as_ref())
                    .unwrap_or(DEFAULT_INITIAL_SHELL_PATH),
            )
            .env("SHPOOL_SESSION_NAME", &header.name)
            .env("USER", user_info.user)
            .env("SSH_AUTH_SOCK", self.ssh_auth_sock_symlink(PathBuf::from(&header.name)));
        if self.config.norc.unwrap_or(false) && shell == "/bin/bash" {
            cmd.arg("--norc").arg("--noprofile");
        }

        if let Ok(xdg_runtime_dir) = env::var("XDG_RUNTIME_DIR") {
            cmd.env("XDG_RUNTIME_DIR", xdg_runtime_dir);
        }

        // Most of the time, use the TERM that the user sent along in
        // the attach header. If they have an explicit TERM value set
        // in their config file, use that instead. If they have a blank
        // term in their config, don't set TERM in the spawned shell at
        // all.
        let mut term = None;
        if let Some(t) = header.local_env_get("TERM") {
            term = Some(String::from(t));
        }
        if let Some(env) = self.config.env.as_ref() {
            term = match env.get("TERM") {
                None => term,
                Some(t) if t.is_empty() => None,
                Some(t) => Some(String::from(t)),
            };

            // If the user has configured a term of "", we want
            // to make sure not to set it at all in the environment.
            // An unset TERM variable can produce a shell that generates
            // output which is easier to parse and interact with for
            // another machine. This is particularly useful for testing
            // shpool itself.
            let filtered_env_pin;
            let env = if term.is_none() {
                let mut e = env.clone();
                e.remove("TERM");
                filtered_env_pin = Some(e);
                filtered_env_pin.as_ref().unwrap()
            } else {
                env
            };

            if env.len() > 0 {
                cmd.envs(env);
            }
        }
        info!("injecting TERM into shell {:?}", term);
        if let Some(t) = term {
            cmd.env("TERM", t);
        }

        // inject all other local variables
        for (var, val) in &header.local_env {
            if var == "TERM" || var == "SSH_AUTH_SOCK" {
                continue;
            }
            cmd.env(var, val);
        }

        // parse and load /etc/environment unless we've been asked not to
        if !self.config.noread_etc_environment.unwrap_or(false) {
            match fs::File::open("/etc/environment") {
                Ok(f) => {
                    let pairs = etc_environment::parse_compat(io::BufReader::new(f))?;
                    for (var, val) in pairs.into_iter() {
                        cmd.env(var, val);
                    }
                }
                Err(e) => {
                    warn!("could not open /etc/environment to load env vars: {:?}", e);
                }
            }
        }

        // spawn the shell as a login shell by setting
        // arg0 to be the basename of the shell path
        // proceeded with a "-". You can see sshd doing the
        // same thing if you look in the session.c file of
        // openssh.
        let shell_basename = Path::new(&shell)
            .file_name()
            .ok_or(anyhow!("error building login shell indicator"))?
            .to_str()
            .ok_or(anyhow!("error parsing shell name as utf8"))?;
        cmd.arg0(format!("-{}", shell_basename));

        let noecho = self.config.noecho.unwrap_or(false);
        info!("about to fork subshell noecho={}", noecho);
        let fork = shpool_pty::fork::Fork::from_ptmx().context("forking pty")?;
        if let Ok(slave) = fork.is_child() {
            if noecho {
                tty::disable_echo(slave.as_raw_fd()).unwrap();
            }
            for fd in STDERR_FD + 1..(nix::unistd::SysconfVar::OPEN_MAX as i32) {
                let _ = nix::unistd::close(fd);
            }
            let err = cmd.exec();
            eprintln!("shell exec err: {:?}", err);
            std::process::exit(1);
        }

        // spawn a background thread to reap the shell when it exits
        // and notify about the exit by closing a channel.
        let child_exit_notifier = Arc::new(ExitNotifier::new());
        let waitable_child = fork.clone();
        let session_name = header.name.clone();
        let notifiable_child_exit_notifier = Arc::clone(&child_exit_notifier);
        thread::spawn(move || {
            let _s = span!(Level::INFO, "child_watcher", s = session_name, cid = conn_id).entered();

            match waitable_child.wait_for_exit() {
                Ok((_, Some(exit_status))) => {
                    info!("child exited with status {}", exit_status);
                    notifiable_child_exit_notifier.notify_exit(exit_status);
                }
                Ok((_, None)) => {
                    info!("child exited without status, using 1");
                    notifiable_child_exit_notifier.notify_exit(1);
                }
                Err(e) => {
                    info!("error waiting on child, using exit status 1: {:?}", e);
                    notifiable_child_exit_notifier.notify_exit(1);
                }
            }
            info!("reaped child shell: {:?}", waitable_child);
        });

        let (client_connection_tx, client_connection_rx) = crossbeam_channel::bounded(0);
        let (client_connection_ack_tx, client_connection_ack_rx) = crossbeam_channel::bounded(0);
        let (tty_size_change_tx, tty_size_change_rx) = crossbeam_channel::bounded(0);
        let (tty_size_change_ack_tx, tty_size_change_ack_rx) = crossbeam_channel::bounded(0);

        let reader_ctl = Arc::new(Mutex::new(shell::ReaderCtl {
            client_connection: client_connection_tx,
            client_connection_ack: client_connection_ack_rx,
            tty_size_change: tty_size_change_tx,
            tty_size_change_ack: tty_size_change_ack_rx,
        }));
        let mut session_inner = shell::SessionInner {
            name: header.name.clone(),
            reader_ctl: Arc::clone(&reader_ctl),
            pty_master: fork,
            client_stream: Some(client_stream),
            config: self.config.clone(),
            reader_join_h: None,
        };
        let child_pid = session_inner.pty_master.child_pid().ok_or(anyhow!("no child pid"))?;
        session_inner.reader_join_h = Some(
            session_inner.spawn_reader(
                conn_id,
                header.local_tty_size.clone(),
                match (self.config.output_spool_lines, &self.config.session_restore_mode) {
                    (Some(l), _) => l,
                    (None, Some(config::SessionRestoreMode::Lines(l))) => *l as usize,
                    (None, _) => DEFAULT_OUTPUT_SPOOL_LINES,
                },
                self.config
                    .session_restore_mode
                    .clone()
                    .unwrap_or(config::SessionRestoreMode::default()),
                client_connection_rx,
                client_connection_ack_tx,
                tty_size_change_rx,
                tty_size_change_ack_tx,
            )?,
        );

        if let Some(ttl_secs) = header.ttl_secs {
            info!("registering session with ttl with the reaper");
            self.register_new_reapable_session
                .send((header.name.clone(), Instant::now().add(Duration::from_secs(ttl_secs))))
                .context("sending reapable session registration msg")?;
        }

        Ok(shell::Session {
            reader_ctl,
            child_pid,
            child_exit_notifier,
            started_at: time::SystemTime::now(),
            inner: Arc::new(Mutex::new(session_inner)),
        })
    }

    fn ssh_auth_sock_symlink(&self, session_name: PathBuf) -> PathBuf {
        self.runtime_dir.join("sessions").join(session_name).join("ssh-auth-sock.socket")
    }
}

#[instrument(skip_all)]
fn parse_connect_header(stream: &mut UnixStream) -> anyhow::Result<protocol::ConnectHeader> {
    let header: protocol::ConnectHeader =
        bincode::deserialize_from(stream).context("parsing header")?;
    Ok(header)
}

#[instrument(skip_all)]
fn write_reply<H>(stream: &mut UnixStream, header: H) -> anyhow::Result<()>
where
    H: serde::Serialize,
{
    stream
        .set_write_timeout(Some(consts::SOCK_STREAM_TIMEOUT))
        .context("setting write timout on inbound session")?;

    let serializeable_stream = stream.try_clone().context("cloning stream handle")?;
    bincode::serialize_into(serializeable_stream, &header).context("writing reply")?;

    stream.set_write_timeout(None).context("unsetting write timout on inbound session")?;
    Ok(())
}

/// check_peer makes sure that a process dialing in on the shpool
/// control socket has the same UID as the current user and that
/// both have the same executable path.
fn check_peer(sock: &UnixStream) -> anyhow::Result<Vec<String>> {
    use nix::{sys::socket, unistd};

    let peer_creds = socket::getsockopt(sock.as_raw_fd(), socket::sockopt::PeerCredentials)
        .context("could not get peer creds from socket")?;
    let peer_uid = unistd::Uid::from_raw(peer_creds.uid());
    let self_uid = unistd::getuid();
    if peer_uid != self_uid {
        return Err(anyhow!("shpool prohibits connections across users"));
    }

    let peer_pid = unistd::Pid::from_raw(peer_creds.pid());
    let self_pid = unistd::getpid();
    let peer_exe = exe_for_pid(peer_pid).context("could not resolve exe from the pid")?;
    let self_exe = exe_for_pid(self_pid).context("could not resolve our own exe")?;
    if peer_exe != self_exe {
        return Ok(vec![String::from("attach binary differs from daemon binary")]);
    }

    Ok(vec![])
}

fn exe_for_pid(pid: Pid) -> anyhow::Result<PathBuf> {
    let path = std::fs::read_link(format!("/proc/{}/exe", pid))?;
    Ok(path)
}