#[cfg(not(unix))]
compile_error!("fncdu currently supports Unix-like platforms because UID metadata is required.");

use clap::Parser;
use std::ffi::OsStr;
use std::fs::{self, Metadata};
use std::io::{self, BufWriter, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Parser, Debug)]
#[command(
    name = "fncdu",
    version,
    about = "Stream ncdu JSON for files owned by a target UID.",
    long_about = "Recursively scans a directory without following symlinks, filters non-directory entries by Unix UID, and writes ncdu export JSON to standard output."
)]
struct Args {
    /// Print summary statistics to standard error when done.
    #[arg(short = 's', long)]
    stats: bool,

    /// Print each entry that cannot be read to standard error.
    #[arg(short = 'v', long)]
    verbose: bool,

    /// Directory to scan.
    directory_path: PathBuf,

    /// Target Unix user ID.
    target_uid: u32,
}

#[derive(Default)]
struct Stats {
    matched_files: u64,
    matched_apparent_size: u64,
    errors: u64,
}

struct Outcome {
    stats: Stats,
    json_bytes: u64,
    /// The reader closed stdout before the export was complete.
    truncated: bool,
}

/// Counts the bytes stdout accepted, for the summary.
struct CountingWriter<W> {
    inner: W,
    bytes: u64,
}

impl<W: Write> Write for CountingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let written = self.inner.write(buf)?;
        self.bytes += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

struct Scanner<W: Write> {
    out: BufWriter<CountingWriter<W>>,
    uid: u32,
    report_errors: bool,
    stats: Stats,
}

fn main() {
    let args = Args::parse();
    let stats = args.stats;
    let started = Instant::now();

    match run(args, io::stdout().lock()) {
        Ok(outcome) => {
            if stats {
                print_summary(&outcome, started.elapsed());
            }
            if outcome.truncated {
                std::process::exit(1);
            }
        }
        Err(error) => {
            let _ = writeln!(io::stderr(), "fncdu: {error}");
            std::process::exit(1);
        }
    }
}

fn run<W: Write>(args: Args, output: W) -> io::Result<Outcome> {
    // Like ncdu: follow a symlinked root and name it by its absolute, resolved path.
    let root = fs::canonicalize(&args.directory_path)?;
    let metadata = fs::metadata(&root)?;
    if !metadata.is_dir() {
        let message = format!("{} is not a directory", args.directory_path.display());
        return Err(io::Error::new(io::ErrorKind::InvalidInput, message));
    }

    let mut scan = Scanner {
        out: BufWriter::with_capacity(
            1 << 20,
            CountingWriter {
                inner: output,
                bytes: 0,
            },
        ),
        uid: args.target_uid,
        report_errors: args.verbose,
        stats: Stats::default(),
    };
    let result = (|| {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        writeln!(
            scan.out,
            "[1,2,{{\"progname\":\"fncdu\",\"progver\":\"{}\",\"timestamp\":{timestamp}}},",
            env!("CARGO_PKG_VERSION")
        )?;
        scan.dir(&root, root.as_os_str(), &metadata)?;
        scan.out.write_all(b"]")?;
        scan.out.flush()
    })();

    let truncated = match result {
        Ok(()) => false,
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => true,
        Err(error) => return Err(error),
    };
    Ok(Outcome {
        json_bytes: scan.out.get_ref().bytes,
        stats: scan.stats,
        truncated,
    })
}

impl<W: Write> Scanner<W> {
    /// Writes a directory as an ncdu array: its info object, then its children.
    /// Directories are written whoever owns them, but carry no size, so ncdu totals
    /// only count the target UID's files.
    fn dir(&mut self, path: &Path, name: &OsStr, metadata: &Metadata) -> io::Result<()> {
        let entries = fs::read_dir(path);
        self.out.write_all(b"[{\"name\":")?;
        write_name(&mut self.out, name)?;
        write!(self.out, ",\"dev\":{}", metadata.dev())?;
        let entries = match entries {
            Ok(entries) => entries,
            Err(error) => {
                self.error("cannot read directory", path, &error);
                return self.out.write_all(b",\"read_error\":true}]");
            }
        };
        self.out.write_all(b"}")?;

        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    self.error("cannot read directory entry", path, &error);
                    continue;
                }
            };
            // DirEntry::metadata does not follow symlinks.
            let metadata = match entry.metadata() {
                Ok(metadata) => metadata,
                Err(error) => {
                    self.error("cannot read metadata", &entry.path(), &error);
                    continue;
                }
            };
            if metadata.is_dir() {
                self.out.write_all(b",\n")?;
                self.dir(&entry.path(), &entry.file_name(), &metadata)?;
            } else if metadata.uid() == self.uid {
                self.leaf(&entry.file_name(), &metadata)?;
            }
        }
        self.out.write_all(b"]")
    }

    /// Writes any non-directory: regular file, symlink, FIFO, socket or device.
    fn leaf(&mut self, name: &OsStr, metadata: &Metadata) -> io::Result<()> {
        self.stats.matched_files += 1;
        self.stats.matched_apparent_size += metadata.len();

        self.out.write_all(b",\n{\"name\":")?;
        write_name(&mut self.out, name)?;
        let dsize = metadata.blocks().saturating_mul(512);
        write!(self.out, ",\"asize\":{},\"dsize\":{dsize}", metadata.len())?;
        // ncdu only needs the inode to count a hard-linked file once.
        if metadata.nlink() > 1 {
            write!(self.out, ",\"ino\":{},\"hlnkc\":true", metadata.ino())?;
        }
        if !metadata.is_file() {
            self.out.write_all(b",\"notreg\":true")?;
        }
        self.out.write_all(b"}")
    }

    fn error(&mut self, action: &str, path: &Path, error: &io::Error) {
        self.stats.errors += 1;
        if self.report_errors {
            let _ = writeln!(io::stderr(), "fncdu: {action}: {}: {error}", path.display());
        }
    }
}

