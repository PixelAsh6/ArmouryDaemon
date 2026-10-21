//! Named-pipe server. One accept thread; one handler thread per connection.
//! Events flow to the main loop via a crossbeam channel.

use armoury_core::protocol::{write_msg, Request, Response};
use crossbeam_channel::Sender;
use interprocess::local_socket::{prelude::*, Listener, Stream};
use log::{info, warn};
use std::io::{self, BufRead, BufReader};
use std::thread;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::Threading::SetEvent;

pub enum PipeEvent {
    Connected { id: u64, push: Sender<Response> },
    Request { id: u64, req: Request },
    Disconnected { id: u64 },
}

/// The largest message the daemon reads from a client.
///
/// The pipe is a security boundary: a client that keeps sending without a newline must not
/// make the daemon grow a buffer until the machine runs out of memory. A longer message is
/// a bad message, and a bad message drops the connection.
const MAX_MESSAGE_BYTES: usize = 1024 * 1024;

/// Spawn the accept loop on a dedicated thread. `wake` is the daemon's wake event
/// (raw handle as usize) so pipe traffic is handled without waiting for the 1 s tick.
pub fn start(listener: Listener, tx: Sender<PipeEvent>, wake: usize) {
    thread::spawn(move || {
        let mut next_id: u64 = 1;
        for conn in listener.incoming() {
            match conn {
                Ok(stream) => {
                    let id = next_id;
                    next_id += 1;
                    let tx = tx.clone();
                    thread::spawn(move || handle_conn(stream, id, tx, wake));
                }
                Err(e) => warn!("pipe accept error: {e}"),
            }
        }
    });
}

fn signal(wake: usize) {
    if wake != 0 {
        unsafe { SetEvent(wake as HANDLE) };
    }
}

fn handle_conn(stream: Stream, id: u64, tx: Sender<PipeEvent>, wake: usize) {
    let (recv_half, send_half) = stream.split();

    // Writer thread: daemon -> client.
    // Unbounded so a slow/stuck client can never block the daemon main loop.
    let (push_tx, push_rx) = crossbeam_channel::unbounded::<Response>();
    let writer = thread::spawn(move || {
        let mut w = send_half;
        while let Ok(msg) = push_rx.recv() {
            if write_msg(&mut w, &msg).is_err() {
                break;
            }
        }
    });

    let _ = tx.send(PipeEvent::Connected {
        id,
        push: push_tx.clone(),
    });
    signal(wake);

    // Reader loop (this thread): client -> daemon.
    let mut reader = BufReader::new(recv_half);
    loop {
        match read_request(&mut reader, MAX_MESSAGE_BYTES) {
            Ok(Some(req)) => {
                if tx.send(PipeEvent::Request { id, req }).is_err() {
                    break;
                }
                signal(wake);
            }
            Ok(None) => break, // clean EOF: client closed the pipe
            Err(e) => {
                warn!("pipe client {id}: bad message ({e}); dropping connection");
                break;
            }
        }
    }

    let _ = tx.send(PipeEvent::Disconnected { id });
    signal(wake);
    drop(push_tx);
    let _ = writer.join();
    info!("pipe client {id} handler finished");
}

/// Read one request from the client, but never more than `limit` bytes of it.
///
/// The message is one line of JSON. Reading it into a String without a limit is how a
/// client takes the daemon down: a line that never ends grows the buffer until the machine
/// runs out of memory. So the read stops at `limit` and reports the message as bad, and a
/// bad message drops the connection — the same way a malformed one does.
///
/// `Ok(None)` is a clean end of the pipe.
fn read_request<R: BufRead>(reader: &mut R, limit: usize) -> io::Result<Option<Request>> {
    let mut line: Vec<u8> = Vec::new();
    loop {
        let available = match reader.fill_buf() {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if available.is_empty() {
            // The client closed the pipe. A message that is half read is a bad message.
            return if line.is_empty() {
                Ok(None)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "the client closed the pipe in the middle of a message",
                ))
            };
        }
        match available.iter().position(|&b| b == b'\n') {
            Some(at) => {
                if line.len() + at + 1 > limit {
                    return Err(too_long(limit));
                }
                line.extend_from_slice(&available[..=at]);
                let used = at + 1;
                let text = std::mem::take(&mut line);
                reader.consume(used);
                return match parse_request(&text) {
                    Ok(Some(req)) => Ok(Some(req)),
                    Ok(None) => continue, // a blank line is not a message
                    Err(e) => Err(e),
                };
            }
            None => {
                if line.len() + available.len() > limit {
                    return Err(too_long(limit));
                }
                line.extend_from_slice(available);
                let used = available.len();
                reader.consume(used);
            }
        }
    }
}

