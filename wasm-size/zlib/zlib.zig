const std = @import("std");
const flate = std.compress.flate;

pub fn main() void {
    var threaded: std.Io.Threaded = .init_single_threaded;
    const io = threaded.io();

    // Read all gzip data from stdin
    var stdin_buf: [8192]u8 = undefined;
    var stdin_reader = std.Io.File.stdin().reader(io, &stdin_buf);

    // Decompress gzip and write to stdout
    var decompressed: [8192]u8 = undefined;
    var window: [flate.max_window_len]u8 = undefined;
    var decompressor = flate.Decompress.init(&stdin_reader.interface, .gzip, &window);
    const decompressed_len = decompressor.reader.readSliceShort(&decompressed) catch {
        std.debug.print("decompress failed\n", .{});
        return;
    };

    var stdout_buf: [8192]u8 = undefined;
    var stdout_writer = std.Io.File.stdout().writer(io, &stdout_buf);
    stdout_writer.interface.writeAll(decompressed[0..decompressed_len]) catch return;
    stdout_writer.interface.flush() catch return;
}
