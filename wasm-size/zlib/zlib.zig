const std = @import("std");
const flate = std.compress.flate;
const Reader = std.Io.Reader;

pub fn main() void {
    // Read all gzip data from stdin
    var input_buf: [8192]u8 = undefined;
    var input_len: usize = 0;
    while (input_len < input_buf.len) {
        const n = std.posix.read(0, input_buf[input_len..]) catch break;
        if (n == 0) break;
        input_len += n;
    }

    // Decompress gzip and write to stdout
    var decompressed: [8192]u8 = undefined;
    var input_reader = Reader.fixed(input_buf[0..input_len]);
    var window: [flate.max_window_len]u8 = undefined;
    var decompressor = flate.Decompress.init(&input_reader, .gzip, &window);
    const decompressed_len = decompressor.reader.readSliceShort(&decompressed) catch {
        std.debug.print("decompress failed\n", .{});
        return;
    };

    var written: usize = 0;
    while (written < decompressed_len) {
        const n = std.posix.write(1, decompressed[written..decompressed_len]) catch break;
        if (n == 0) break;
        written += n;
    }
}
