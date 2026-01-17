// Leibniz formula: π/4 = 1 - 1/3 + 1/5 - 1/7 + ...
let pi: f64 = 0.0;
let sign: f64 = 1.0;
for (let i: i32 = 0; i < 1000000; i++) {
    pi += sign / (2.0 * <f64>i + 1.0);
    sign = -sign;
}
pi *= 4.0;
console.log("pi = " + pi.toString());