/// Writes a JSON string the way ncdu's own export does: the raw filesystem bytes
/// (not necessarily UTF-8), with quotes, backslashes and control bytes escaped.
fn write_name(out: &mut impl Write, name: &OsStr) -> io::Result<()> {
    let bytes = name.as_bytes();
    let mut start = 0;
    out.write_all(b"\"")?;
    for (i, &byte) in bytes.iter().enumerate() {
        if byte < 0x20 || byte == b'"' || byte == b'\\' {
            out.write_all(&bytes[start..i])?;
            write!(out, "\\u{byte:04x}")?;
            start = i + 1;
        }
    }
    out.write_all(&bytes[start..])?;
    out.write_all(b"\"")
}

fn print_summary(outcome: &Outcome, elapsed: Duration) {
    let stats = &outcome.stats;
    let truncated = if outcome.truncated {
        " (truncated: broken pipe)"
    } else {
        ""
    };
    let mut stderr = io::stderr().lock();
    let _ = writeln!(stderr, "\nfncdu summary");
    let _ = writeln!(stderr, "  matched files: {}", stats.matched_files);
    let _ = writeln!(
        stderr,
        "  matched apparent size: {}",
        format_size(stats.matched_apparent_size)
    );
    let _ = writeln!(
        stderr,
        "  JSON bytes written: {}{truncated}",
        outcome.json_bytes
    );
    let _ = writeln!(stderr, "  errors: {}", stats.errors);
    let _ = writeln!(stderr, "  elapsed time: {:.3}s", elapsed.as_secs_f64());
}

fn format_size(bytes: u64) -> String {
    const UNITS: [(&str, u64); 3] = [("GiB", 1 << 30), ("MiB", 1 << 20), ("KiB", 1 << 10)];
    match UNITS.iter().find(|&&(_, unit)| bytes >= unit) {
        Some((name, unit)) => format!("{:.2} {name} ({bytes} bytes)", bytes as f64 / *unit as f64),
        None => format!("{bytes} bytes"),
    }
}

#[cfg(test)]
mod tests {
    use super::{Args, format_size, run};
    use std::fs;
    use std::io::{self, Write};
    use std::os::unix::fs::MetadataExt;
    use tempfile::TempDir;

    #[test]
    fn formats_size_units() {
        assert_eq!(format_size(0), "0 bytes");
        assert_eq!(format_size(1024), "1.00 KiB (1024 bytes)");
        assert_eq!(format_size(1024 * 1024), "1.00 MiB (1048576 bytes)");
        assert_eq!(
            format_size(1024 * 1024 * 1024),
            "1.00 GiB (1073741824 bytes)"
        );
    }

    #[test]
    fn broken_pipe_returns_partial_outcome() {
        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join("owned.txt"), b"hello").unwrap();
        let uid = fs::symlink_metadata(temp.path().join("owned.txt"))
            .unwrap()
            .uid();
        let args = Args {
            stats: false,
            verbose: false,
            directory_path: temp.path().to_path_buf(),
            target_uid: uid,
        };

        let outcome = run(args, BrokenAfter { remaining: 32 }).unwrap();
        assert!(outcome.truncated);
        assert_eq!(outcome.json_bytes, 32);
    }

    struct BrokenAfter {
        remaining: usize,
    }

    impl Write for BrokenAfter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.remaining == 0 {
                return Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"));
            }

            let written = self.remaining.min(buf.len());
            self.remaining -= written;
            Ok(written)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
}
