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
priv enum PipeReader {
  NoReader
  ReadEndClosed
  HasReader(
    coro~ : @coroutine.Coroutine,
    buf~ : FixedArray[Byte],
    offset~ : Int,
    max_len~ : Int,
    mut result~ : Int
  )
}

///|
priv enum PipeWriter {
  NoWriter
  WriteEndClosed
  HasWriter(
    coro~ : @coroutine.Coroutine,
    buf~ : Bytes,
    offset~ : Int,
    len~ : Int,
    mut result~ : Int
  )
}

///|
priv struct Pipe {
  mut reader : PipeReader
  mut writer : PipeWriter
  read_buf : @io_buffer.Buffer
}

///|
/// The read end of a pipe.
/// Data written to the write end can be read from here.
struct PipeRead(Pipe)

///|
/// The write end of a pipe.
/// Data written here can be read from the read end.
struct PipeWrite(Pipe)

///|
/// Create a new in-memory pipe.
/// Data written to the write end can be obtained from the read end.
/// Useful for testing and converting writer-based function to reader.
pub fn pipe() -> (PipeRead, PipeWrite) {
  let p = {
    reader: NoReader,
    writer: NoWriter,
    read_buf: @io_buffer.Buffer::new(0),
  }
  (PipeRead(p), PipeWrite(p))
}

///|
/// Close the read end of a pipe.
/// After closing the read end, writing to the write end result in error.
pub fn PipeRead::close(self : PipeRead) -> Unit {
  let PipeRead(self) = self
  if self.writer is (HasWriter(_) as writer) {
    writer.result = 0
    writer.coro.wake()
  }
  self.reader = ReadEndClosed
}

///|
/// Close the write end of a pipe.
/// After closing the write end, reading from the read end result in EOF.
pub fn PipeWrite::close(self : PipeWrite) -> Unit {
  let PipeWrite(self) = self
  if self.reader is (HasReader(_) as reader) {
    reader.result = 0
    reader.coro.wake()
  }
  self.writer = WriteEndClosed
}

///|
/// Reader implementation for the read end of a pipe.
/// Read data from a pipe. The data come from the write end of the pipe.
/// Data transfer only happen when reader and writer are present at the same time.
pub impl Reader for PipeRead with _direct_read(self, dst, offset~, max_len~) {
  let PipeRead(self) = self
  guard self.reader is NoReader
  match self.writer {
    NoWriter => {
      let coro = @coroutine.current_coroutine()
      let reader = HasReader(coro~, buf=dst, offset~, max_len~, result=0)
      self.reader = reader
      defer (if self.reader is HasReader(_) { self.reader = NoReader })
      @coroutine.suspend()
      guard reader is HasReader(result~, ..)
      result
    }
    WriteEndClosed => 0
    HasWriter(_) as writer => {
      let len = @cmp.minimum(max_len, writer.len)
      dst.blit_from_bytes(offset, writer.buf, writer.offset, len)
      writer.result = len
      writer.coro.wake()
      @coroutine.pause()
      len
    }
  }
}

///|
/// Provide a read buffer for the pipe reader.
pub impl Reader for PipeRead with _get_internal_buffer(self) {
  let PipeRead(self) = self
  self.read_buf
}

///|
/// This error will be raised when
/// writing to a pipe whose read end is already closed.
pub suberror PipeClosed derive(Show, ToJson)

///|
/// Writer implementation for the write end of a pipe.
/// Write data to a pipe. The data can be read from the read end of the pipe.
/// Data transfer only happen when reader and writer are present at the same time.
pub impl Writer for PipeWrite with write_once(self, buf, offset~, len~) {
  let PipeWrite(self) = self
  guard self.writer is NoWriter
  match self.reader {
    NoReader => {
      let coro = @coroutine.current_coroutine()
      let writer = HasWriter(coro~, buf~, offset~, len~, result=0)
      self.writer = writer
      defer (if self.writer is HasWriter(_) { self.writer = NoWriter })
      @coroutine.suspend()
      guard writer is HasWriter(result~, ..)
      guard result > 0 else { raise PipeClosed }
      result
    }
    ReadEndClosed => raise PipeClosed
    HasReader(_) as reader => {
      let len = @cmp.minimum(len, reader.max_len)
      reader.buf.blit_from_bytes(reader.offset, buf, offset, len)
      reader.result = len
      reader.coro.wake()
      // We must make sure the writer return after the reader is woken,
      // otherwise another immediate write after this one will
      // accidentally write to the same read request.
      //
      // We cannot set `self.reader` to `NoReader` a priori here as well,
      // because in that case, an immediate write + read combo after this write
      // may complete faster than the previous read.
      // The `pause` here is also vital for preventing stackoverflow in a busy loop.
      @coroutine.pause()
      len
    }
  }
}
