//! Logging subsystem
//!
//! Logs are stored in `/var/log/sv/{unit}/current` for system services and
//! `/var/log/sv/{user}/{unit}/current` for user services.

use chrono::{DateTime, Datelike, Local, NaiveDateTime, TimeZone};
use std::io::{self, ErrorKind};
use std::str;
use std::{borrow::Cow, io::Write};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt};

/// Entry doesn't necessarily corespond to a single line, it corresponds to the amount a single
/// call to `read` returns in case log buffering is disabled or up-to one buffer size in case it's
/// enabled, could be less than a line or could be more.
///
/// It's job of the pretty-printer to figure out lines from entries.
pub struct LogEntry<'a> {
    /// timestamp in UTC timezone
    timestamp: NaiveDateTime,
    /// entry bytes
    entry: Cow<'a, [u8]>,
}

const MAX_ENTRY_SIZE: usize = 4096;
const DATE_FORMAT: &'static str = "%Y-%m-%d %H:%M:%S.%6f";
const DATE_LEN: usize =
      4 // %Y (checked at construction to be non-negative)
    + 1 // "-"
    + 2 // %m
    + 1 // "-"
    + 2 // %d"
    + 1 // " "
    + 2 // %H
    + 1 // ":"
    + 2 // %M
    + 1 // ":"
    + 2 // %S
    + 1 // "."
    + 6 // %6f
;
const SYNCHRONIZE_START: [u8; 4] = [0xFF; 4];
const SYNCHRONIZE_END: [u8; 4] = [0x00; 4];

#[derive(Error, Debug)]
pub enum DeserializeError {
    #[error("not enough bytes on the input")]
    NotEnoughInput,
    #[error("there's more data than the entry said there should be")]
    TooMuchInput,
    #[error("timestamp is not valid UTF-8")]
    Utf8Error(#[from] str::Utf8Error),
    #[error("invalid timestamp")]
    InvalidTimestamp(#[from] chrono::format::ParseError),
    #[error("invalid escape, missing byte after 0x00")]
    InvalidEscape,
    #[error("missing synchronization prefix")]
    MissingSynchronizeStart,
    #[error("missing synchronization suffix")]
    MissingSynchronizeEnd,
}

/// prevents either [`SYNCHRONIZE_END`] or [`SYNCHRONIZE_START`] from occuring in the message
/// payload
fn escape(input: &[u8], output: &mut Vec<u8>) {
    for byte in input {
        match *byte {
            // escape 0x00 and 0xFF bytes
            0x00 => output.extend([0x00, 0xF0]),
            0xFF => output.extend([0x00, 0xFF]),
            byte => output.push(byte),
        }
    }
}

fn unescape(input: &[u8], output: &mut Vec<u8>) -> Result<(), DeserializeError> {
    let mut iter = input.iter();
    while let Some(&byte) = iter.next() {
        if byte == 0x00 {
            let escaped = *iter.next().ok_or(DeserializeError::InvalidEscape)?;
            let byte = match escaped {
                0xF0 => 0x00,
                0xFF => 0xFF,
                _ => return Err(DeserializeError::InvalidEscape),
            };
            output.push(byte);
        } else {
            output.push(byte);
        }
    }
    Ok(())
}

impl<'a> LogEntry<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        let now = Local::now();
        assert!(now.year() > 0, "no negative year");
        assert!(bytes.len() <= MAX_ENTRY_SIZE, "entry too long");
        LogEntry {
            timestamp: now.naive_utc(),
            entry: Cow::Borrowed(bytes),
        }
    }

