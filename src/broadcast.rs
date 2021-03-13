use std::collections::{HashMap, HashSet, VecDeque};
use std::net::{TcpStream, TcpListener, Ipv4Addr};
use std::{time, thread, cmp};
use std::io::{self, Read, Write};

use {amy, httparse};
use url::Url;

use crate::api;
use crate::config::{Config, StreamConfig, Container};
use kaeru::AVCodecID::AV_CODEC_ID_OPUS;
use sha1::{Digest, Sha1};

const CLIENT_BUFFER_LEN: usize = 16384;
// Number of frames to buffer by
const BACK_BUFFER_LEN: usize = 256;
// Seconds of inactivity until client timeout
const CLIENT_TIMEOUT: u64 = 10;

pub struct Broadcaster {
    poll: amy::Poller,
    reg: amy::Registrar,
    data: amy::Receiver<Buffer>,
    /// Map of amy ID -> incoming client
    incoming: HashMap<usize, Incoming>,
    /// Map from amy ID -> client
    clients: HashMap<usize, Client>,
    /// Vec of mount names, idx is mount id
    streams: Vec<Stream>,
    /// vec where idx: mount id , val: set of clients attached to mount id
    client_mounts: Vec<HashSet<usize>>,
    listener: TcpListener,
    listeners: api::Listeners,
    lid: usize,
    tid: usize,
    name: String,
}

#[derive(Clone, Debug)]
pub struct Buffer {
    mount: usize,
    data: BufferData
}

#[derive(Clone, Debug)]
pub enum BufferData {
    Header(Vec<u8>),
    Frame { data: Vec<u8>, pts: f64 },
    Trailer(Vec<u8>),
    InlineData(Vec<u8>),
}

struct Client {
    conn: TcpStream,
    buffer: VecDeque<u8>,
    last_action: time::Instant,
    agent: Agent,
    options: HashMap<String, String>,
    chunked_encoder: Chunker,
    websocket_encoder: WebSocketChunker,
}

#[derive(PartialEq)]
enum Agent {
    MPV,
    MPD,
    NotChunked,
    WebSocket,
    Other,
}

struct Incoming {
    last_action: time::Instant,
    conn: TcpStream,
    buf: [u8; 1024],
    len: usize,
}

struct Stream {
    config: StreamConfig,
    inline: BufferData,
    header: BufferData,
    buffer: VecDeque<BufferData>,
}

enum Chunker {
    Header(String),
    Body(usize),
    Footer(String),
}

enum WebSocketChunker {
    Header(Vec<u8>),
    Body(usize)
}

// Write
enum WR {
    Ok,
    Inc(usize),
    Blocked,
    Err,
}

pub fn start(cfg: &Config, listeners: api::Listeners) -> amy::Sender<Buffer> {
    let (mut b, tx) = Broadcaster::new(cfg, listeners).unwrap();
    thread::spawn(move || b.run());
    tx
}

impl Broadcaster {
    pub fn new(cfg: &Config, listeners: api::Listeners) -> io::Result<(Broadcaster, amy::Sender<Buffer>)> {
        let poll = amy::Poller::new()?;
        let mut reg = poll.get_registrar();
        let listener = TcpListener::bind((Ipv4Addr::new(0, 0, 0, 0), cfg.radio.port))?;
        listener.set_nonblocking(true)?;
        let lid = reg.register(&listener, amy::Event::Read)?;
        let tid = reg.set_interval(5000)?;
        let (tx, rx) = reg.channel()?;
        let mut streams = Vec::new();
        for config in cfg.streams.iter().cloned() {
            streams.push(Stream { config, inline: BufferData::InlineData(b"\x01".to_vec()), header: BufferData::Header(b"".to_vec()), buffer: VecDeque::with_capacity(BACK_BUFFER_LEN) })
        }

        Ok((Broadcaster {
            poll,
            reg,
            data: rx,
            incoming: HashMap::new(),
            clients: HashMap::new(),
            streams,
            client_mounts: vec![HashSet::new(); cfg.streams.len()],
            listener,
            listeners,
            lid,
            tid,
            name: cfg.radio.name.clone(),
        }, tx))
    }

