//! The app side of the local MCP connection.
//!
//! The app listens on `127.0.0.1` at a random port and writes the port and a
//! per-launch token to `mcp.json` under the config root. `wisp --mcp` reads
//! that file and forwards MCP tool calls here over one JSON line per
//! connection.
//!
//! Requests are handed to the UI thread through an `async_channel` (the same
//! pattern the tray uses to post events onto GPUI's executor) because all of
//! the state a tool touches — project rows, the supervisor, install progress —
//! belongs to the `Wisp` entity.

use std::io::{BufRead, BufReader, Read as _, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::mcp::protocol::{EndpointInfo, Request, Response, DISCOVERY_FILE, PROTOCOL_VERSION};

/// A request is a handful of JSON fields; anything this large is a mistake (or
/// an attempt to exhaust memory).
const MAX_REQUEST_BYTES: usize = 1024 * 1024;

/// Long enough for an action that waits on a Windows permission prompt, short
/// enough that a wedged UI does not hang an MCP client forever.
const REPLY_TIMEOUT: Duration = Duration::from_secs(120);

/// One request on its way to the `Wisp` entity.
pub struct Incoming {
    pub request: Request,
    /// The listener thread waits on this; the UI sends exactly one reply.
    pub reply: std::sync::mpsc::Sender<Response>,
}

/// A running loopback listener. Dropping it stops the listener and removes the
/// discovery file.
pub struct Endpoint {
    port: u16,
    token: String,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    /// Whether this endpoint owns the discovery file. Test endpoints do not,
    /// so a test can never delete a running app's `mcp.json`.
    publish: bool,
}

impl Endpoint {
    /// Bind a random loopback port, start accepting, and publish `mcp.json`.
    pub fn start(requests: async_channel::Sender<Incoming>) -> std::io::Result<Self> {
        let endpoint = Self::bind(requests, true)?;
        endpoint.write_discovery()?;
        Ok(endpoint)
    }

    fn bind(requests: async_channel::Sender<Incoming>, publish: bool) -> std::io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        let port = listener.local_addr()?.port();
        let token = uuid::Uuid::new_v4().simple().to_string();
        let stop = Arc::new(AtomicBool::new(false));

        let endpoint = Self {
            port,
            token,
            stop: stop.clone(),
            thread: None,
            publish,
        };

        let worker_token = endpoint.token.clone();
        let thread = std::thread::Builder::new()
            .name("wisp-mcp-endpoint".to_string())
            .spawn(move || accept_loop(listener, worker_token, requests, stop))?;
        let mut endpoint = endpoint;
        endpoint.thread = Some(thread);
        Ok(endpoint)
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    fn write_discovery(&self) -> std::io::Result<()> {
        let info = EndpointInfo {
            version: PROTOCOL_VERSION,
            port: self.port,
            token: self.token.clone(),
            pid: std::process::id(),
            app_version: env!("CARGO_PKG_VERSION").to_string(),
        };
        let path = discovery_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(&info)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        std::fs::write(path, text)
    }
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // `accept` is blocked; one connection to ourselves makes it return so
        // the loop can observe `stop`.
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        if self.publish {
            remove_discovery();
        }
    }
}

/// The discovery file's location. Shared with the bridge.
pub fn discovery_path() -> PathBuf {
    crate::config::root().join(DISCOVERY_FILE)
}

/// Read and validate the discovery file.
pub fn read_discovery() -> std::io::Result<EndpointInfo> {
    let text = std::fs::read_to_string(discovery_path())?;
    let info: EndpointInfo = serde_json::from_str(&text)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    if info.version != PROTOCOL_VERSION {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "Wisp's MCP endpoint speaks protocol {} but this bridge expects {}",
                info.version, PROTOCOL_VERSION
            ),
        ));
    }
    Ok(info)
}

/// Best-effort removal, used when MCP is switched off and when the app exits.
pub fn remove_discovery() {
    let _ = std::fs::remove_file(discovery_path());
}

fn accept_loop(
    listener: TcpListener,
    token: String,
    requests: async_channel::Sender<Incoming>,
    stop: Arc<AtomicBool>,
) {
    for incoming in listener.incoming() {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let Ok(stream) = incoming else {
            continue;
        };
        let token = token.clone();
        let requests = requests.clone();
        // A thread per connection keeps a slow tool call from blocking status
        // reads (the bridge polls while an install or sync is running).
        let _ = std::thread::Builder::new()
            .name("wisp-mcp-request".to_string())
            .spawn(move || handle_connection(stream, &token, &requests));
    }
}

