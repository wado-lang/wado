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
async fn[R : Reader] @io_buffer.Buffer::try_ensure(
  self : Self,
  len : Int,
  reader~ : R,
) -> Bool {
  if self.len >= len {
    return true
  }
  self.enlarge_to(len)
  let capacity = self.buf.length()
  while self.len < len {
    let offset = self.start + self.len
    let n = reader._direct_read(self.buf, offset~, max_len=capacity - offset)
    if n == 0 {
      return false
    }
    self.len += n
  } else {
    true
  }
}

///|
async fn[R : Reader] @io_buffer.Buffer::find_opt(
  self : Self,
  target : Bytes,
  reader~ : R,
) -> Int? {
  let target_len = target.length()
  for searched = 0 {
    guard self.try_ensure(searched + target_len, reader~) else { return None }
    let len = self.len
    let start = self.start + searched
    let end = self.start + len
    let region = self.buf.unsafe_reinterpret_as_bytes()[start:end]
    if region.find(target) is Some(i) {
      return Some(searched + i)
    }
    continue len - target_len + 1
  }
}
