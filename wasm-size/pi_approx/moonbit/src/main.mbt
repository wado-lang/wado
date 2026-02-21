// Leibniz formula: π/4 = 1 - 1/3 + 1/5 - 1/7 + ...
fn main {
  let mut pi = 0.0
  let mut sign = 1.0
  for i = 0; i < 1000000; i = i + 1 {
    pi = pi + sign / (2.0 * i.to_double() + 1.0)
    sign = -sign
  }
  pi = pi * 4.0
  try { @stdio.stdout.write_sync("pi = \{pi}\n") } catch { _ => () }
}