    pub fn to_owned(&self) -> LogEntry<'static> {
        LogEntry {
            timestamp: self.timestamp,
            entry: Cow::Owned(self.entry.clone().into_owned()),
        }
    }

    pub fn serialize(&self, buffer: &mut Vec<u8>) {
        buffer.extend(SYNCHRONIZE_START);
        buffer
            .write_fmt(format_args!("{}", self.timestamp.format(DATE_FORMAT)))
            .unwrap();
        let entry = self.entry.as_ref();
        let len = entry.len(); // length before escaping
        buffer.extend(u16::to_le_bytes(len.try_into().unwrap()));
        escape(self.entry.as_ref(), &mut *buffer);
        buffer.extend(SYNCHRONIZE_END); // synchronization suffix
    }

    pub fn deserialize(buffer: &[u8]) -> Result<LogEntry<'_>, DeserializeError> {
        let buffer = buffer
            .strip_prefix(&SYNCHRONIZE_START)
            .ok_or(DeserializeError::MissingSynchronizeStart)?
            .strip_suffix(&SYNCHRONIZE_END)
            .ok_or(DeserializeError::MissingSynchronizeEnd)?;

        if buffer.len() < DATE_LEN + 2 {
            return Err(DeserializeError::NotEnoughInput);
        }

        let (timestamp, rest) = buffer.split_at(DATE_LEN);
        let timestamp = str::from_utf8(timestamp)?;
        let timestamp = NaiveDateTime::parse_from_str(timestamp, DATE_FORMAT)?;

        let (len, rest) = rest.split_at(2);
        let len = usize::from(u16::from_le_bytes(len.try_into().unwrap()));

        if rest.len() > len * 2 {
            // pre unescape check if there's too much input
            return Err(DeserializeError::TooMuchInput);
        }

        let entry = if len == rest.len() {
            // if the length matches there was no escaping so we don't need to unescape anything
            Cow::Borrowed(rest)
        } else {
            let mut output = Vec::with_capacity(len);
            unescape(rest, &mut output)?;
            // check the escaped input matches the declared length
            if output.len() > len {
                return Err(DeserializeError::TooMuchInput);
            } else if output.len() < len {
                return Err(DeserializeError::NotEnoughInput);
            }
            Cow::Owned(output)
        };

        Ok(LogEntry { timestamp, entry })
    }

    pub fn local_timestamp(&self) -> DateTime<Local> {
        Local.from_utc_datetime(&self.timestamp)
    }

    pub fn as_slice(&self) -> &[u8] {
        self.entry.as_ref()
    }
}

/// buffer capacity for the [`LogReader`] is based on the maximum amount of space required to
/// deserialize one [`LogEntry`], which is statically known
const BUFFER_CAPACITY: usize = SYNCHRONIZE_START.len()
    + DATE_LEN
    + 2 // u16 for len of entry size
    + MAX_ENTRY_SIZE * 2 // all bytes were escaped and use 2 bytes per byte
    + SYNCHRONIZE_END.len();

pub struct LogReader {
    buffer: Box<[u8; BUFFER_CAPACITY]>,
    /// number of valid data bytes in `buffer`
    bytes: usize,
    /// length of the previous message
    last_len: usize,
    /// reader has reached EOF before a synchronization point
    pub incomplete: bool,
    /// total bytes read from the input reader
    pub read_total: u64,
}

impl LogReader {
    /// discards `amount` bytes from the start of the buffer
    fn shift_buffer(&mut self, amount: usize) {
        if amount == 0 {
            // nothing to shift
            return;
        }
        if amount >= self.bytes {
            // nothing to copy, discard all bytes
            self.bytes = 0;
            return;
        }
        // copy bytes to the beginning
        self.buffer.copy_within(amount..self.bytes, 0);
        self.bytes -= amount;
    }

    pub fn new() -> LogReader {
        LogReader {
            buffer: Box::new([0; BUFFER_CAPACITY]),
            bytes: 0,
            last_len: 0, // no message was read yet
            incomplete: false,
            read_total: 0,
        }
    }
}

