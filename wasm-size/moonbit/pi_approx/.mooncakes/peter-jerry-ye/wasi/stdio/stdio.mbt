// Copyright 2025 International Digital Economy Academy
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

///|
/// Wrapper for synchronous stdin reads.
struct Input {
  io : @wasi.Fd
  read_buf : @io_buffer.Buffer
}

///|
/// Read bytes from stdin into the provided buffer.
pub impl @sync_io.Reader for Input with _direct_read(
  self,
  buf,
  offset~,
  max_len~,
) {
  let buffer = FixedArray::make(max_len, b'\x00')
  let read = self.io.fd_read([buffer]).0.reinterpret_as_int()
  buffer.blit_to(buf, len=read, dst_offset=offset)
  read
}

///|
/// Provide a read buffer for stdin.
pub impl @sync_io.Reader for Input with _get_internal_buffer(self) {
  self.read_buf
}

///|
/// Read bytes from stdin into the provided buffer.
pub impl @io.Reader for Input with _direct_read(self, buf, offset~, max_len~) {
  let len = @event_loop.suspend_for_read(self.io)
  let len = @cmp.minimum(len, max_len.to_uint64()).to_int()
  let buffer = FixedArray::make(len, b'\x00')
  let read = self.io.fd_read([buffer]).0.reinterpret_as_int()
  buffer.blit_to(buf, len=read, dst_offset=offset)
  read
}

///|
/// Provide a read buffer for stdin.
pub impl @io.Reader for Input with _get_internal_buffer(self) {
  self.read_buf
}

///|
/// Wrapper for synchronous stdout/stderr writes.
struct Output(@wasi.Fd)

///|
/// Write bytes to stdout/stderr.
pub impl @sync_io.Writer for Output with write_once_sync(self, buf) {
  self.0.fd_write([buf]).0.reinterpret_as_int()
}

///|
/// Write bytes to stdout/stderr.
pub impl @io.Writer for Output with write_once(self, buf, offset~, len~) {
  let len = @cmp.minimum(@event_loop.suspend_for_write(self.0), len.to_uint64()).to_int()
  self.0.fd_write([buf[offset:offset + len]]).0.reinterpret_as_int()
}

///|
/// The standard input channel.
/// Using this value will result in standard input being set to non-blocking mode.
/// The standard input will be reset to its original state after the program exits.
pub let stdin : Input = { io: @wasi.Fd(0), read_buf: @io_buffer.Buffer::new(0) }

///|
/// The standard output channel.
/// Using this value will result in standard output being set to non-blocking mode.
/// The standard output will be reset to its original state after the program exits.
pub let stdout : Output = @wasi.Fd(1)

///|
/// The standard error channel.
/// Using this value will result in standard error being set to non-blocking mode.
/// The standard error will be reset to its original state after the program exits.
pub let stderr : Output = @wasi.Fd(2)