    pub fn run(&mut self) {
        debug!("starting broadcaster");
        loop {
            for n in self.poll.wait(15).unwrap() {
                if n.id == self.lid {
                    self.accept_client();
                } else if n.id == self.tid{
                    self.reap();
                } else if n.id == self.data.get_id() {
                    self.process_buffer();
                } else if self.incoming.contains_key(&n.id) {
                    self.process_incoming(n.id);
                } else if self.clients.contains_key(&n.id) {
                    self.process_client(n.id);
                } else {
                    warn!("Received amy event for bad id: {}", n.id);
                }
            }
        }
    }

    fn reap(&mut self) {
        let mut ids = Vec::new();
        for (id, inc) in self.incoming.iter() {
            if inc.last_action.elapsed() > time::Duration::from_secs(CLIENT_TIMEOUT) {
                ids.push(*id);
            }
        }
        for id in ids.iter() {
            self.remove_incoming(id);
        }
        ids.clear();

        for (id, client) in self.clients.iter() {
            if client.last_action.elapsed() > time::Duration::from_secs(CLIENT_TIMEOUT) {
                ids.push(*id);
            }
        }
        for id in ids.iter() {
            self.remove_client(id);
        }
    }

    fn accept_client(&mut self) {
        loop {
            match self.listener.accept() {
                Ok((conn, ip)) => {
                    debug!("Accepted new connection from {:?}!", ip);
                    let pid = self.reg.register(&conn, amy::Event::Read).unwrap();
                    self.incoming.insert(pid, Incoming::new(conn));
                }
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                    break;
                }
                _ => { unimplemented!(); }
            }
        }

    }

    fn process_buffer(&mut self) {
        while let Ok(buf) = self.data.try_recv() {
            for id in self.client_mounts[buf.mount].clone() {
                if {
                    let client = self.clients.get_mut(&id).unwrap();
                    if buf.data.is_data() || client.agent == Agent::MPD || client.agent == Agent::WebSocket {
                        client.send_data(&buf.data)
                    } else {
                        Ok(())
                    }
                }.is_err() {
                    self.remove_client(&id);
                }
            }
            if buf.data.is_header() {
                self.streams[buf.mount].header = buf.data.clone();
            }
            if buf.data.is_inline() {
                self.streams[buf.mount].inline = buf.data.clone();
            }
            {
                let ref mut sb = self.streams[buf.mount].buffer;
                sb.push_back(buf.data);
                while sb.len() > BACK_BUFFER_LEN {
                    sb.pop_front();
                }
            }
        }
    }

    fn process_incoming(&mut self, id: usize) {
        match self.incoming.get_mut(&id).unwrap().process() {
            Ok(Some((path, agent, headers, options))) => {
                // Need this
                let ub = Url::parse("http://localhost/").unwrap();
                let url = if let Ok(u) = ub.join(&path) {
                    u
                } else {
                    self.remove_incoming(&id);
                    return;
                };
                let mount = url.path();

                let inc = self.incoming.remove(&id).unwrap();
                for (mid, stream) in self.streams.iter().enumerate() {
                    if mount.ends_with(&stream.config.mount) {
                        debug!("Adding a client to stream {}, id {}", stream.config.mount, id);
                        // Swap to write only mode
                        self.reg.reregister(id, &inc.conn, amy::Event::Write).unwrap();
                        let mut client = Client::new(inc.conn, agent, options);
                        // Send header, and buffered data
                        let res = client.write_resp(&self.name, &stream.config)
                            .and_then(|_| {
                                if client.agent == Agent::WebSocket {
                                    client.send_data(&stream.inline)?
                                }
                                Ok(())
                            })
                            .and_then(|_| client.send_data(&stream.header))
                            .and_then(|_| {
                                // TODO: Consider edge case where the header is double sent
                                for buf in stream.buffer.iter() {
                                    client.send_data(buf)?
                                }
                                Ok(())
                            });

                        if res.is_ok() {
                            self.client_mounts[mid].insert(id);
                            self.clients.insert(id, client);
                            self.listeners.lock().unwrap().insert(id, api::Listener {
                                mount: stream.config.mount.clone(),
                                path: path.clone(),
                                headers,
                            });
                        } else {
                            debug!("Failed to write data to client");
                        }
                        return;
                    }
                }
                debug!("Client specified unknown path: {}", mount);
            }
            Ok(None) => { },
            Err(()) => self.remove_incoming(&id),
        }
    }

    fn process_client(&mut self, id: usize) {
        match self.clients.get_mut(&id).unwrap().flush_buffer() {
            Err(()) => self.remove_client(&id),
            _ => { }
        }
    }

    fn remove_client(&mut self, id: &usize) {
        let client = self.clients.remove(id).unwrap();
        self.reg.deregister(&client.conn).unwrap();
        self.listeners.lock().unwrap().remove(id);
        debug!("Removing a client from stream, id {}", id);
        // Remove from client_mounts map too
        for m in self.client_mounts.iter_mut() {
            m.remove(id);
        }
    }

    fn remove_incoming(&mut self, id: &usize) {
        let inc = self.incoming.remove(id).unwrap();
        self.reg.deregister(&inc.conn).unwrap();
    }
}

