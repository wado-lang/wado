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
pub(open) trait Reader {
  // API for internal implementation only, do not use
  _get_internal_buffer(Self) -> @io_buffer.Buffer

  // API for internal implementation only, do not use
  _direct_read(Self, FixedArray[Byte], offset~ : Int, max_len~ : Int) -> Int raise
  read_sync(Self, FixedArray[Byte], offset? : Int, max_len? : Int) -> Int raise = _
  drop(Self, Int) -> Int raise = _
  read_exactly_sync(Self, len : Int) -> Bytes raise = _
  read_some_sync(Self, max_len? : Int) -> Bytes? raise = _
  read_all_sync(Self) -> &Data raise = _
  read_until_sync(Self, StringView) -> String? raise = _
}

///|
pub(all) suberror ReaderClosed derive(Show, ToJson)

///|
/// `reader.read(dst, offset~, max_len~)` read at most `max_len` bytes from the reader,
/// storing the result in `dst[offset:]`.
/// The number of bytes actually read is returned.
/// Note that the return value may be smaller than `max_len`
/// if not enough data is *immediately* available.
/// If EOF is reached and no more data is available, zero will be returned.
impl Reader with read_sync(
  self,
  dst,
  offset? = 0,
  max_len? = dst.length() - offset,
) {
  let buf = self._get_internal_buffer()
  if buf.len > 0 {
    let n = @cmp.minimum(max_len, buf.len)
    buf.buf.blit_to(dst, src_offset=buf.start, dst_offset=offset, len=n)
    buf.drop(n)
    n
  } else {
    self._direct_read(dst, offset~, max_len~)
  }
}

///|
/// `reader.drop(len)` drops `len` number of bytes from the reader.
/// The number of bytes actually dropped is returned.
/// `drop` will wait until enough number of bytes is available,
/// so the return value would be smaller than `len` only when EOF is reached.
impl Reader with drop(self, len) {
  let buf = self._get_internal_buffer()
  if buf.len >= len {
    buf.drop(len)
    len
  } else {
    let buffered = buf.len
    buf.drop(buffered)
    buf.enlarge_to(1)
    for dropped = buffered; dropped < len; {
      buf.len = self._direct_read(buf.buf, offset=0, max_len=buf.buf.length())
      if buf.len == 0 {
        return dropped
      }
      continue dropped + buf.len
    } else {
      buf.start = buf.len - (dropped - len)
      buf.len = dropped - len
      len
    }
  }
}

///|
/// `reader.read_exactly(len)` read exactly `len` number of bytes from the reader.
/// It will wait until enough number of bytes is available.
/// If EOF is reached before enough data is read, `ReaderClosed` is raised.
impl Reader with read_exactly_sync(self, len) {
  let buf = FixedArray::make(len, b'0')
  for received = 0; received < len; {
    let new_received = self.read_sync(
      buf,
      offset=received,
      max_len=len - received,
    )
    if new_received == 0 {
      raise ReaderClosed
    }
    continue received + new_received
  }
  buf.unsafe_reinterpret_as_bytes()
}

///|
/// `reader.read_some(max_len?)` fetch and return a chunk of data from the reader.
/// It will return as fast as possible when new data become available.
/// If EOF is reached, `None` is returned.
///
/// If `max_len` is specified, the returned chunk will not be larger than `max_len`.
/// Since `read_some` will return immediately when new data become available,
/// the returned chunk may be smaller than `max_len`,
/// even if there are still data available in the reader.
impl Reader with read_some_sync(self, max_len?) {
  let buf = self._get_internal_buffer()
  if buf.len > 0 {
    let buf_bytes = buf.buf.unsafe_reinterpret_as_bytes()
    if max_len is Some(max_len) && max_len < buf.len {
      let data = buf_bytes[buf.start:buf.start + max_len].to_bytes()
      buf.start += max_len
      buf.len -= max_len
      return Some(data)
    } else {
      let data = buf_bytes[buf.start:buf.start + buf.len].to_bytes()
      buf.start = 0
      buf.len = 0
      return Some(data)
    }
  }
  buf.enlarge_to(if max_len is Some(max_len) { max_len } else { 1 })
  let n = self._direct_read(buf.buf, offset=0, max_len=buf.buf.length())
  if n == 0 {
    return None
  }
  if max_len is Some(max_len) && max_len < n {
    buf.start = max_len
    buf.len = n - max_len
    Some(buf.buf.unsafe_reinterpret_as_bytes()[:max_len].to_bytes())
  } else {
    buf.start = 0
    buf.len = 0
    Some(buf.buf.unsafe_reinterpret_as_bytes()[:n].to_bytes())
  }
}

///|
/// Read all remaining content from the reader.
/// The return value is a `&io.Data` object, which can be converted to different formats
/// using `.binary()`, `.text()` or `.json()`.
impl Reader with read_all_sync(self) {
  let buffer_list = []
  let mut buffer = FixedArray::make(1024, b'0')
  let mut offset = 0
  while self.read_sync(buffer, offset~, max_len=buffer.length() - offset) is n &&
        n > 0 {
    offset += n
    if offset == buffer.length() {
      buffer_list.push(buffer)
      buffer = FixedArray::make(1024, b'0')
      offset = 0
    }
  }
  let total_size = buffer_list.length() * 1024 + offset
  let result = FixedArray::make(total_size, b'0')
  for i, buf in buffer_list {
    result.unsafe_blit(i * 1024, buf, 0, 1024)
  }
  result.unsafe_blit(buffer_list.length() * 1024, buffer, 0, offset)
  result.unsafe_reinterpret_as_bytes()
}

///|
/// `reader.read_until(sep)` read UTF-8 encoded text from the input stream
/// until `sep` or EOF is reached.
/// The segment of input from start of reader to start of `sep` or EOF is returned.
/// `sep` will be conusmed from the input, but is not included in the return value.
impl Reader with read_until_sync(self, sep) {
  let sep = @encoding/utf8.encode(sep)
  let buf = self._get_internal_buffer()
  match buf.find_opt(sep, reader=self) {
    None if buf.len > 0 => {
      let buf_bytes = buf.buf.unsafe_reinterpret_as_bytes()
      let remaining = @encoding/utf8.decode(
        buf_bytes[buf.start:buf.start + buf.len],
      )
      buf.drop(buf.len)
      Some(remaining)
    }
    None => None
    Some(index) => {
      let buf_bytes = buf.buf.unsafe_reinterpret_as_bytes()
      let result = @encoding/utf8.decode(buf_bytes[buf.start:buf.start + index])
      buf.drop(index + sep.length())
      Some(result)
    }
  }
}
