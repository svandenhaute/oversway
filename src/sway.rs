//! Minimal sway IPC: one thread holds the subscription socket and forwards events
//! (plus a fresh tree after workspace/window events) to the main loop.

use serde_json::Value;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Instant;

const MAGIC: &[u8; 6] = b"i3-ipc";
pub const RUN_COMMAND: u32 = 0;
pub const GET_WORKSPACES: u32 = 1;
pub const SUBSCRIBE: u32 = 2;
pub const GET_TREE: u32 = 4;
const EVENT_WORKSPACE: u32 = 0x8000_0000;
const EVENT_WINDOW: u32 = 0x8000_0003;
const EVENT_BINDING: u32 = 0x8000_0005;
const EVENT_TICK: u32 = 0x8000_0007;

pub enum Message {
    /// A workspace became focused: thumbnails need a fresh capture.
    WorkspaceFocus,
    WindowChange,
    Binding { command: String, received: Instant },
    Snapshot(Snapshot),
}

#[derive(Clone, Debug, Default)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

#[derive(Clone, Debug)]
pub struct Window {
    pub rect: Rect,
    pub app_id: String,
    pub focused: bool,
}

#[derive(Clone, Debug)]
pub struct Workspace {
    pub name: String,
    pub number: i64,
    pub output: String,
    pub focused: bool,
    pub visible: bool,
    pub rect: Rect,
    pub windows: Vec<Window>,
}

#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub workspaces: Vec<Workspace>,
}

impl Snapshot {
    pub fn focused_output(&self) -> Option<&str> {
        self.workspaces.iter().find(|workspace| workspace.focused).map(|workspace| workspace.output.as_str())
    }
    pub fn visible_workspace(&self, output: &str) -> Option<&Workspace> {
        self.workspaces.iter().find(|workspace| workspace.visible && workspace.output == output)
    }
}

pub fn socket_path() -> String {
    std::env::var("SWAYSOCK").expect("SWAYSOCK is not set")
}

pub fn connect() -> std::io::Result<UnixStream> {
    UnixStream::connect(socket_path())
}

pub fn send(stream: &mut UnixStream, kind: u32, payload: &str) -> std::io::Result<()> {
    let mut message = Vec::with_capacity(14 + payload.len());
    message.extend_from_slice(MAGIC);
    message.extend_from_slice(&(payload.len() as u32).to_ne_bytes());
    message.extend_from_slice(&kind.to_ne_bytes());
    message.extend_from_slice(payload.as_bytes());
    stream.write_all(&message)
}

pub fn receive(stream: &mut UnixStream) -> std::io::Result<(u32, Vec<u8>)> {
    let mut header = [0u8; 14];
    stream.read_exact(&mut header)?;
    let length = u32::from_ne_bytes(header[6..10].try_into().unwrap()) as usize;
    let kind = u32::from_ne_bytes(header[10..14].try_into().unwrap());
    let mut body = vec![0u8; length];
    stream.read_exact(&mut body)?;
    Ok((kind, body))
}

pub fn request(stream: &mut UnixStream, kind: u32, payload: &str) -> std::io::Result<Value> {
    send(stream, kind, payload)?;
    let (_, body) = receive(stream)?;
    Ok(serde_json::from_slice(&body).unwrap_or(Value::Null))
}

pub fn run_command(command: &str) {
    match connect().and_then(|mut stream| request(&mut stream, RUN_COMMAND, command)) {
        Ok(reply) => {
            if reply.get(0).and_then(|result| result.get("success")).and_then(Value::as_bool) != Some(true) {
                log::warn!("sway command {command:?} failed: {reply}");
            }
        }
        Err(error) => log::warn!("sway command {command:?}: {error}"),
    }
}

/// Runs commands in order on one thread, so the caller never waits for sway.
pub fn command_sender() -> std::sync::mpsc::Sender<String> {
    let (sender, receiver) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        for command in receiver {
            run_command(&command);
        }
    });
    sender
}

fn rect(value: &Value) -> Rect {
    let number = |key: &str| value.get(key).and_then(Value::as_f64).unwrap_or(0.0) as f32;
    Rect { x: number("x"), y: number("y"), width: number("width"), height: number("height") }
}

