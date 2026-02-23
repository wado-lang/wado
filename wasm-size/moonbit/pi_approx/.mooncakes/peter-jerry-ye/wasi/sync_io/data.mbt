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
/// `Data` is an abstraction over various kinds of data format
/// that can be read from/write to a reader/writer.
/// Currently, available formats of data include:
///
/// - `Bytes` and `BytesView` for raw binary data
/// - `String` and `StringView`, for string (UTF8 encoded)
/// - `Json` for JSON string (UTF8 encoded)
trait Data {
  to_bytesview(Self) -> BytesView = _
  to_bytes(Self) -> Bytes
}

///|
impl Data with to_bytesview(self) {
  self.to_bytes()[:]
}

///|
/// `Data` implementation for binary bytes.
pub impl Data for Bytes with to_bytes(self) {
  self
}

///|
/// `Data` implementation for byte views.
pub impl Data for BytesView with to_bytes(self) {
  self.to_bytes()
}

///|
/// `Data` implementation for byte views without copying.
pub impl Data for BytesView with to_bytesview(self) {
  self
}

///|
/// `Data` implementation for UTF-8 strings.
pub impl Data for String with to_bytes(self) {
  @encoding/utf8.encode(self)
}

///|
/// `Data` implementation for UTF-8 string views.
pub impl Data for StringView with to_bytes(self) {
  @encoding/utf8.encode(self)
}

///|
/// `Data` implementation for JSON values.
pub impl Data for Json with to_bytes(self) {
  @encoding/utf8.encode(self.stringify())
}

// ==============================================================================
// internal node: the `Data` trait serves two purpose.
// When the user is sending data, we use `to_binary` to convert user data to binary.
// When the user is receiving data, raw data is always represented as raw binary,
// so `&Data` always internally hold a `Bytes`,
// and the user may use these helpers below to convert them to different format.
//
// Technically we should use two types for these two purposes,
// but that would mean two different names, instead of the simple `Data`.
// So currently we take this rather hacky approach for simplicify of API.
// ==============================================================================

///|
/// Extract the raw binary data from `&Data`
pub fn &Data::binary(self : &Data) -> Bytes {
  self.to_bytes()
}

///|
/// Decode data as a UTF8 encoded string.
///
/// Calling this method will cause string decoding,
/// so user should avoid calling `.text()` more than once.
pub fn &Data::text(self : &Data) -> String raise {
  self.to_bytes() |> @encoding/utf8.decode
}

///|
/// Decode data as a UTF8 encoded JSON string.
///
/// Calling this method will cause string decoding and json parsing,
/// so user should avoid calling `.json()` more than once.
pub fn &Data::json(self : &Data) -> Json raise {
  self.to_bytes() |> @encoding/utf8.decode |> @json.parse
}
