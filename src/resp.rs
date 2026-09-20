use std::io::{self, BufRead, Write};

const MAX_BULK_LEN: i64 = 64 * 1024 * 1024;
const MAX_ARGS: i64 = 1024;

pub enum Reply {
    Simple(&'static str),
    Error(String),
    Integer(i64),
    Bulk(Vec<u8>),
    Nil,
    Array(Vec<Reply>),
}

impl Reply {
    pub fn write_to<W: Write>(&self, writer: &mut W) -> Result<(), io::Error> {
        match self {
            Reply::Simple(text) => write!(writer, "+{text}\r\n"),
            Reply::Error(text) => write!(writer, "-{text}\r\n"),
            Reply::Integer(n) => write!(writer, ":{n}\r\n"),
            Reply::Nil => writer.write_all(b"$-1\r\n"),
            Reply::Bulk(bytes) => {
                write!(writer, "${}\r\n", bytes.len())?;
                writer.write_all(bytes)?;
                writer.write_all(b"\r\n")
            }
            Reply::Array(items) => {
                write!(writer, "*{}\r\n", items.len())?;
                for item in items {
                    item.write_to(writer)?;
                }
                Ok(())
            }
        }
    }
}

pub fn read_command<R: BufRead>(reader: &mut R) -> Result<Option<Vec<Vec<u8>>>, io::Error> {
    let Some(line) = read_line(reader)? else {
        return Ok(None);
    };

    // Anything not starting with `*` is an inline command, which is what a raw
    // telnet or netcat session sends.
    if line.first() != Some(&b'*') {
        return Ok(Some(
            line.split(|b| b.is_ascii_whitespace())
                .filter(|part| !part.is_empty())
                .map(|part| part.to_vec())
                .collect(),
        ));
    }

    let count = parse_len(&line[1..], MAX_ARGS)?;
    let mut args = Vec::with_capacity(count.max(0) as usize);
    for _ in 0..count {
        let Some(header) = read_line(reader)? else {
            return Err(unexpected_eof());
        };
        if header.first() != Some(&b'$') {
            return Err(invalid("expected a bulk string argument"));
        }

        let len = parse_len(&header[1..], MAX_BULK_LEN)?;
        if len < 0 {
            return Err(invalid("negative bulk string length"));
        }

        let mut arg = vec![0u8; len as usize];
        reader.read_exact(&mut arg)?;
        let mut terminator = [0u8; 2];
        reader.read_exact(&mut terminator)?;
        args.push(arg);
    }

    Ok(Some(args))
}

fn read_line<R: BufRead>(reader: &mut R) -> Result<Option<Vec<u8>>, io::Error> {
    let mut line = Vec::new();
    if reader.read_until(b'\n', &mut line)? == 0 {
        return Ok(None);
    }
    while matches!(line.last(), Some(b'\n' | b'\r')) {
        line.pop();
    }
    Ok(Some(line))
}

fn parse_len(bytes: &[u8], max: i64) -> Result<i64, io::Error> {
    let text = std::str::from_utf8(bytes).map_err(|_| invalid("length is not valid UTF-8"))?;
    let len: i64 = text
        .parse()
        .map_err(|_| invalid("length is not an integer"))?;
    if len > max {
        return Err(invalid("length exceeds the protocol limit"));
    }
    Ok(len)
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_string())
}

fn unexpected_eof() -> io::Error {
    io::Error::new(
        io::ErrorKind::UnexpectedEof,
        "connection closed mid-command",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(input: &[u8]) -> Option<Vec<Vec<u8>>> {
        read_command(&mut io::Cursor::new(input)).unwrap()
    }

    fn encode(reply: Reply) -> Vec<u8> {
        let mut out = Vec::new();
        reply.write_to(&mut out).unwrap();
        out
    }

    #[test]
    fn parses_an_array_of_bulk_strings() {
        let args = parse(b"*3\r\n$3\r\nSET\r\n$3\r\nkey\r\n$5\r\nvalue\r\n").unwrap();
        assert_eq!(
            args,
            vec![b"SET".to_vec(), b"key".to_vec(), b"value".to_vec()]
        );
    }

    #[test]
    fn parses_an_inline_command() {
        assert_eq!(
            parse(b"GET  key\r\n").unwrap(),
            vec![b"GET".to_vec(), b"key".to_vec()]
        );
    }

    #[test]
    fn bulk_strings_are_binary_safe() {
        // The length prefix is what allows a value to contain the very
        // delimiter that ends every other part of the protocol.
        let args = parse(b"*2\r\n$3\r\nSET\r\n$4\r\na\r\nb\r\n").unwrap();
        assert_eq!(args[1], b"a\r\nb".to_vec());
    }

    #[test]
    fn closed_connection_reads_as_none() {
        assert!(parse(b"").is_none());
    }

    #[test]
    fn truncated_command_is_an_error() {
        let err = read_command(&mut io::Cursor::new(b"*2\r\n$3\r\nGET\r\n")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn oversized_bulk_length_is_rejected_before_allocating() {
        let err = read_command(&mut io::Cursor::new(b"*1\r\n$999999999\r\n")).unwrap_err();
        assert!(err.to_string().contains("protocol limit"));
    }

    #[test]
    fn encodes_every_reply_shape() {
        assert_eq!(encode(Reply::Simple("OK")), b"+OK\r\n");
        assert_eq!(encode(Reply::Error("ERR nope".into())), b"-ERR nope\r\n");
        assert_eq!(encode(Reply::Integer(-1)), b":-1\r\n");
        assert_eq!(encode(Reply::Nil), b"$-1\r\n");
        assert_eq!(encode(Reply::Bulk(b"hi".to_vec())), b"$2\r\nhi\r\n");
        assert_eq!(
            encode(Reply::Array(vec![Reply::Integer(1), Reply::Nil])),
            b"*2\r\n:1\r\n$-1\r\n"
        );
    }
}