fn handle_connection(stream: TcpStream, token: &str, requests: &async_channel::Sender<Incoming>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));

    let mut line = String::new();
    // `read_line` grows to the newline; cap the bytes read so one oversized
    // request cannot allocate without bound before the size check below.
    let mut reader = BufReader::new(&stream).take((MAX_REQUEST_BYTES + 1) as u64);
    match reader.read_line(&mut line) {
        Ok(0) | Err(_) => return,
        Ok(_) => {}
    }
    if line.len() > MAX_REQUEST_BYTES {
        respond(
            &stream,
            Response::err(0, "bad_request", "The request was too large."),
        );
        return;
    }

    let request: Request = match serde_json::from_str(line.trim()) {
        Ok(request) => request,
        Err(error) => {
            respond(
                &stream,
                Response::err(0, "bad_request", format!("Invalid JSON: {error}")),
            );
            return;
        }
    };
    let id = request.id;

    if request.token != token {
        respond(
            &stream,
            Response::err(
                id,
                "unauthorized",
                "The MCP token does not match this Wisp process. Restart Wisp or relaunch the client.",
            ),
        );
        return;
    }

    let (reply, replies) = std::sync::mpsc::channel();
    if requests.send_blocking(Incoming { request, reply }).is_err() {
        respond(
            &stream,
            Response::err(id, "unavailable", "Wisp is shutting down."),
        );
        return;
    }

    match replies.recv_timeout(REPLY_TIMEOUT) {
        Ok(response) => respond(&stream, response),
        Err(_) => respond(
            &stream,
            Response::err(id, "timeout", "Wisp did not answer in time."),
        ),
    }
}

fn respond(mut stream: &TcpStream, response: Response) {
    let Ok(mut text) = serde_json::to_string(&response) else {
        return;
    };
    text.push('\n');
    let _ = stream.write_all(text.as_bytes());
    let _ = stream.flush();
}

/// Drives one request through a real listener, with `answer` standing in for
/// the UI thread. Test-only so the endpoint can be exercised without a window.
#[cfg(test)]
fn endpoint_round_trip<F>(token: Option<&str>, answer: F) -> Response
where
    F: Fn(Request) -> Response + Send + 'static,
{
    let (requests, incoming) = async_channel::unbounded::<Incoming>();
    std::thread::spawn(move || {
        while let Ok(job) = incoming.recv_blocking() {
            let _ = job.reply.send(answer(job.request));
        }
    });

    // A test endpoint stays off the real discovery file entirely.
    let endpoint = Endpoint::bind(requests, false).unwrap();
    let stream = TcpStream::connect(("127.0.0.1", endpoint.port())).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();

    let request = Request {
        token: token.map_or_else(|| endpoint.token.clone(), ToString::to_string),
        id: 1,
        method: "ping".into(),
        params: serde_json::Value::Null,
    };
    let mut line = serde_json::to_string(&request).unwrap();
    line.push('\n');
    (&stream).write_all(line.as_bytes()).unwrap();

    let mut reply = String::new();
    BufReader::new(&stream).read_line(&mut reply).unwrap();
    let response = serde_json::from_str(reply.trim()).unwrap();

    drop(endpoint);
    response
}

#[cfg(test)]
mod tests {
    use super::endpoint_round_trip;

    #[test]
    fn the_endpoint_answers_a_ping() {
        // The endpoint is a thin transport; this drives one request through it
        // with a stand-in UI thread that answers everything.
        let response = endpoint_round_trip(None, |request| {
            assert_eq!(request.method, "ping");
            super::Response::ok(request.id, serde_json::json!({"pong": true}))
        });
        assert!(response.ok);
        assert_eq!(response.result.unwrap()["pong"], true);
    }

    #[test]
    fn a_wrong_token_is_rejected_before_the_handler() {
        let response = endpoint_round_trip(Some("not-the-token"), |request| {
            // If the token check is missing, this would be the reply.
            super::Response::ok(request.id, serde_json::json!({"leaked": true}))
        });
        assert!(!response.ok);
        assert_eq!(response.error.unwrap().code, "unauthorized");
    }
}
