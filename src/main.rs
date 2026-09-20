use anyhow::Result;
use lsmrs::Db;
use lsmrs::cli::{Command, execute, parse};
use lsmrs::config::Config;
use lsmrs::server::Server;
use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;

const DEFAULT_ADDR: &str = "127.0.0.1:6379";

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("serve") {
        let addr = args
            .get(2)
            .filter(|a| !a.starts_with("--"))
            .map(String::as_str)
            .unwrap_or(DEFAULT_ADDR);
        return serve(addr, !args.iter().any(|a| a == "--no-sync"));
    }

    repl()
}

fn serve(addr: &str, sync: bool) -> Result<()> {
    let config = Config {
        sync,
        ..Config::default()
    };
    let server = Server::bind(addr, Db::with_config(config)?)?;
    if !sync {
        println!("lsmrs: WAL fsync disabled -- writes are not crash-durable");
    }
    println!("lsmrs listening on {}", server.local_addr()?);
    server.run()?;
    Ok(())
}

fn repl() -> Result<()> {
    let mut db = Db::open()?;
    let mut rl = DefaultEditor::new()?;

    loop {
        match rl.readline("lsmrs> ") {
            Ok(line) => {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                rl.add_history_entry(line)?;

                match parse(line) {
                    Ok(Command::Exit) => break,
                    Ok(cmd) => match execute(&mut db, cmd) {
                        Ok(Some(out)) => println!("{out}"),
                        Ok(None) => {}
                        Err(e) => eprintln!("error: {e}"),
                    },
                    Err(e) => eprintln!("error: {e}"),
                }
            }
            Err(ReadlineError::Interrupted | ReadlineError::Eof) => break,
            Err(e) => {
                eprintln!("readline error: {e}");
                break;
            }
        }
    }

    Ok(())
}