impl Buffer {
    pub fn new(mount: usize, data: BufferData) -> Buffer {
        Buffer { mount, data }
    }
}

impl BufferData {
    pub fn is_data(&self) -> bool {
        match *self {
            BufferData::Frame { .. } => true,
            _ => false,
        }
    }
    pub fn is_header(&self) -> bool {
        match *self {
            BufferData::Header { .. } => true,
            _ => false,
        }
    }
    pub fn is_inline(&self) -> bool {
        match *self {
            BufferData::InlineData { .. } => true,
            _ => false,
        }
    }

    pub fn frame(&self) -> &[u8] {
        match *self {
            BufferData::Header(ref f)
            | BufferData::Frame { data: ref f, .. }
            | BufferData::Trailer(ref f)
            | BufferData::InlineData(ref f) => f,
        }
    }
}

impl Incoming {
    fn new(conn: TcpStream) -> Incoming {
        conn.set_nonblocking(true).unwrap();
        Incoming {
            last_action: time::Instant::now(),
            conn,
            buf: [0; 1024],
            len: 0,
        }
    }

    fn process(&mut self) -> Result<Option<(String, Agent, Vec<api::Header>, HashMap<String, String>)>, ()> {
        self.last_action = time::Instant::now();
        if self.read().is_ok() {
            let mut headers = [httparse::EMPTY_HEADER; 1024];
            let mut req = httparse::Request::new(&mut headers);
            let mut options = HashMap::new();
            match req.parse(&self.buf[..self.len]) {
                Ok(httparse::Status::Complete(_)) => {
                    if let Some(p) = req.path {
                        let mut agent = Agent::Other;
                        let ah: Vec<api::Header> = req.headers.into_iter()
                            .filter(|h| *h != &httparse::EMPTY_HEADER)
                            .map(|h| api::Header {
                                name: h.name.to_owned(),
                                value: String::from_utf8(h.value.to_vec()).unwrap_or("".to_owned())
                            })
                            .map(|h| {
                                if h.name.to_lowercase() == "user-agent" {
                                    if h.value.starts_with("mpv") {
                                        agent = Agent::MPV;
                                    } else if h.value.starts_with("mpd") || h.value.starts_with("Music Player Daemon") {
                                        agent = Agent::MPD;
                                    }
                                } else if h.name.to_lowercase() == "upgrade" && h.value == "websocket" {
                                    agent = Agent::WebSocket;
                                } else if h.name.to_lowercase() == "sec-websocket-key" {
                                    let mut hasher = Sha1::new();
                                    hasher.update(h.value.as_bytes());
                                    hasher.update("258EAFA5-E914-47DA-95CA-C5AB0DC85B11".as_bytes()); //WebSocket UUID defined on spec
                                    options.insert("websocket:accept".to_string(), base64::encode(hasher.finalize()));
                                }
                                h
                            })
                            .collect();
                        Ok(Some((p.to_owned(), agent, ah, options)))
                    } else {
                        Err(())
                    }
                }
                Ok(httparse::Status::Partial) => Ok(None),
                Err(_) => Err(()),
            }
        } else {
            Err(())
        }
    }

