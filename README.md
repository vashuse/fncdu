# fncdu

`fncdu` streams an [`ncdu`](https://dev.yorhel.nl/ncdu) JSON export containing
only the files owned by a target Unix UID.

It is designed for very large trees and network-attached storage: traversal is a
single pass, output is written directly to `stdout`, and the program does not
build an in-memory file tree.

## Install

```sh
cargo install --path .
```

## Usage

```text
fncdu [FLAGS] <directory_path> <target_uid>
```

Examples:

```sh
fncdu /home 1000 | ncdu -f-
fncdu --stats /srv/archive 1001 > uid-1001.ncdu.json
fncdu /mnt/storage 1000 | gzip > uid-1000.ncdu.json.gz
```

Flags:

```text
-s, --stats      Print summary statistics when done
-v, --verbose    Print each entry that cannot be read
-h, --help       Show help
```

By default `fncdu` prints nothing to `stderr` except a fatal error, so it does
not disturb `ncdu`'s screen when piped into it. Unreadable directories are
still shown in `ncdu`, marked with `!`. When combining `--stats` or `--verbose`
with `ncdu`, redirect `stderr` (for example `2>fncdu.log`).

## Output

The output is a standard ncdu export (header `[1,2,...]`) that `ncdu -f` reads
directly. Directories are included as structural nodes so the result remains
navigable in `ncdu`, but carry no size of their own, so totals only count the
target UID's data. Every non-directory entry (regular file, symlink, FIFO,
socket, device) is included when its UID matches `<target_uid>`.

- Hard-linked files are marked (`ino` + `hlnkc`), so `ncdu` counts them once.
- Directories that cannot be read are marked with `read_error`.
- Names are written as raw filesystem bytes, like ncdu's own export, so a name
  that is not valid UTF-8 makes the stream invalid strict JSON.

Symbolic links are never followed, except a symlinked `<directory_path>`, which
is resolved like `ncdu` does. A symlink to a directory is emitted as a leaf
entry when it matches the requested UID.

## Statistics

With `--stats`, `fncdu` prints a summary to `stderr` containing:

- matched file count
- matched apparent size in bytes plus KiB, MiB, or GiB when appropriate
- exact JSON bytes written to `stdout`
- error count
- elapsed time

The summary counts a hard-linked file once per link; `ncdu` itself counts it once.

If `stdout` is closed before the export is complete, the summary marks the
output as truncated and `fncdu` exits with status 1.

## Platform Support

`fncdu` currently targets Unix-like systems. It uses Unix UID metadata and does
not compile on platforms without `std::os::unix`.