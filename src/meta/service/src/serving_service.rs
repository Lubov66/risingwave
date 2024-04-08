// Copyright 2024 RisingWave Labs
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

use itertools::Itertools;
use risingwave_meta::manager::MetadataManager;
use risingwave_pb::meta::serving_service_server::ServingService;
use risingwave_pb::meta::{
    FragmentWorkerMapping, GetServingVnodeMappingsRequest,
    GetServingVnodeMappingsResponse,
};
use tonic::{Request, Response, Status};

use crate::serving::ServingVnodeMappingRef;

pub struct ServingServiceImpl {
    serving_vnode_mapping: ServingVnodeMappingRef,
    metadata_manager: MetadataManager,
}

impl ServingServiceImpl {
    pub fn new(
        serving_vnode_mapping: ServingVnodeMappingRef,
        metadata_manager: MetadataManager,
    ) -> Self {
        Self {
            serving_vnode_mapping,
            metadata_manager,
        }
    }
}

#[async_trait::async_trait]
impl ServingService for ServingServiceImpl {
    async fn get_serving_vnode_mappings(
        &self,
        _request: Request<GetServingVnodeMappingsRequest>,
    ) -> Result<Response<GetServingVnodeMappingsResponse>, Status> {
        let mappings = self
            .serving_vnode_mapping
            .all()
            .into_iter()
            .map(|(fragment_id, mapping)| FragmentWorkerMapping {
                fragment_id,
                mapping: Some(mapping.to_protobuf()),
            })
            .collect();
        let fragment_to_table = {
            match &self.metadata_manager {
                MetadataManager::V1(mgr) => {
                    let guard = mgr.fragment_manager.get_fragment_read_guard().await;
                    guard
                        .table_fragments()
                        .iter()
                        .flat_map(|(table_id, tf)| {
                            tf.fragment_ids()
                                .map(|fragment_id| (fragment_id, table_id.table_id))
                                .collect_vec()
                        })
                        .collect()
                }
                MetadataManager::V2(mgr) => mgr
                    .catalog_controller
                    .fragment_job_mapping()
                    .await?
                    .into_iter()
                    .map(|(fragment_id, job_id)| (fragment_id as u32, job_id as u32))
                    .collect(),
            }
        };
        Ok(Response::new(GetServingVnodeMappingsResponse {
            mappings: vec![],
            fragment_to_table,
            worker_mappings: mappings,
        }))
    }
}
