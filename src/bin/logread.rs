use std::io::{ErrorKind, SeekFrom};

use anyhow::{ensure, Context, Result};
use camino::Utf8Path as Path;
use clap::Parser;
use inotify::{EventMask, Inotify, WatchMask};
use std::fmt::{self, Display};
use svmgr::log::{LogEntry, LogReader};
use tokio::fs::File;
use tokio::io::AsyncSeekExt;
use tokio::sync::mpsc;
use tokio::task;
use tokio_stream::StreamExt;

#[derive(Parser)]
struct Args {
    /// Tail the logs instead of printing all entries
    #[clap(short, long)]
    follow: bool,

    /// Which logs to read
    ///
    /// User logs are specified as `{user}/{tag}`, system logs just `{tag}`
    logs: Vec<String>,
}

#[derive(Clone, Copy)]
struct Tag {
    user: Option<&'static str>,
    sv: &'static str,
}

impl Display for Tag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (&self.user, self.sv) {
            (Some(user), sv) => f.write_fmt(format_args!("{user}/{sv}")),
            (None, sv) => f.write_fmt(format_args!("{sv}")),
        }
    }
}

impl Tag {
    fn new(log: &str) -> Option<Tag> {
        if log.chars().filter(|&ch| ch == '/').count() > 1 {
            return None;
        }
        if log.chars().any(|ch| ch != '/' && !ch.is_ascii_graphic()) {
            return None;
        }

        Some(match log.split_once('/') {
            Some((user, sv)) => Tag {
                user: Some(Box::leak(Box::from(user))),
                sv: Box::leak(Box::from(sv)),
            },
            None => Tag {
                user: None,
                sv: Box::leak(Box::from(log)),
            },
        })
    }
}

struct TaggedLogEntry {
    tag: Tag,
    entry: LogEntry<'static>,
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args = Args::parse();

    if args.logs.is_empty() {
        return;
    }

    if !args.follow {
        eprintln!("warning: only follow is supported for now");
    }

    let (tx, mut rx) = mpsc::channel(1);

    let base_path = Path::new("/var/log/sv");
    for log in &args.logs {
        let tx = tx.clone();
        if let Some(tag) = Tag::new(&log) {
            let path = base_path.join(log);
            if !path.exists() {
                eprintln!("[{path}] does not exist");
                continue;
            }
            task::spawn(async move { tail_log(tag, &path, tx).await });
        } else {
            eprintln!("invalid service tag: `{log}`");
        }
    }

    while let Some(log_entry) = rx.recv().await {
        let tag = log_entry.tag;
        let timestamp = log_entry
            .entry
            .local_timestamp()
            .format("%Y-%m-%d %H:%M:%S.%3f");
        let entry = String::from_utf8_lossy(log_entry.entry.as_slice());
        for line in entry.lines() {
            println!("{timestamp} {tag} {line}");
        }
    }
}

async fn tail_log(tag: Tag, path: &Path, tx: mpsc::Sender<TaggedLogEntry>) {
    for _ in 0..3 {
        // TODO better retry limit strategy
        if let Err(err) = try_tail_log(tag, &path, tx.clone()).await {
            eprintln!("[{path}] {err:?}");
        }
    }
}

/// tries to register an inotify watch first for the current log file and hand over to `tail_file`,
/// if it's not found it tries watching the parent directory and hands over to `wait_for_file`
async fn try_tail_log(tag: Tag, path: &Path, tx: mpsc::Sender<TaggedLogEntry>) -> Result<()> {
    let mut inotify = Inotify::init().context("inotify init")?;
    let current_path = path.join("current");
    match inotify.add_watch(&current_path, WatchMask::MODIFY | WatchMask::MOVED_TO) {
        Ok(_) => tail_file(tag, &current_path, tx, inotify).await,
        Err(err) if err.kind() == ErrorKind::NotFound => {
            match inotify.add_watch(path, WatchMask::CREATE) {
                Ok(_) => wait_for_file(tag, &current_path, tx, inotify).await,
                Err(err) if err.kind() == ErrorKind::NotFound => {
                    todo!()
                }
                Err(err) => Err(err).context("watching log directory"),
            }
        }
        Err(err) => Err(err).context("watching current log file"),
    }
}

/// tail a log file. reads `LogEntry`s when the file is modified or overwritten with a new file
async fn tail_file(
    tag: Tag,
    path: &Path,
    tx: mpsc::Sender<TaggedLogEntry>,
    mut inotify: Inotify,
) -> Result<()> {
    let buffer_size = inotify::get_absolute_path_buffer_size(path.as_ref());
    let buffer = vec![0u8; buffer_size].into_boxed_slice();
    let mut event_stream = inotify
        .event_stream(buffer)
        .context("create inotify event stream")?;

    let mut file = File::open(path).await.context("opening log file")?;
    // keep the position in the file where we finished reading, when the length increases we'll read
    // the difference. when the file gets moved to we'll reset it to 0.
    // because we're following from the end we seek to the end at the beginning.
    let mut position = file.seek(SeekFrom::End(0)).await.context("seek log file")?;
    let mut log_reader = LogReader::new();

    while let Some(event) = event_stream.next().await {
        let event = event.context("reading inotify event")?;
        match event.mask {
            EventMask::MODIFY => {
                let metadata = file.metadata().await.context("read log file metadata")?;
                ensure!(metadata.len() >= position, "log file was truncated");
                let read = read_entries(tag, &mut log_reader, &mut file, &tx)
                    .await
                    .context("log entries")?;
                position += read;
            }
            EventMask::MOVED_TO => {
                // reopen the new file
                position = 0;
                file = File::open(path).await.context("opening new log file")?;
            }
            e => unreachable!("did not register this kind of event: {e:?}"),
        }
    }
    Ok(())
}

async fn read_entries(
    tag: Tag,
    log_reader: &mut LogReader,
    file: &mut File,
    tx: &mpsc::Sender<TaggedLogEntry>,
) -> Result<u64> {
    log_reader.read_total = 0;
    log_reader.incomplete = false;

    loop {
        match log_reader.next_entry(file).await {
            Ok(entry) => {
                let tagged = TaggedLogEntry {
                    tag,
                    entry: entry.to_owned(),
                };
                if tx.send(tagged).await.is_err() {
                    break;
                }
            }
            Err(err) => {
                if log_reader.incomplete {
                    break;
                } else {
                    return Err(err).context("read log entry");
                }
            }
        }
    }

    Ok(log_reader.read_total)
}

/// watches a directory until the current log file is created, then hands over to `tail_file`
async fn wait_for_file(
    _tag: Tag,
    _path: &Path,
    _tx: mpsc::Sender<TaggedLogEntry>,
    _inotify: Inotify,
) -> Result<()> {
    todo!()
}
