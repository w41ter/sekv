// Copyright 2023 The Sekas Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! A mod to extend the ascii standard library.

/// Escapes bytes that are not printable ASCII characters.
#[inline]
pub fn escape_bytes(bytes: &[u8]) -> String {
    String::from_utf8(bytes.iter().flat_map(|&b| std::ascii::escape_default(b)).collect::<Vec<_>>())
        .expect("all bytes are escaped")
}
