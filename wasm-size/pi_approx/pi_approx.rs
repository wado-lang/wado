// Leibniz formula: π/4 = 1 - 1/3 + 1/5 - 1/7 + ...
fn main() {
    let mut pi = 0.0_f64;
    let mut sign = 1.0_f64;
    for i in 0..1_000_000 {
        pi += sign / (2.0 * i as f64 + 1.0);
        sign = -sign;
    }
    pi *= 4.0;
    println!("pi = {:.15}", pi);
}
