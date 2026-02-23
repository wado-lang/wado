# WASI API Notes

## Unified stdio

- `@wasi/stdio.stdin` implements `@sync_io.Reader` and `@io.Reader`.
- `@wasi/stdio.stdout` and `@wasi/stdio.stderr` implement `@sync_io.Writer` and `@io.Writer`.

## Sync I/O

- `@sync_io.Reader::read_until_sync(reader, "\n") -> String?`
- `@sync_io.Reader::read_all_sync(reader) -> &@sync_io.Data`
- `@sync_io.Writer::write_sync(writer, &@sync_io.Data)`

## Async I/O

- `@io.Reader::read_until(reader, "\n") -> String?`
- `@io.Reader::read_all(reader) -> &@io.Data`
- `@io.Writer::write(writer, &@io.Data)`

## Files

- `@fs.read_file(path)` returns `&@io.Data` (async)
- `@fs.open_sync(path, mode=...)` returns a `File` (sync)

## WASI args/env

- `@wasi.args_get() -> Array[String]`
- `@wasi.environ_get() -> Array[(String, String)]`
