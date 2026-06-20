use crate::db::Db;
use crate::error::{DbError, Result};

pub enum Command {
    Get(Vec<u8>),
    Put(Vec<u8>, Vec<u8>),
    Del(Vec<u8>),
    Scan,
    Exit,
}

pub fn parse(line: &str) -> Result<Command> {
    let mut parts = line.split_whitespace();

    let cmd_str = parts.next().ok_or(DbError::KeyNotFound)?;

    match cmd_str {
        "GET" => {
            let key = parts
                .next()
                .ok_or(DbError::InvalidCommand("GET requires a key".to_string()))?;
            Ok(Command::Get(key.as_bytes().to_vec()))
        }
        "PUT" => {
            let key = parts
                .next()
                .ok_or(DbError::InvalidCommand("PUT requires a key".to_string()))?;
            let val = parts
                .next()
                .ok_or(DbError::InvalidCommand("PUT requires a value".to_string()))?;
            Ok(Command::Put(
                key.as_bytes().to_vec(),
                val.as_bytes().to_vec(),
            ))
        }
        "DEL" => {
            let key = parts
                .next()
                .ok_or(DbError::InvalidCommand("PUT requires a key".to_string()))?;
            Ok(Command::Del(key.as_bytes().to_vec()))
        }
        "SCAN" => Ok(Command::Scan),
        "EXIT" => Ok(Command::Exit),
        _ => Err(DbError::InvalidCommand(cmd_str.to_string())),
    }
}
pub fn execute(db: &mut Db, cmd: Command) -> Result<Option<String>> {
    match cmd {
        Command::Get(key) => match db.get(&key) {
            Some(v) => Ok(Some(String::from_utf8_lossy(&v).to_string())),
            None => Ok(Some("(not found)".to_string())),
        },
        Command::Put(key, val) => {
            let _x = db.put(key, val);
            Ok(Some("OK".to_string()))
        }
        Command::Del(key) => match db.delete(&key) {
            Ok(_) => Ok(Some("OK".to_string())),
            Err(_) => Ok(Some("(not found)".to_string())),
        },
        Command::Scan => {
            let output: Vec<String> = db
                .scan()
                .map(|(k, v)| {
                    format!(
                        "{} {}",
                        String::from_utf8_lossy(k),
                        String::from_utf8_lossy(v)
                    )
                })
                .collect();
            if output.is_empty() {
                Ok(Some("(empty)".to_string()))
            } else {
                Ok(Some(output.join("\n")))
            }
        }
        Command::Exit => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_get() {
        match parse("GET foo").unwrap() {
            Command::Get(k) => assert_eq!(k, b"foo"),
            _ => panic!("expected Get"),
        }
    }

    #[test]
    fn parse_put() {
        match parse("PUT foo bar").unwrap() {
            Command::Put(k, v) => {
                assert_eq!(k, b"foo");
                assert_eq!(v, b"bar");
            }
            _ => panic!("expected Put"),
        }
    }

    #[test]
    fn parse_unknown_command_errors() {
        assert!(matches!(
            parse("FROBNICATE x"),
            Err(DbError::InvalidCommand(_))
        ));
    }
}
