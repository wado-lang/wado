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
  write_once_sync(Self, BytesView) -> Int raise
  write_sync(Self, &Data) -> Unit raise = _
  write_reader_sync(Self, &Reader) -> Unit raise = _
}

///|
impl Writer with write_sync(self, data) {
  let mut view = data.to_bytesview()
  while view.length() > 0 {
    let written = self.write_once_sync(view)
    view = view[written:]
  }
}

///|
impl Writer with write_reader_sync(self, reader) {
  let buf = reader._get_internal_buffer()
  if buf.len > 0 {
    let buf_bytes = buf.buf.unsafe_reinterpret_as_bytes()
    self.write_sync(buf_bytes[buf.start:buf.start + buf.len])
    buf.start = 0
    buf.len = 0
  }
  buf.enlarge_to(1)
  let max_len = buf.buf.length()
  while reader._direct_read(buf.buf, offset=0, max_len~) is n && n > 0 {
    let buf_bytes = buf.buf.unsafe_reinterpret_as_bytes()
    self.write_sync(buf_bytes[:n])
  }
}
