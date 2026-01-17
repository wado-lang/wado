fn main {
  try { @stdio.stdout.write_sync("Hello, World!\n") } catch { _ => () }
}
