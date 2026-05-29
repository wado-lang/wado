// Float-to-string benchmark
// Converts 5M random f64 values to decimal strings using {d:.6} format.
// Uses a linear congruential generator for deterministic float sequence.
//
// How to run:
//   mise run benchmark-fts

const std = @import("std");

pub fn main() !void {
    const n: u32 = 5_000_000;
    var state: u32 = 42;
    var total_bytes: u64 = 0;
    var byte_sum: u64 = 0;
    var fmt_buf: [32]u8 = undefined;

    const t0 = std.time.nanoTimestamp();

    for (0..n) |_| {
        state = (state *% 1103515245 +% 12345) & 0x7FFFFFFF;
        const x: f64 = @as(f64, @floatFromInt(state)) / 2147483648.0;
        const slice = std.fmt.bufPrint(&fmt_buf, "{d:.6}", .{x}) catch unreachable;
        total_bytes += slice.len;
        for (slice) |b| {
            byte_sum += b;
        }
    }

    const t1 = std.time.nanoTimestamp();
    const elapsed_ms = @divFloor(t1 - t0, 1_000_000);

    const file = std.fs.File.stdout();
    var out_buf: [256]u8 = undefined;
    var len: usize = 0;

    len = (std.fmt.bufPrint(&out_buf, "fts: {d} f64 conversions (%.6f)\n", .{n}) catch unreachable).len;
    try file.writeAll(out_buf[0..len]);

    len = (std.fmt.bufPrint(&out_buf, "Total bytes: {d}, byte sum: {d}\n", .{ total_bytes, byte_sum }) catch unreachable).len;
    try file.writeAll(out_buf[0..len]);

    len = (std.fmt.bufPrint(&out_buf, "Elapsed: {d} ms\n", .{elapsed_ms}) catch unreachable).len;
    try file.writeAll(out_buf[0..len]);
}