/// The request a line of the wire carries. A blank line carries none.
///
/// The line has to be text: a byte that is not UTF-8 makes the message bad, like a line
/// that is not JSON.
fn parse_request(text: &[u8]) -> io::Result<Option<Request>> {
    let text = std::str::from_utf8(text)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let trimmed = text.trim_end_matches(['\r', '\n']);
    if trimmed.trim().is_empty() {
        return Ok(None);
    }
    serde_json::from_str(trimmed)
        .map(Some)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// The error for a message that is longer than the daemon reads.
fn too_long(limit: usize) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("the message is longer than {limit} bytes"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use armoury_core::status::StatusSnapshot;
    use interprocess::local_socket::{ConnectOptions, GenericNamespaced, ListenerOptions};
    use std::collections::HashMap;
    use std::io::Write;
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    /// How long a client waits for an answer. Generous: the daemon answers in milliseconds,
    /// and a busy machine must not fail the test.
    const ANSWER: Duration = Duration::from_secs(15);

    /// The messages the battery sends. Every one of them has to be refused, and the server
    /// has to stay up and keep serving.
    fn hostile_messages() -> Vec<(&'static str, Vec<u8>)> {
        let big = "A".repeat(4 * 1024 * 1024);
        vec![
            (
                "a message that is not JSON at all",
                b"this is not json\n".to_vec(),
            ),
            (
                "JSON that is not a request",
                b"{\"hello\":\"world\"}\n".to_vec(),
            ),
            (
                "a request with a field of the wrong type",
                b"{\"type\":\"set_paused\",\"paused\":\"yes\"}\n".to_vec(),
            ),
            (
                "a request with a missing field",
                b"{\"type\":\"set_paused\"}\n".to_vec(),
            ),
            (
                "an unknown request",
                b"{\"type\":\"explode_the_daemon\"}\n".to_vec(),
            ),
            (
                "a request with enormous strings",
                format!(
                    "{{\"type\":\"add_tracked_app\",\"name\":\"{big}\",\"exe\":\"{big}\"}}\n"
                )
                .into_bytes(),
            ),
        ]
    }

    /// What came of a wait for a line.
    enum Wait {
        Line(String),
        End,
        Timeout,
    }

    /// A client of the pipe. The reading happens on its own thread, so a server that does
    /// not answer cannot hang the test.
    struct Client {
        send: interprocess::local_socket::SendHalf,
        lines: crossbeam_channel::Receiver<Option<String>>,
    }

    impl Client {
        /// Connect to the pipe, or `None` when it does not answer.
        fn connect(pipe: &str) -> Option<Client> {
            let name = pipe.to_ns_name::<GenericNamespaced>().ok()?;
            let stream = ConnectOptions::new().name(name).connect_sync().ok()?;
            let (recv, send) = stream.split();
            let (tx, lines) = crossbeam_channel::unbounded();
            thread::spawn(move || {
                let mut reader = BufReader::new(recv);
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line) {
                        Ok(0) => {
                            let _ = tx.send(None); // the pipe ended
                            return;
                        }
                        Ok(_) => {
                            if tx.send(Some(line.trim_end().to_string())).is_err() {
                                return;
                            }
                        }
                        Err(_) => {
                            let _ = tx.send(None);
                            return;
                        }
                    }
                }
            });
            Some(Client { send, lines })
        }

        /// Send raw bytes. The server may hang up in the middle, so the caller has to be
        /// ready for an error.
        fn send(&mut self, bytes: &[u8]) -> io::Result<()> {
            self.send.write_all(bytes)
        }

        /// Send one line of the wire.
        fn send_line(&mut self, text: &str) -> io::Result<()> {
            self.send.write_all(text.as_bytes())?;
            self.send.write_all(b"\n")
        }

        /// The next line of the wire.
        fn next_line(&self, wait: Duration) -> Wait {
            match self.lines.recv_timeout(wait) {
                Ok(Some(text)) => Wait::Line(text),
                Ok(None) => Wait::End,
                Err(_) => Wait::Timeout,
            }
        }

        /// The next status the server sends, skipping the other messages.
        fn next_status(&self, wait: Duration) -> Option<StatusSnapshot> {
            let deadline = Instant::now() + wait;
            while Instant::now() < deadline {
                match self.next_line(deadline.saturating_duration_since(Instant::now())) {
                    Wait::Line(text) => {
                        if let Ok(Response::Status { status }) = serde_json::from_str::<Response>(&text)
                        {
                            return Some(status);
                        }
                    }
                    Wait::End | Wait::Timeout => return None,
                }
            }
            None
        }

        /// Ask for the status and wait for the answer.
        fn status(&mut self) -> Option<StatusSnapshot> {
            self.send_line(r#"{"type":"get_status"}"#).ok()?;
            self.next_status(ANSWER)
        }
    }

    /// A namespace of its own for the throw-away server: the pipe, the mutex and the config
    /// folder all carry it, so the installed daemon is never touched.
    fn namespace() -> String {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        format!("rusttest-{}-{n}", std::process::id())
    }

    /// The `packs` folder the daemon would create next to its executable when there is none.
    /// The daemon creates it on startup, so the test has to take it away again.
    fn daemon_packs_folder() -> Option<PathBuf> {
        let exe = std::env::current_exe().ok()?;
        let dir = exe.parent()?;
        if dir.join("packs").is_dir() {
            return None; // it was already there: it is not ours to remove
        }
        let up2 = dir.parent().and_then(|d| d.parent());
        if let Some(up2) = up2
            && up2.join("packs").is_dir()
        {
            return None; // the daemon uses that one
        }
        Some(dir.join("packs"))
    }

    /// The throw-away daemon: the daemon itself, in a namespace of its own, so it gets its
    /// own mutex, its own pipe and its own config folder.
    ///
    /// It is this test binary, asked for the daemon test only: the executable in
    /// `target\debug` is not used, because `cargo test` does not rebuild it — it would be
    /// the daemon of an earlier build, and the test would prove nothing about this code.
    struct RealDaemon {
        child: std::process::Child,
        pipe: String,
        config_dir: PathBuf,
        packs: Option<PathBuf>,
    }

    impl RealDaemon {
        /// Start the daemon.
        fn start(ns: &str) -> Option<RealDaemon> {
            let exe = std::env::current_exe().ok()?;
            let config_dir = std::env::temp_dir().join(format!("ArmouryDaemon-test-{ns}"));
            std::fs::create_dir_all(&config_dir).ok()?;
            // The folder belongs to the throw-away daemon: if it never starts, it is taken
            // away again here, and nothing is left behind in the temp folder.
            let Some(child) = Self::spawn(&exe, ns, &config_dir) else {
                let _ = std::fs::remove_dir_all(&config_dir);
                return None;
            };
            Some(RealDaemon {
                child,
                pipe: format!("armoury-daemon.pipe-{ns}"),
                config_dir,
                packs: daemon_packs_folder(),
            })
        }

        /// Start the throw-away daemon, with its output in the config folder.
        fn spawn(
            exe: &std::path::Path,
            ns: &str,
            config_dir: &std::path::Path,
        ) -> Option<std::process::Child> {
            let out = std::fs::File::create(config_dir.join("daemon.out")).ok()?;
            let err = std::fs::File::create(config_dir.join("daemon.err")).ok()?;
            Command::new(exe)
                .arg("--exact")
                .arg("ipc::tests::the_daemon_role")
                .arg("--nocapture")
                .env("ARMOURY_TEST_NS", ns)
                .env("ARMOURY_TEST_DAEMON", "1")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::from(out))
                .stderr(std::process::Stdio::from(err))
                .spawn()
                .ok()
        }

        /// What the daemon wrote: the last lines of its log and whatever it printed. It is
        /// the explanation when the daemon did not survive.
        fn diagnostics(&self) -> String {
            let mut out = String::new();
            for name in ["logs/daemon.log", "daemon.out", "daemon.err"] {
                let path = self.config_dir.join(name);
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let lines: Vec<&str> = text.lines().rev().take(6).collect();
                let lines: Vec<&str> = lines.into_iter().rev().collect();
                out.push_str(&format!("--- {name} ---\n{}\n", lines.join("\n")));
            }
            out
        }
    }

    impl Drop for RealDaemon {
        fn drop(&mut self) {
            // The daemon must not outlive the test.
            let _ = self.child.kill();
            let _ = self.child.wait();
            let _ = std::fs::remove_dir_all(&self.config_dir);
            if let Some(packs) = &self.packs {
                let _ = std::fs::remove_dir_all(packs);
            }
        }
    }

    /// The pipe server of this process, with a loop that answers what the battery asks. It
    /// is the same `ipc::start` the daemon uses, so the hostile input reaches the same code.
    ///
    /// It stands in for the throw-away daemon only when the test asks for it
    /// (`ARMOURY_TEST_INPROCESS`): the battery is then against this process instead.
    struct InProcess {
        pipe: String,
    }

    impl InProcess {
        fn start(ns: &str) -> InProcess {
            let pipe = format!("armoury-daemon.pipe-{ns}");
            let name = pipe
                .clone()
                .to_ns_name::<GenericNamespaced>()
                .expect("the pipe name");
            let listener = ListenerOptions::new()
                .name(name)
                .create_sync()
                .expect("the pipe");
            let (tx, rx) = crossbeam_channel::unbounded::<PipeEvent>();
            let server_cooldown = Arc::new(Mutex::new(0u64));
            start(listener, tx, 0);
            thread::spawn(move || {
                let mut clients: HashMap<u64, Sender<Response>> = HashMap::new();
                while let Ok(event) = rx.recv() {
                    match event {
                        PipeEvent::Connected { id, push } => {
                            clients.insert(id, push);
                        }
                        PipeEvent::Disconnected { id } => {
                            clients.remove(&id);
                        }
                        PipeEvent::Request { id, req } => {
                            let mut changed = None;
                            if let Request::SetCooldown { seconds } = req {
                                let seconds = seconds.clamp(5, 3600);
                                *server_cooldown.lock().expect("the state") = seconds;
                                changed = Some(seconds);
                            }
                            let status = status_of(*server_cooldown.lock().expect("the state"));
                            match changed {
                                // the daemon tells every client about the new state
                                Some(_) => {
                                    for push in clients.values() {
                                        let _ = push.send(Response::Status {
                                            status: status.clone(),
                                        });
                                    }
                                }
                                None => {
                                    if let Some(push) = clients.get(&id) {
                                        let _ = push.send(Response::Status { status });
                                    }
                                }
                            }
                        }
                    }
                }
            });
            InProcess { pipe }
        }
    }

    /// A status with nothing in it: every field of a status has a default.
    fn status_of(cooldown_seconds: u64) -> StatusSnapshot {
        let mut status: StatusSnapshot =
            serde_json::from_str("{}").expect("an empty status is a status");
        status.cooldown_seconds = cooldown_seconds;
        status
    }

    /// The server the battery talks to: the real daemon, or the pipe server of this process.
    enum Server {
        Real(RealDaemon),
        InProcess(InProcess),
    }

    impl Server {
        fn pipe(&self) -> &str {
            match self {
                Server::Real(daemon) => &daemon.pipe,
                Server::InProcess(server) => &server.pipe,
            }
        }

        /// A client of the pipe.
        fn client(&self) -> Option<Client> {
            Client::connect(self.pipe())
        }

        /// The number of threads the server has, or `None` when it cannot be counted (the
        /// pipe server of this process shares the threads of the test run).
        fn threads(&self) -> Option<usize> {
            match self {
                Server::Real(daemon) => Some(thread_count(daemon.child.id())),
                Server::InProcess(_) => None,
            }
        }

        /// What the server has to say for itself: the explanation when it did not survive.
        fn diagnostics(&self) -> String {
            match self {
                Server::Real(daemon) => daemon.diagnostics(),
                Server::InProcess(_) => String::new(),
            }
        }
    }

    /// The number of threads the process with this id has.
    fn thread_count(pid: u32) -> usize {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Thread32First, Thread32Next, THREADENTRY32, TH32CS_SNAPTHREAD,
        };
        let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
        if snap.is_null() {
            return 0;
        }
        let mut entry: THREADENTRY32 = unsafe { std::mem::zeroed() };
        entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
        let mut count = 0;
        let mut ok = unsafe { Thread32First(snap, &mut entry) } != 0;
        while ok {
            if entry.th32OwnerProcessID == pid {
                count += 1;
            }
            ok = unsafe { Thread32Next(snap, &mut entry) } != 0;
        }
        unsafe { CloseHandle(snap) };
        count
    }

    /// Wait until the server answers, or give up.
    fn wait_for(server: &Server) -> bool {
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if let Some(mut client) = server.client() {
                if client.status().is_some() {
                    return true;
                }
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        false
    }

    /// Drive the server with hostile input. It has to survive all of it: it stays up, it
    /// keeps serving, it keeps its state, and it does not leave a thread behind for every
    /// connection that came and went.
    fn battery(server: &Server) {
        // A good client, connected before the battery and still good after it.
        let mut good = server.client().expect("a client");
        assert!(
            good.status().is_some(),
            "the server answers before the battery"
        );
        good.send_line(r#"{"type":"set_cooldown","seconds":37}"#)
            .expect("the request");
        let status = good.next_status(ANSWER).expect("the new state");
        assert_eq!(status.cooldown_seconds, 37, "the state is set");

        let threads_before = server.threads();

        for (label, bytes) in hostile_messages() {
            let mut bad = server.client().expect("a client");
            // The server hangs up on a message it will not read: the write may fail.
            let _ = bad.send(&bytes);
            drop(bad);

            // The good client is untouched: it is still there and still served.
            assert!(
                good.status().is_some(),
                "the good client was dropped after {label}\n{}",
                server.diagnostics()
            );
            // And a fresh client is served as well.
            let mut fresh = server.client().expect("a client");
            assert!(
                fresh.status().is_some(),
                "the server stopped serving after {label}\n{}",
                server.diagnostics()
            );
        }

        // A message that never ends: the server must not read it forever.
        {
            let mut flood = server.client().expect("a client");
            let chunk = vec![b'A'; 64 * 1024];
            for _ in 0..64 {
                if flood.send(&chunk).is_err() {
                    break; // the server hung up: it refused the message
                }
            }
            let deadline = Instant::now() + ANSWER;
            let mut ended = false;
            while Instant::now() < deadline {
                match flood.next_line(deadline.saturating_duration_since(Instant::now())) {
                    Wait::End => {
                        ended = true;
                        break;
                    }
                    // a broadcast of the daemon: keep waiting for the end of the pipe
                    Wait::Line(_) => continue,
                    Wait::Timeout => break,
                }
            }
            assert!(
                ended,
                "the server kept a message of 4 MiB it will never answer\n{}",
                server.diagnostics()
            );
        }
        assert!(
            good.status().is_some(),
            "the good client was dropped by the endless message\n{}",
            server.diagnostics()
        );

        // A partial line, then the connection is closed in the middle of the message.
        {
            let mut partial = server.client().expect("a client");
            let _ = partial.send(b"{\"type\":\"get_st");
            drop(partial);
        }
        assert!(
            good.status().is_some(),
            "the good client was dropped by the half message\n{}",
            server.diagnostics()
        );

        // Several hundred malformed messages in a row.
        for _ in 0..200 {
            let mut bad = server.client().expect("a client");
            let _ = bad.send(b"not json at all\n");
            drop(bad);
        }
        assert!(
            good.status().is_some(),
            "the good client was dropped by the flood\n{}",
            server.diagnostics()
        );

        // The server is still up, still serving, and still has its state.
        let status = good.status().expect("the server still answers");
        assert_eq!(status.cooldown_seconds, 37, "the server kept its state");

        // No thread is left behind for every connection that came and went.
        if let (Some(before), Some(after)) = (threads_before, server.threads()) {
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut after = after;
            while after > before + 4 && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(200));
                after = server.threads().unwrap_or(after);
            }
            assert!(
                after <= before + 4,
                "the server kept {after} threads after the flood (it had {before})"
            );
        }
    }

    /// The daemon role of this test binary.
    ///
    /// The throw-away daemon is this binary asked for this one test: the executable in
    /// `target\debug` is not used, because `cargo test` does not rebuild it — it would be
    /// the daemon of an earlier build, and the test would prove nothing about this code.
    #[test]
    fn the_daemon_role() {
        // The daemon runs only when this binary was started for this test alone: an
        // ordinary run of the test suite must not turn into a daemon.
        let asked_for_this_test = std::env::args()
            .any(|arg| arg == "ipc::tests::the_daemon_role");
        if !asked_for_this_test || std::env::var_os("ARMOURY_TEST_DAEMON").is_none() {
            return;
        }
        crate::daemon::run(crate::daemon::Start::Autostart);
    }

    #[test]
    fn the_pipe_survives_hostile_input() {
        let ns = namespace();
        // The daemon itself, in a namespace of its own. The pipe server of this
        // process only takes its place when the test asks for it: a silent fallback
        // would leave the real daemon, its pipe, its mutex and its config folder
        // untested, and the test green all the same.
        let server = match RealDaemon::start(&ns) {
            Some(daemon) => Server::Real(daemon),
            None if std::env::var_os("ARMOURY_TEST_INPROCESS").is_some() => {
                eprintln!(
                    "the daemon could not be started: the pipe server of this process is \
                     used instead"
                );
                Server::InProcess(InProcess::start(&ns))
            }
            None => panic!("the throw-away daemon could not be started"),
        };
        assert!(
            wait_for(&server),
            "the server answers on its pipe\n{}",
            server.diagnostics()
        );
        eprintln!(
            "the battery runs against the {}",
            match server {
                Server::Real(_) => "throw-away daemon",
                Server::InProcess(_) => "pipe server of this process",
            }
        );
        battery(&server);
    }

    #[test]
    fn a_message_that_is_too_long_is_refused_without_reading_it_all() {
        // A client that keeps sending without a newline must not make the daemon grow a
        // buffer until the machine runs out of memory.
        let mut data = vec![b'A'; MAX_MESSAGE_BYTES * 4];
        data.push(b'\n');
        let mut cursor = io::Cursor::new(data);
        {
            let mut reader = BufReader::new(&mut cursor);
            let err = read_request(&mut reader, MAX_MESSAGE_BYTES)
                .expect_err("the message is too long");
            assert!(err.to_string().contains("longer than"), "{err}");
        }
        assert!(
            cursor.position() <= MAX_MESSAGE_BYTES as u64 + 8192,
            "the daemon read {} bytes of a message it was going to refuse",
            cursor.position()
        );
    }

    #[test]
    fn a_message_within_the_limit_is_read() {
        // The cap must not touch the messages the daemon really gets.
        let line = r#"{"type":"get_status"}"#;
        let mut reader = BufReader::new(io::Cursor::new(format!("{line}\n").into_bytes()));
        let request = read_request(&mut reader, MAX_MESSAGE_BYTES)
            .expect("the message is read")
            .expect("there is a message");
        assert!(matches!(request, Request::GetStatus));
    }

    #[test]
    fn a_blank_line_is_not_a_message() {
        let mut reader = BufReader::new(io::Cursor::new(b"\n\n{\"type\":\"get_status\"}\n".to_vec()));
        let request = read_request(&mut reader, MAX_MESSAGE_BYTES)
            .expect("the message is read")
            .expect("there is a message");
        assert!(matches!(request, Request::GetStatus));
    }

    #[test]
    fn the_end_of_the_pipe_is_not_a_message() {
        let mut reader = BufReader::new(io::Cursor::new(Vec::<u8>::new()));
        assert!(
            read_request(&mut reader, MAX_MESSAGE_BYTES)
                .expect("the pipe ended")
                .is_none()
        );
    }

    #[test]
    fn a_message_that_is_not_text_is_refused() {
        // A byte that is not UTF-8 makes the message bad: it is dropped like any other
        // bad message, not patched up.
        let mut data = b"{\"type\":\"get_st".to_vec();
        data.push(0xff);
        data.push(b'\n');
        let mut reader = BufReader::new(io::Cursor::new(data));
        let err = read_request(&mut reader, MAX_MESSAGE_BYTES)
            .expect_err("the message is not text");
        assert!(err.to_string().contains("utf-8"), "{err}");
    }
}
