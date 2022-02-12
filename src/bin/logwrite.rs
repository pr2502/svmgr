//! Reads `stdin` and logs it into a file in the `svmgr` log format.
//!
//! For system mode logs are written into `/var/log/sv/{tag}/current`, for user mode logs are
//! written into `/var/log/sv/{user}/{tag}`.

use anyhow::{Context, Result};
use camino::Utf8Path as Path;
use clap::Parser;
use std::io::{Read, Write};
use std::{fs, io};
use svmgr::log::LogEntry;

#[derive(Parser, Debug)]
struct Args {
    /// If present `log` starts in user mode for the given user
    #[clap(long)]
    user: Option<String>,

    /// Log tag, usually the service name
    tag: String,
}

/// maximum payload size for one log entry
const LOGENTRY_LIMIT: usize = 4096;

fn main() -> Result<()> {
    let args = Args::parse();

    let base_path = Path::new("/var/log/sv");
    let log_dir_path = match &args.user {
        Some(user) => base_path.join(user).join(&args.tag),
        None => base_path.join(&args.tag),
    };

    fs::create_dir_all(&log_dir_path)
        .with_context(|| format!("create log directory: `{log_dir_path}`"))?;

    let log_file_path = log_dir_path.join("current");
    let mut log_file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_file_path)
        .with_context(|| format!("open log file for appending: `{log_file_path}`"))?;

    let stdin = io::stdin();
    let mut stdin = stdin.lock();

    let mut in_buffer = Box::new([0u8; LOGENTRY_LIMIT]);
    let mut out_buffer = Vec::new();

    loop {
        match stdin.read(&mut *in_buffer) {
            Ok(0) => break Ok(()), // EOF
            Err(err) => break Err(err).context("read stdin"),
            Ok(n) => {
                let log_entry = LogEntry::new(&in_buffer[..n]);
                out_buffer.clear();
                log_entry.serialize(&mut out_buffer);
                log_file.write_all(&out_buffer).context("write log entry")?;
                log_file.flush().context("flush log file")?;
            }
        }
    }
}