fn collect_windows(node: &Value, windows: &mut Vec<Window>) {
    let children: Vec<&Value> = ["nodes", "floating_nodes"]
        .iter()
        .filter_map(|key| node.get(*key).and_then(Value::as_array))
        .flatten()
        .collect();
    let is_view = node.get("pid").is_some() && children.is_empty();
    if is_view {
        let app_id = node
            .get("app_id")
            .and_then(Value::as_str)
            .or_else(|| node.pointer("/window_properties/class").and_then(Value::as_str))
            .or_else(|| node.get("name").and_then(Value::as_str))
            .unwrap_or("")
            .to_string();
        windows.push(Window {
            rect: rect(&node["rect"]),
            app_id,
            focused: node.get("focused").and_then(Value::as_bool).unwrap_or(false),
        });
    }
    for child in children {
        collect_windows(child, windows);
    }
}

pub fn snapshot(stream: &mut UnixStream) -> std::io::Result<Snapshot> {
    let tree = request(stream, GET_TREE, "")?;
    let workspaces_reply = request(stream, GET_WORKSPACES, "")?;
    let mut workspaces = Vec::new();
    for output in tree["nodes"].as_array().into_iter().flatten() {
        let output_name = output["name"].as_str().unwrap_or("");
        if output_name == "__i3" {
            continue;
        }
        for workspace in output["nodes"].as_array().into_iter().flatten() {
            let name = workspace["name"].as_str().unwrap_or("").to_string();
            let status = workspaces_reply
                .as_array()
                .into_iter()
                .flatten()
                .find(|entry| entry["name"].as_str() == Some(name.as_str()));
            let flag = |key: &str| status.and_then(|entry| entry[key].as_bool()).unwrap_or(false);
            let mut windows = Vec::new();
            collect_windows(workspace, &mut windows);
            workspaces.push(Workspace {
                number: workspace["num"].as_i64().unwrap_or(-1),
                output: output_name.to_string(),
                focused: flag("focused"),
                visible: flag("visible"),
                rect: rect(&output["rect"]),
                windows,
                name,
            });
        }
    }
    workspaces.sort_by_key(|workspace| (workspace.number < 0, workspace.number, workspace.name.clone()));
    Ok(Snapshot { workspaces })
}

/// Blocking loop: run on its own thread.
/// Blocking loop: run on its own thread. Tree snapshots are fetched on a second thread
/// so a slow get_tree never delays the next binding event.
pub fn listen(sender: impl Fn(Message) -> bool + Clone + Send + 'static) {
    let (refresh, refresh_requests) = std::sync::mpsc::channel::<()>();
    let snapshot_sender = sender.clone();
    std::thread::spawn(move || {
        let mut queries: Option<UnixStream> = None;
        while refresh_requests.recv().is_ok() {
            while refresh_requests.try_recv().is_ok() {}
            if queries.is_none() {
                queries = connect().ok();
            }
            match queries.as_mut().map(snapshot) {
                Some(Ok(snapshot)) => {
                    if !snapshot_sender(Message::Snapshot(snapshot)) {
                        return;
                    }
                }
                _ => queries = None,
            }
        }
    });
    loop {
        let result = (|| -> std::io::Result<()> {
            let mut events = connect()?;
            request(&mut events, SUBSCRIBE, r#"["workspace","window","binding","tick"]"#)?;
            let _ = refresh.send(());
            loop {
                let (kind, body) = receive(&mut events)?;
                let received = Instant::now();
                let event: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                let change = event["change"].as_str().unwrap_or("");
                let needs_snapshot = match kind {
                    EVENT_WORKSPACE => {
                        if change == "focus" {
                            sender(Message::WorkspaceFocus);
                        }
                        true
                    }
                    EVENT_WINDOW => {
                        sender(Message::WindowChange);
                        change != "title" && change != "mark" && change != "urgent"
                    }
                    EVENT_BINDING => {
                        let command = event.pointer("/binding/command").and_then(Value::as_str).unwrap_or("");
                        sender(Message::Binding { command: command.to_string(), received });
                        false
                    }
                    // `swaymsg -t send_tick overlay_toggle` acts like the Alt tap (for scripts and tests).
                    EVENT_TICK => {
                        if event["payload"].as_str() == Some("overlay_toggle") {
                            sender(Message::Binding { command: "nop overlay_toggle".into(), received });
                        }
                        false
                    }
                    _ => false,
                };
                if needs_snapshot && refresh.send(()).is_err() {
                    return Ok(());
                }
            }
        })();
        match result {
            Ok(()) => return,
            Err(error) => {
                log::warn!("sway IPC: {error}; reconnecting");
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
        }
    }
}