    fn read(&mut self) -> Result<(), ()> {
        loop {
            match self.conn.read(&mut self.buf[self.len..]) {
                Ok(0) => return Err(()),
                Ok(a) => self.len += a,
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(_) => return Err(()),
            }
        }
    }
}



macro_rules! write_agent_match {
        ($self:ident, $buf:ident) => {
            match $self.agent {
                Agent::NotChunked => match $self.conn.write($buf) {
                    Ok(a) => Ok(Some(a)),
                    Err(e) => Err(e)
                },
                Agent::WebSocket => $self.websocket_encoder.write(&mut $self.conn, $buf),
                _ => $self.chunked_encoder.write(&mut $self.conn, $buf)
            }
        };
    }

impl Client {
    fn new(conn: TcpStream, agent: Agent, options: HashMap<String, String>) -> Client {
        Client {
            conn,
            buffer: VecDeque::with_capacity(CLIENT_BUFFER_LEN),
            last_action: time::Instant::now(),
            chunked_encoder: Chunker::new(),
            websocket_encoder: WebSocketChunker::new(),
            agent,
            options
        }
    }

    fn write_resp(&mut self, name: &str, config: &StreamConfig) -> Result<(), ()> {
        let mut lines =
            match self.agent {
                Agent::WebSocket => {
                    vec![
                        format!("HTTP/1.1 101 Switching Protocols"),
                        format!("server: {}/{}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")),
                        format!("Upgrade: websocket"),
                        format!("Connection: Upgrade"),
                            format!("Sec-WebSocket-Accept: {}", match self.options.get("websocket:accept") {
                            Some(s) => s,
                            None => ""
                        }),
                        format!("Sec-WebSocket-Version: 13")
                    ]
                },
                _ => vec![
                    format!("HTTP/1.1 200 OK"),
                    format!("server: {}/{}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")),
                    format!("content-type: {}", if let Container::MP3 = config.container {
                        "audio/mpeg;codecs=mp3"
                    } else if let Container::FLAC = config.container {
                        "audio/flac"
                    } else if let Container::AAC = config.container {
                        "audio/aac"
                    } else if let Container::Ogg = config.container {
                        if config.codec == AV_CODEC_ID_OPUS {
                            "audio/ogg;codecs=opus"
                        } else {
                            "audio/ogg"
                        }
                    } else {
                        "audio/ogg"
                    }),
                    format!("connection: close"),
                    format!("cache-control: no-store, max-age=0, no-transform"),
                    format!("accept-ranges: none"),
                    //Fixes chrome opening and closing streams continuously with Range requests
                    format!("x-content-type-options: nosniff"),
                    format!("x-audiocast-name: {}", name),
                    match config.bitrate {
                        Some(bitrate) => format!("x-audiocast-bitrate: {}", bitrate),
                        None => format!("x-audiocast-bitrate: 0"),
                    },
                    format!("icy-name: {}", name),
                    match config.bitrate {
                        Some(bitrate) => format!("icy-br: {}", bitrate),
                        None => format!("icy-br: 0"),
                    },
                ]
            };

        if self.agent != Agent::NotChunked && self.agent != Agent::WebSocket {
            lines.push(format!("transfer-encoding: chunked"));
        }

        let data = lines.join("\r\n") + "\r\n\r\n";
        match self.conn.write_all(data.as_bytes()) {
            Ok(_) => Ok(()),
            Err(ref e) => {
                debug!("Failed to write_resp: {}", e);
                Err(())
            },
        }
    }

    fn send_data(&mut self, frame: &BufferData) -> Result<(), ()> {
        if frame.frame().len() == 0 {
            return Ok(());
        }
        let vector_data = match self.agent {
            Agent::WebSocket => {
                use byteorder::{BigEndian, WriteBytesExt};
                let mut d = match frame {
                    BufferData::Header(data) => {
                        let mut h = vec![1];
                        h.write_u64::<BigEndian>(data.len() as u64).unwrap();
                        h
                    }
                    BufferData::Frame {data, pts} => {
                        let mut h = vec![2];
                        h.write_u64::<BigEndian>((data.len() + core::mem::size_of::<f64>()) as u64).unwrap();
                        h.write_f64::<BigEndian>(*pts as f64).unwrap();
                        h
                    }
                    BufferData::Trailer(data) => {
                        let mut h = vec![3];
                        h.write_u64::<BigEndian>(data.len() as u64).unwrap();
                        h
                    }

                    BufferData::InlineData(data) => {
                        let mut h = vec![255];
                        h.write_u64::<BigEndian>(data.len() as u64).unwrap();
                        h
                    }
                };
                d.extend(frame.frame());
                d
            }
            _ => frame.frame().to_vec()
        };
        let data = vector_data.as_slice();

        // Attempt to flush buffer first
        match self.flush_buffer() {
            Ok(true) => { },
            Ok(false) => {
                self.buffer.extend(data.iter());
                while self.buffer.len() > CLIENT_BUFFER_LEN {
                    self.buffer.pop_front();
                }
                return Ok(())
            },
            Err(()) => return Err(()),
        }

        match write_agent_match!(self, data) {
            Ok(Some(0)) => {
                debug!("Failed to send_data to client: 0 bytes written");
                Err(())
            },
            // Complete write, do nothing
            Ok(Some(a)) if a == data.len() => Ok(()),
            // Incomplete write, append to buf
            Ok(Some(a)) => {
                self.buffer.extend(data[0..a].iter());
                while self.buffer.len() > CLIENT_BUFFER_LEN {
                    self.buffer.pop_front();
                }
                Ok(())
            }
            Ok(None) => {
                self.buffer.extend(data.iter());
                while self.buffer.len() > CLIENT_BUFFER_LEN {
                    self.buffer.pop_front();
                }
                Ok(())
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                self.buffer.extend(data.iter());
                while self.buffer.len() > CLIENT_BUFFER_LEN {
                    self.buffer.pop_front();
                }
                Ok(())
            }
            Err(ref e) => {
                debug!("Failed to send_data: {}", e);
                Err(())
            },
        }
    }

    fn flush_buffer(&mut self) -> Result<bool, ()> {
        self.last_action = time::Instant::now();

        if self.buffer.is_empty() {
            return Ok(true);
        }
        loop {
            match self.write_buffer() {
                WR::Ok => {
                    self.buffer.clear();
                    return Ok(true);
                }
                WR::Inc(a) => {
                    for _ in 0..a {
                        self.buffer.pop_front();
                    }
                }
                WR::Blocked => return Ok(false),
                WR::Err => return Err(()),
            }
        }
    }

    fn write_buffer(&mut self) -> WR {
        let (head, tail) = self.buffer.as_slices();

        match write_agent_match!(self, head) {
            Ok(Some(0)) => {
                debug!("Failed to write_buffer to client: 0 bytes written");
                WR::Err
            },
            Ok(Some(a)) if a == head.len() => {
                match write_agent_match!(self, tail) {
                    Ok(Some(0)) => {
                        debug!("Failed to write_buffer to client: 0 bytes written");
                        WR::Err
                    },
                    Ok(Some(i)) if i == tail.len() => WR::Ok,
                    Ok(Some(i)) => WR::Inc(i + a),
                    Ok(None) => WR::Inc(a),
                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => WR::Inc(a),
                    Err(ref e) => {
                        debug!("Failed to write_buffer: {}", e);
                        WR::Err
                    },
                }
            },
            Ok(Some(a)) => WR::Inc(a),
            Ok(None) => WR::Inc(0),
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => WR::Blocked,
            Err(ref e) => {
                debug!("Failed to write_buffer: {}", e);
                WR::Err
            }
        }
    }
}


