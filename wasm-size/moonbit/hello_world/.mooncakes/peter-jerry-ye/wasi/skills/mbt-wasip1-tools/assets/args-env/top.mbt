///|
pub async fn run() -> Unit {
  let args = (try? @wasi.args_get()).unwrap()
  let envs = (try? @wasi.environ_get()).unwrap()

  @io.Writer::write(@stdio.stdout, "args:\n")
  for arg in args {
    @io.Writer::write(@stdio.stdout, "- ")
    @io.Writer::write(@stdio.stdout, arg)
    @io.Writer::write(@stdio.stdout, "\n")
  }

  @io.Writer::write(@stdio.stdout, "env:\n")
  for env in envs {
    let (key, value) = env
    @io.Writer::write(@stdio.stdout, "- ")
    @io.Writer::write(@stdio.stdout, key)
    @io.Writer::write(@stdio.stdout, "=")
    @io.Writer::write(@stdio.stdout, value)
    @io.Writer::write(@stdio.stdout, "\n")
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
