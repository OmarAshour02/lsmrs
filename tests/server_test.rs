use lsmrs::config::Config;
use lsmrs::db::Db;
use lsmrs::server::Server;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_config() -> Config {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    Config {
        path: std::env::temp_dir().join(format!("lsmrs_net_{}_{}", std::process::id(), n)),
        sync: false,
        ..Config::default()
    }
}

struct Harness {
    addr: String,
    shutdown: lsmrs::server::ShutdownHandle,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Harness {
    fn start() -> Self {
        let db = Db::with_config(temp_config()).unwrap();
        let server = Arc::new(Server::bind("127.0.0.1:0", db).unwrap());
        let addr = server.local_addr().unwrap().to_string();
        let shutdown = server.shutdown_handle().unwrap();

        let thread = std::thread::spawn(move || {
            server.run().unwrap();
        });

        Self {
            addr,
            shutdown,
            thread: Some(thread),
        }
    }

    fn client(&self) -> Client {
        let stream = TcpStream::connect(&self.addr).unwrap();
        Client {
            writer: stream.try_clone().unwrap(),
            reader: BufReader::new(stream),
        }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.shutdown.shutdown();
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

struct Client {
    writer: TcpStream,
    reader: BufReader<TcpStream>,
}

impl Client {
    fn send(&mut self, parts: &[&str]) {
        let mut out = format!("*{}\r\n", parts.len());
        for part in parts {
            out += &format!("${}\r\n{part}\r\n", part.len());
        }
        self.writer.write_all(out.as_bytes()).unwrap();
        self.writer.flush().unwrap();
    }

    fn reply(&mut self) -> String {
        let mut line = String::new();
        self.reader.read_line(&mut line).unwrap();
        if !line.starts_with('$') {
            return line;
        }
        let len: i64 = line[1..].trim().parse().unwrap();
        if len < 0 {
            return line;
        }
        let mut body = vec![0u8; len as usize + 2];
        self.reader.read_exact(&mut body).unwrap();
        line + &String::from_utf8_lossy(&body)
    }

    fn call(&mut self, parts: &[&str]) -> String {
        self.send(parts);
        self.reply()
    }
}

#[test]
fn set_and_get_over_the_wire() {
    let server = Harness::start();
    let mut client = server.client();

    assert_eq!(client.call(&["PING"]), "+PONG\r\n");
    assert_eq!(client.call(&["SET", "colour", "blue"]), "+OK\r\n");
    assert_eq!(client.call(&["GET", "colour"]), "$4\r\nblue\r\n");
    assert_eq!(client.call(&["DEL", "colour"]), ":1\r\n");
    assert_eq!(client.call(&["GET", "colour"]), "$-1\r\n");
}

#[test]
fn many_clients_share_one_store() {
    let server = Harness::start();

    let writers: Vec<_> = (0..8)
        .map(|i| {
            let mut client = server.client();
            std::thread::spawn(move || {
                for j in 0..25 {
                    let key = format!("c{i}-{j}");
                    assert_eq!(client.call(&["SET", &key, &key]), "+OK\r\n");
                }
            })
        })
        .collect();

    for writer in writers {
        writer.join().unwrap();
    }

    let mut reader = server.client();
    for i in 0..8 {
        for j in 0..25 {
            let key = format!("c{i}-{j}");
            assert_eq!(
                reader.call(&["GET", &key]),
                format!("${}\r\n{key}\r\n", key.len()),
                "lost {key}"
            );
        }
    }
}

#[test]
fn quit_closes_the_connection() {
    let server = Harness::start();
    let mut client = server.client();

    assert_eq!(client.call(&["QUIT"]), "+OK\r\n");

    let mut rest = String::new();
    client.reader.read_to_string(&mut rest).unwrap();
    assert!(rest.is_empty(), "server kept the connection open");
}

#[test]
fn protocol_error_reports_and_hangs_up() {
    let server = Harness::start();
    let mut client = server.client();

    client.writer.write_all(b"*1\r\n$99999999999\r\n").unwrap();
    assert!(client.reply().starts_with("-ERR Protocol error"));
}

#[test]
fn a_broken_connection_does_not_take_the_server_down() {
    let server = Harness::start();

    let mut rude = server.client();
    rude.send(&["SET", "k", "v"]);
    drop(rude);

    let mut client = server.client();
    assert_eq!(client.call(&["PING"]), "+PONG\r\n");
}
