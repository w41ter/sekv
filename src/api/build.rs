// Copyright 2023-present The Sekas Authors.
// Copyright 2022 The Engula Authors.
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

use std::error::Error;
use std::result::Result;

fn main() -> Result<(), Box<dyn Error>> {
    std::env::set_var("PROTOC", protoc_build::PROTOC);
    std::env::set_var("PROTOC_INCLUDE", protoc_build::PROTOC_INCLUDE);

    tonic_build::configure().compile(
        &[
            "sekas/server/v1/catalog.proto",
            "sekas/server/v1/error.proto",
            "sekas/server/v1/metadata.proto",
            "sekas/server/v1/node.proto",
            "sekas/server/v1/root.proto",
            "sekas/server/v1/txn_persistent.proto",
            "sekas/server/v1/types.proto",
            "sekas/server/v1/write.proto",
        ],
        &["."],
    )?;
    Ok(())
}
