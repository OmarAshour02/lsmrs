use crate::db::Db;
use crate::resp::{Reply, read_command};
use std::io::{self, BufReader, BufWriter, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::thread;

pub struct Server {
    listener: TcpListener,
    db: Arc<RwLock<Db>>,
    shutdown: Arc<AtomicBool>,
}

#[derive(Clone)]
pub struct ShutdownHandle {
    flag: Arc<AtomicBool>,
    addr: SocketAddr,
}

impl ShutdownHandle {
    // `accept` blocks, so setting the flag is not enough to stop the loop.
    // Connecting to our own port wakes it up so it can notice.
    pub fn shutdown(&self) {
        self.flag.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(self.addr);
    }
}

impl Server {
    pub fn bind(addr: &str, db: Db) -> Result<Self, io::Error> {
        Ok(Self {
            listener: TcpListener::bind(addr)?,
            db: Arc::new(RwLock::new(db)),
            shutdown: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn local_addr(&self) -> Result<SocketAddr, io::Error> {
        self.listener.local_addr()
    }

    pub fn shutdown_handle(&self) -> Result<ShutdownHandle, io::Error> {
        Ok(ShutdownHandle {
            flag: Arc::clone(&self.shutdown),
            addr: self.listener.local_addr()?,
        })
    }

    pub fn run(&self) -> Result<(), io::Error> {
        for stream in self.listener.incoming() {
            if self.shutdown.load(Ordering::Relaxed) {
                return Ok(());
            }

            let stream = match stream {
                Ok(stream) => stream,
                Err(e) => {
                    eprintln!("lsmrs: accept failed: {e}");
                    continue;
                }
            };

            let db = Arc::clone(&self.db);
            thread::spawn(move || {
                if let Err(e) = serve_connection(stream, &db) {
                    eprintln!("lsmrs: connection ended: {e}");
                }
            });
        }
        Ok(())
    }
}

fn serve_connection(stream: TcpStream, db: &RwLock<Db>) -> Result<(), io::Error> {
    // `BufReader` takes ownership, so the writing half needs its own handle to
    // the same socket -- `try_clone` is `dup(2)`.
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = BufWriter::new(stream);

    loop {
        let args = match read_command(&mut reader) {
            Ok(Some(args)) => args,
            Ok(None) => return Ok(()),
            // A desynchronised stream cannot be recovered, so say so and hang
            // up rather than misreading the next bytes as a command.
            Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                Reply::Error(format!("ERR Protocol error: {e}")).write_to(&mut writer)?;
                return writer.flush();
            }
            Err(e) => return Err(e),
        };

        match dispatch(db, &args) {
            Outcome::Nothing => {}
            Outcome::Reply(reply) => {
                reply.write_to(&mut writer)?;
                writer.flush()?;
            }
            Outcome::Quit => {
                Reply::Simple("OK").write_to(&mut writer)?;
                return writer.flush();
            }
        }
    }
}

pub enum Outcome {
    Nothing,
    Reply(Reply),
    Quit,
}

pub fn dispatch(db: &RwLock<Db>, args: &[Vec<u8>]) -> Outcome {
    let Some(name) = args.first() else {
        return Outcome::Nothing;
    };
    let name = name.to_ascii_uppercase();

    match (name.as_slice(), args.len()) {
        (b"PING", 1) => Outcome::Reply(Reply::Simple("PONG")),
        (b"PING", 2) => Outcome::Reply(Reply::Bulk(args[1].clone())),
        (b"ECHO", 2) => Outcome::Reply(Reply::Bulk(args[1].clone())),
        (b"QUIT", _) => Outcome::Quit,

        // redis-cli sends COMMAND DOCS on connect and is happy with nothing.
        (b"COMMAND", _) => Outcome::Reply(Reply::Array(Vec::new())),

        (b"GET", 2) => match db.read().unwrap().get(&args[1]) {
            Ok(Some(value)) => Outcome::Reply(Reply::Bulk(value)),
            Ok(None) => Outcome::Reply(Reply::Nil),
            Err(e) => Outcome::Reply(server_error(e)),
        },

        (b"SET", 3) => match db.write().unwrap().put(args[1].clone(), args[2].clone()) {
            Ok(()) => Outcome::Reply(Reply::Simple("OK")),
            Err(e) => Outcome::Reply(server_error(e)),
        },

        (b"DEL", n) if n >= 2 => {
            let mut db = db.write().unwrap();
            let mut removed = 0;
            for key in &args[1..] {
                match db.get(key) {
                    // Redis counts keys that were actually there, so an absent
                    // key must not write a tombstone or inflate the count.
                    Ok(Some(_)) => match db.delete(key) {
                        Ok(()) => removed += 1,
                        Err(e) => return Outcome::Reply(server_error(e)),
                    },
                    Ok(None) => {}
                    Err(e) => return Outcome::Reply(server_error(e)),
                }
            }
            Outcome::Reply(Reply::Integer(removed))
        }

        (b"PING" | b"ECHO" | b"GET" | b"SET" | b"DEL", _) => {
            let name = String::from_utf8_lossy(&name).to_lowercase();
            Outcome::Reply(Reply::Error(format!(
                "ERR wrong number of arguments for '{name}' command"
            )))
        }

        _ => Outcome::Reply(Reply::Error(format!(
            "ERR unknown command '{}'",
            String::from_utf8_lossy(&args[0])
        ))),
    }
}

fn server_error(e: std::io::Error) -> Reply {
    Reply::Error(format!("ERR {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn test_db() -> RwLock<Db> {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("lsmrs_srv_test_{}_{}", std::process::id(), n));
        RwLock::new(
            Db::with_config(Config {
                path,
                sync: false,
                ..Config::default()
            })
            .unwrap(),
        )
    }

    fn args(parts: &[&str]) -> Vec<Vec<u8>> {
        parts.iter().map(|p| p.as_bytes().to_vec()).collect()
    }

    fn encode(db: &RwLock<Db>, parts: &[&str]) -> String {
        match dispatch(db, &args(parts)) {
            Outcome::Reply(reply) => {
                let mut out = Vec::new();
                reply.write_to(&mut out).unwrap();
                String::from_utf8_lossy(&out).into_owned()
            }
            Outcome::Quit => "<quit>".into(),
            Outcome::Nothing => "<nothing>".into(),
        }
    }

    #[test]
    fn ping_and_echo() {
        let db = test_db();
        assert_eq!(encode(&db, &["ping"]), "+PONG\r\n");
        assert_eq!(encode(&db, &["PING", "hi"]), "$2\r\nhi\r\n");
        assert_eq!(encode(&db, &["echo", "hi"]), "$2\r\nhi\r\n");
    }

    #[test]
    fn set_get_del_round_trip() {
        let db = test_db();
        assert_eq!(encode(&db, &["SET", "k", "v"]), "+OK\r\n");
        assert_eq!(encode(&db, &["GET", "k"]), "$1\r\nv\r\n");
        assert_eq!(encode(&db, &["DEL", "k"]), ":1\r\n");
        assert_eq!(encode(&db, &["GET", "k"]), "$-1\r\n");
    }

    #[test]
    fn del_counts_only_keys_that_existed() {
        let db = test_db();
        encode(&db, &["SET", "a", "1"]);
        encode(&db, &["SET", "b", "2"]);
        assert_eq!(encode(&db, &["DEL", "a", "missing", "b"]), ":2\r\n");
    }

    #[test]
    fn command_names_are_case_insensitive() {
        let db = test_db();
        encode(&db, &["sEt", "k", "v"]);
        assert_eq!(encode(&db, &["GeT", "k"]), "$1\r\nv\r\n");
    }

    #[test]
    fn wrong_arity_is_an_error_not_a_panic() {
        let db = test_db();
        assert_eq!(
            encode(&db, &["GET"]),
            "-ERR wrong number of arguments for 'get' command\r\n"
        );
        assert_eq!(
            encode(&db, &["SET", "k"]),
            "-ERR wrong number of arguments for 'set' command\r\n"
        );
    }

    #[test]
    fn unknown_command_names_itself() {
        let db = test_db();
        assert_eq!(
            encode(&db, &["FLUSHALL"]),
            "-ERR unknown command 'FLUSHALL'\r\n"
        );
    }

    #[test]
    fn quit_and_empty_are_not_replies() {
        let db = test_db();
        assert_eq!(encode(&db, &["QUIT"]), "<quit>");
        assert_eq!(encode(&db, &[]), "<nothing>");
    }
}
