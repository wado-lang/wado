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
pub(all) enum Encoding {
  UTF8
}

///|
pub(open) trait Writer {
  async write_once(Self, Bytes, offset~ : Int, len~ : Int) -> Int
  async write(Self, &Data) -> Unit = _
  async write_reader(Self, &Reader) -> Unit = _
}

///|
impl Writer with write(self, data) {
  let view = data.to_bytesview()
  let start = view.start_offset()
  let len = view.length()
  guard len > 0 else { return }
  let end = start + len
  let offset = self.write_once(view.data(), offset=start, len~)
  if offset == end {
    return
  }
  for offset = start + offset; offset < end; {
    let progress = self.write_once(view.data(), offset~, len=end - offset)
    continue offset + progress
  }
}

///|
impl Writer with write_reader(self, reader) {
  let buf = reader._get_internal_buffer()
  if buf.len > 0 {
    let buf_bytes = buf.buf.unsafe_reinterpret_as_bytes()
    self.write(buf_bytes[buf.start:buf.start + buf.len])
    buf.start = 0
    buf.len = 0
  }
  buf.enlarge_to(1)
  let max_len = buf.buf.length()
  while reader._direct_read(buf.buf, offset=0, max_len~) is n && n > 0 {
    let buf_bytes = buf.buf.unsafe_reinterpret_as_bytes()
    self.write(buf_bytes[:n])
  }
}
