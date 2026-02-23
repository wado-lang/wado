///|
pub async fn run() -> Unit {
  while @io.Reader::read_until(@stdio.stdin, "\n") is Some(line) {
    @io.Writer::write(@stdio.stdout, line)
  }
}

///|
fn main {
  @async.with_event_loop(fn(_) { run() }) catch {
    e => {
      try! @stdio.stderr.write_sync("\{e}\n")
      @wasi.proc_exit(1)
    }
  }
}
