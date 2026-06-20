use anyhow::Result;
use lsmrs::Db;
use lsmrs::cli::{Command, execute, parse};
use lsmrs::config::Config;
use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;

fn main() -> Result<()> {
    let config = Config::default();
    let mut db = Db::open(config.path.as_str(), config.sync)?;
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
