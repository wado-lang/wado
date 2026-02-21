const std = @import("std");

// Leibniz formula: π/4 = 1 - 1/3 + 1/5 - 1/7 + ...
pub fn main() void {
    var pi: f64 = 0.0;
    var sign: f64 = 1.0;
    var i: i32 = 0;
    while (i < 1000000) : (i += 1) {
        pi += sign / (2.0 * @as(f64, @floatFromInt(i)) + 1.0);
        sign = -sign;
    }
    pi *= 4.0;
    std.debug.print("pi = {d:.15}\n", .{pi});
}
