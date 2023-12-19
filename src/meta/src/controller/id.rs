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

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use risingwave_meta_model_v2::prelude::{Actor, Fragment};
use risingwave_meta_model_v2::{actor, fragment};
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QuerySelect};

use crate::manager::{IdCategory, IdCategoryType};
use crate::MetaResult;

pub struct IdGenerator<const TYPE: IdCategoryType>(AtomicU64);

impl<const TYPE: IdCategoryType> IdGenerator<TYPE> {
    pub async fn new(conn: &DatabaseConnection) -> MetaResult<Self> {
        let id: i32 = match TYPE {
            IdCategory::Table => {
                // Since we are using object pk to generate id for tables, here we just implement a dummy
                // id generator and refill it later when inserting the table.
                0
            }
            IdCategory::Fragment => Fragment::find()
                .select_only()
                .column_as(fragment::Column::FragmentId.max().add(1), "available_id")
                .into_tuple()
                .one(conn)
                .await?
                .unwrap(),
            IdCategory::Actor => Actor::find()
                .select_only()
                .column_as(actor::Column::ActorId.max().add(1), "available_id")
                .into_tuple()
                .one(conn)
                .await?
                .unwrap(),
            _ => unreachable!("IdGeneratorV2 only supports Table, Fragment, and Actor"),
        };

        Ok(Self(AtomicU64::new(id as u64)))
    }

    pub fn generate_interval(&self, interval: u64) -> u64 {
        self.0.fetch_add(interval, Ordering::Relaxed)
    }
}

pub type IdGeneratorManagerRef = Arc<IdGeneratorManager>;

/// `IdGeneratorManager` is a manager for three id generators: `tables`, `fragments`, and `actors`. Note that this is just a
/// workaround for the current implementation of `IdGenerator`. We should refactor it later.
pub struct IdGeneratorManager {
    pub tables: Arc<IdGenerator<{ IdCategory::Table }>>,
    pub fragments: Arc<IdGenerator<{ IdCategory::Fragment }>>,
    pub actors: Arc<IdGenerator<{ IdCategory::Actor }>>,
}

impl IdGeneratorManager {
    pub async fn new(conn: &DatabaseConnection) -> MetaResult<Self> {
        Ok(Self {
            tables: Arc::new(IdGenerator::new(conn).await?),
            fragments: Arc::new(IdGenerator::new(conn).await?),
            actors: Arc::new(IdGenerator::new(conn).await?),
        })
    }

    pub fn generate<const C: IdCategoryType>(&self) -> u64 {
        match C {
            IdCategory::Table => self.tables.generate_interval(1),
            IdCategory::Fragment => self.fragments.generate_interval(1),
            IdCategory::Actor => self.actors.generate_interval(1),
            _ => unreachable!("IdGeneratorV2 only supports Table, Fragment, and Actor"),
        }
    }

    pub fn generate_interval<const C: IdCategoryType>(&self, interval: u64) -> u64 {
        match C {
            IdCategory::Table => self.tables.generate_interval(interval),
            IdCategory::Fragment => self.fragments.generate_interval(interval),
            IdCategory::Actor => self.actors.generate_interval(interval),
            _ => unreachable!("IdGeneratorV2 only supports Table, Fragment, and Actor"),
        }
    }
}