const CHUNK_SIZE: usize = 4096;

impl Chunker {
    fn new() -> Chunker {
        Chunker::Header(format!("{:X}\r\n", CHUNK_SIZE))
    }

    fn write<T: io::Write>(&mut self, conn: &mut T, data: &[u8]) -> io::Result<Option<usize>> {
        match &*self {
            Chunker::Header(s) => {
                let amnt = conn.write(s.as_bytes())?;
                if amnt == s.len() {
                    *self = Chunker::Body(0);
                    self.write(conn, data)
                } else {
                    *self = Chunker::Header(s[amnt..].to_string());
                    Ok(None)
                }
            }
            Chunker::Body(i) => {
                let amnt = conn.write(&data[..cmp::min(CHUNK_SIZE - i, data.len())])?;
                if i + amnt == CHUNK_SIZE {
                    *self = Chunker::Footer("\r\n".to_string());
                    // Continue writing, and add on the current amount
                    // written to the result.
                    // We ignore errors here for now, since they should
                    // be reported later anyways. TODO: think more about it
                    match self.write(conn, &data[amnt..]) {
                        Ok(r) => {
                            Ok(Some(match r {
                                Some(a) => a + amnt,
                                None => amnt
                            }))
                        }
                        Err(ref e) => {
                            debug!("Failed to Chunker::Body to client partial write due to error: {}, written {}", e, amnt);
                            Ok(Some(amnt))
                        }
                    }
                } else {
                    *self = Chunker::Body(i + amnt);
                    Ok(Some(amnt))
                }
            }
            Chunker::Footer(s) => {
                let amnt = conn.write(s.as_bytes())?;
                if amnt == s.len() {
                    *self = Chunker::Header(format!("{:X}\r\n", CHUNK_SIZE));
                    self.write(conn, data)
                } else {
                    *self = Chunker::Footer(s[amnt..].to_string());
                    Ok(None)
                }
            }
        }
    }
}