#[derive(Error, Debug)]
pub enum ReadEntryError {
    #[error(transparent)]
    DeserializeError(#[from] DeserializeError),
    #[error(transparent)]
    IoError(#[from] io::Error),
}

impl LogReader {
    /// reads more bytes at the end of the buffer
    async fn read_into_buffer<R>(&mut self, reader: &mut R) -> io::Result<()>
    where
        R: AsyncRead + Unpin,
    {
        match reader.read(&mut self.buffer[self.bytes..]).await {
            Ok(0) => {
                self.incomplete = true;
                Err(io::Error::new(
                    ErrorKind::UnexpectedEof,
                    "reader ended before synchronization point was found",
                ))
            },
            Ok(n) => {
                self.read_total += n as u64;
                self.bytes += n;
                Ok(())
            }
            Err(err) => Err(err),
        }
    }

    /// discards bytes from the input until it finds `SYNCHRONIZE_START`
    async fn synchronize_start<R>(&mut self, reader: &mut R) -> io::Result<()>
    where
        R: AsyncRead + Unpin,
    {
        loop {
            let slice = &self.buffer[..self.bytes];
            if let Some(start_offset) = slice
                .array_windows()
                .position(|window| window == &SYNCHRONIZE_START)
            {
                // shift the buffer to the left to drop unwanted bytes before the synchronization
                self.shift_buffer(start_offset);
                // found it
                break Ok(());
            } else {
                // the buffer doesn't contain the whole SYNCHRONIZE_START, check if it ends with anything useful
                let useful_bytes = slice
                    .iter()
                    .rev()
                    .take_while(|&&byte| byte == SYNCHRONIZE_START[0])
                    .count();
                // sanity check: otherwise we would've found the pattern
                assert!(useful_bytes < SYNCHRONIZE_START.len());
                // fill the start manually as it's simpler
                self.buffer[..useful_bytes].fill(SYNCHRONIZE_START[0]);
                self.bytes = useful_bytes;
                // and try reading more bytes
                self.read_into_buffer(reader).await?;
            }
        }
    }

    /// reads into internal buffer until it finds `SYNCHRONIZE_END`, return the length of a slice
    /// containing one serialized [`LogEntry`]
    ///
    /// assumes that the start is already synchronized
    ///
    /// if `SYNCHRONIZE_END` cannot be found in the maximum `LogEntry` size bytes we return
    /// Ok(None), the caller should try calling [`synchronize_start`] again
    async fn synchronize_end<R>(&mut self, reader: &mut R) -> io::Result<Option<usize>>
    where
        R: AsyncRead + Unpin,
    {
        assert!(self.buffer.starts_with(&SYNCHRONIZE_START));

        let mut offset = 0;
        loop {
            let slice = &self.buffer[offset..self.bytes];
            if let Some(end_offset) = slice
                .array_windows()
                .position(|window| window == &SYNCHRONIZE_END)
            {
                break Ok(Some(offset + end_offset + SYNCHRONIZE_END.len()));
            } else {
                // we used the whole buffer and didn't find anything
                if self.buffer.len() == self.bytes {
                    break Ok(None);
                }

                // skip the already scanned portion next time, but rescan the last 3 bytes in case
                // there is a partial SYNCHRONIZE_END at the boundary
                offset += slice.len() - 3;

                self.read_into_buffer(reader).await?;
            }
        }
    }

    /// find and deserialize next entry
    ///
    ///
    pub async fn next_entry<R>(&mut self, reader: &mut R) -> Result<LogEntry<'_>, ReadEntryError>
    where
        R: AsyncRead + Unpin,
    {
        // discard the previous message bytes
        self.shift_buffer(self.last_len);
        loop {
            self.synchronize_start(reader).await?;
            if let Some(len) = self.synchronize_end(reader).await? {
                self.last_len = len;
                return LogEntry::deserialize(&self.buffer[..len]).map_err(<_>::from);
            } else {
                // we couldn't find SYNCHRONIZE_END within the expected distance of
                // SYNCHRONIZE_START, discard the current SYNCHRONIZE_START and try synchronizing
                // again
                self.shift_buffer(SYNCHRONIZE_START.len());
            }
        }
    }
}
