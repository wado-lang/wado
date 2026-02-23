fn count_words(text : String) -> Int {
  let mut count = 0
  let mut in_word = false
  for ch in text {
    if ch.is_whitespace() {
      in_word = false
    } else if !in_word {
      in_word = true
      count += 1
    }
  }
  count
}

fn count_lines(text : String) -> Int {
  let mut lines = 0
  for ch in text {
    if ch == '\n' {
      lines += 1
    }
  }
  lines
}

///|
pub async fn run() -> Unit {
  let args = (try? @wasi.args_get()).unwrap()
  if args.length() < 2 {
    @io.Writer::write(@stdio.stderr, "usage: async-wc <path>\n")
    return
  }
  let path = args[1]
  let data = @fs.read_file(path)
  let text = data.text()
  let lines = count_lines(text)
  let words = count_words(text)
  let bytes = data.binary().length()
  @io.Writer::write(@stdio.stdout, "lines: ")
  @io.Writer::write(@stdio.stdout, lines.to_string())
  @io.Writer::write(@stdio.stdout, "\n")
  @io.Writer::write(@stdio.stdout, "words: ")
  @io.Writer::write(@stdio.stdout, words.to_string())
  @io.Writer::write(@stdio.stdout, "\n")
  @io.Writer::write(@stdio.stdout, "bytes: ")
  @io.Writer::write(@stdio.stdout, bytes.to_string())
  @io.Writer::write(@stdio.stdout, "\n")
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
