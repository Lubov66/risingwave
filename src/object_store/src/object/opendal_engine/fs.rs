// Copyright 2023 RisingWave Labs
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

use opendal::layers::RetryLayer;
use opendal::services::Fs;
use opendal::Operator;

use super::{EngineType, OpendalObjectStore};
use crate::object::ObjectResult;
impl OpendalObjectStore {
    /// create opendal fs engine.
    pub fn new_fs_engine() -> ObjectResult<Self> {
        // Create fs backend builder.
        let mut builder = Fs::default();

        // fs engine is only used in CI, so we can hardcode root.
        builder.root("/tmp/rw_ci");

        let op: Operator = Operator::new(builder)?
            .layer(RetryLayer::default())
            .finish();
        Ok(Self {
            op,
            engine_type: EngineType::Fs,
        })
    }
}