/*
https://tools.ietf.org/html/rfc6455#page-29
The length of the "Payload data", in bytes: if 0-125, that is the
      payload length.  If 126, the following 2 bytes interpreted as a
      16-bit unsigned integer are the payload length.  If 127, the
      following 8 bytes interpreted as a 64-bit unsigned integer (the
      most significant bit MUST be 0) are the payload length.
 */
const WEBSOCKET_CHUNK_SIZE: usize = 4096; // 125, 4096

impl WebSocketChunker {
    fn new() -> WebSocketChunker {
        WebSocketChunker::Header(match WEBSOCKET_CHUNK_SIZE {
            125 => b"\x82\x7D".to_vec(),
            4096 => b"\x82\x7E\x10\x00".to_vec(),
            _ => { unimplemented!() }
        })
    }

    fn write<T: io::Write>(&mut self, conn: &mut T, data: &[u8]) -> io::Result<Option<usize>> {
        match &*self {
            WebSocketChunker::Header(s) => {
                let amnt = conn.write(s.as_ref())?;
                if amnt == s.len() {
                    *self = WebSocketChunker::Body(0);
                    self.write(conn, data)
                } else {
                    *self = WebSocketChunker::Header(s[amnt..].to_vec());
                    Ok(None)
                }
            }
            WebSocketChunker::Body(i) => {
                let amnt = conn.write(&data[..cmp::min(WEBSOCKET_CHUNK_SIZE - i, data.len())])?;
                if i + amnt == WEBSOCKET_CHUNK_SIZE {
                    *self = WebSocketChunker::new();
                    // Continue writing, and add on the current amount
                    // written to the result.
                    // We ignore errors here for now, since they should
                    // be reported later anyways. TODO: think more about it
                    match self.write(conn, &data[amnt..]) {
                        Ok(r) => {
                            Ok(Some(match r {
                                Some(a) => a + amnt,
                                None => amnt
                            }))
                        }
                        Err(ref e) => {
                            debug!("Failed to WebSocketChunker::Body to client partial write due to error: {}, written {}", e, amnt);
                            Ok(Some(amnt))
                        }
                    }
                } else {
                    *self = WebSocketChunker::Body(i + amnt);
                    Ok(Some(amnt))
                }
            }
        }
    }
}
