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

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::anyhow;
use elasticsearch::http::transport::{SingleNodeConnectionPool, TransportBuilder};
use elasticsearch::{BulkOperation, BulkParts, Elasticsearch, Error};
use futures::prelude::TryFuture;
use futures::FutureExt;
use risingwave_common::array::{Op, StreamChunk};
use risingwave_common::catalog::Schema;
use risingwave_common::row::Row;
use risingwave_common::types::{DataType, ToText};
use serde::Deserialize;
use serde_json::Value;
use tonic::async_trait;
use url::Url;
use with_options::WithOptions;

use super::encoder::template::TemplateEncoder;
use super::encoder::{JsonEncoder, RowEncoder};
use super::log_store::DeliveryFutureManagerAddFuture;
use super::writer::{
    AsyncTruncateLogSinkerOf, AsyncTruncateSinkWriter, AsyncTruncateSinkWriterExt,
};
use super::{DummySinkCommitCoordinator, Sink, SinkError, SinkParam, SinkWriterParam};
use crate::sink::Result;

pub const ES_SINK: &str = "elasticsearch";
pub const OPENSEARCH_SINK: &str = "opensearch";
pub const ES_OPTION_INDEX_COLUMN: &str = "index_column";

pub type ElasticSearchSinkDeliveryFuture =
    impl TryFuture<Ok = (), Error = SinkError> + Unpin + 'static;

#[derive(Deserialize, Debug, Clone, WithOptions)]
pub struct ElasticSearchOpenSearchConfig {
    #[serde(rename = "url")]
    pub url: String,
    /// The index's name of elasticsearch or openserach
    #[serde(rename = "index")]
    pub index: Option<String>,
    /// If pk is set, then "pk1+delimiter+pk2+delimiter..." will be used as the key, if pk is not set, we will just use the first column as the key.
    #[serde(rename = "delimiter")]
    pub delimiter: Option<String>,
    /// The username of elasticsearch or openserach
    #[serde(rename = "username")]
    pub username: String,
    /// The username of elasticsearch or openserach
    #[serde(rename = "password")]
    pub password: String,
    /// It is used for dynamic index, if it is be set, the value of this column will be used as the index. It and `index` can only set one
    #[serde(rename = "index_column")]
    pub index_column: Option<usize>,
}

impl ElasticSearchOpenSearchConfig {
    pub fn from_btreemap(
        mut properties: BTreeMap<String, String>,
        schema: &Schema,
    ) -> Result<Self> {
        let index_column = properties
                .get(ES_OPTION_INDEX_COLUMN)
                .cloned()
                .map(|n| {
                    schema
                        .fields()
                        .iter()
                        .position(|s| s.name == n)
                        .ok_or_else(|| anyhow!("please ensure that '{}' is set to an existing column within the schema.", n))
                })
                .transpose()?;
        properties.insert(
            ES_OPTION_INDEX_COLUMN.to_string(),
            index_column.unwrap().to_string(),
        );
        let config = serde_json::from_value::<ElasticSearchOpenSearchConfig>(
            serde_json::to_value(properties).unwrap(),
        )
        .map_err(|e| SinkError::Config(anyhow!(e)))?;
        Ok(config)
    }
}

#[derive(Debug)]
pub struct ElasticSearchSink {
    config: ElasticSearchOpenSearchConfig,
    schema: Schema,
    pk_indices: Vec<usize>,
    is_append_only: bool,
}

#[derive(Debug)]
pub struct OpenSearchSink {
    config: ElasticSearchOpenSearchConfig,
    schema: Schema,
    pk_indices: Vec<usize>,
    is_append_only: bool,
}

#[async_trait]
impl TryFrom<SinkParam> for ElasticSearchSink {
    type Error = SinkError;

    fn try_from(param: SinkParam) -> std::result::Result<Self, Self::Error> {
        let schema = param.schema();
        let config = ElasticSearchOpenSearchConfig::from_btreemap(param.properties, &schema)?;
        Ok(Self {
            config,
            schema,
            pk_indices: param.downstream_pk,
            is_append_only: param.sink_type.is_append_only(),
        })
    }
}

#[async_trait]
impl TryFrom<SinkParam> for OpenSearchSink {
    type Error = SinkError;

    fn try_from(param: SinkParam) -> std::result::Result<Self, Self::Error> {
        let schema = param.schema();
        let config = ElasticSearchOpenSearchConfig::from_btreemap(param.properties, &schema)?;
        Ok(Self {
            config,
            schema,
            pk_indices: param.downstream_pk,
            is_append_only: param.sink_type.is_append_only(),
        })
    }
}

fn validate_config(config: &ElasticSearchOpenSearchConfig, schema: &Schema) -> Result<()> {
    if config.index_column.is_some() && config.index.is_some()
        || config.index_column.is_none() && config.index.is_none()
    {
        return Err(SinkError::Config(anyhow!(
            "please set only one of the 'index_column' or 'index' properties."
        )));
    }

    if let Some(index_column) = &config.index_column {
        let filed = schema.fields().get(*index_column).unwrap();
        if filed.data_type() != DataType::Varchar {
            return Err(SinkError::Config(anyhow!(
                "please ensure the data type of {} is varchar.",
                index_column
            )));
        }
    }
    Ok(())
}

impl Sink for ElasticSearchSink {
    type Coordinator = DummySinkCommitCoordinator;
    type LogSinker = AsyncTruncateLogSinkerOf<ElasticSearchSinkWriter>;

    const SINK_NAME: &'static str = ES_SINK;

    async fn validate(&self) -> Result<()> {
        validate_config(&self.config, &self.schema)?;
        let url = Url::parse(&self.config.url)
            .map_err(|e| SinkError::ElasticSearchOpenSearch(anyhow!(e)))?;
        let transport = TransportBuilder::new(SingleNodeConnectionPool::new(url))
            .auth(elasticsearch::auth::Credentials::ApiKey(
                self.config.username.clone(),
                self.config.password.clone(),
            ))
            .build()
            .map_err(|e| SinkError::ElasticSearchOpenSearch(anyhow!(e)))?;
        let client = Elasticsearch::new(transport);
        client.ping().send().await?;
        Ok(())
    }

    async fn new_log_sinker(&self, _writer_param: SinkWriterParam) -> Result<Self::LogSinker> {
        Ok(ElasticSearchSinkWriter::new(
            self.config.clone(),
            self.schema.clone(),
            self.pk_indices.clone(),
        )?
        .into_log_sinker(usize::MAX))
    }
}

pub struct ElasticSearchSinkWriter {
    config: ElasticSearchOpenSearchConfig,
    schema: Schema,
    pk_indices: Vec<usize>,
    client: Arc<Elasticsearch>,
    key_encoder: TemplateEncoder,
    value_encoder: JsonEncoder,
}

impl ElasticSearchSinkWriter {
    pub fn new(
        config: ElasticSearchOpenSearchConfig,
        schema: Schema,
        pk_indices: Vec<usize>,
    ) -> Result<Self> {
        let url =
            Url::parse(&config.url).map_err(|e| SinkError::ElasticSearchOpenSearch(anyhow!(e)))?;
        let transport = TransportBuilder::new(SingleNodeConnectionPool::new(url))
            .auth(elasticsearch::auth::Credentials::ApiKey(
                config.username.clone(),
                config.password.clone(),
            ))
            .build()
            .map_err(|e| SinkError::ElasticSearchOpenSearch(anyhow!(e)))?;
        let client = Arc::new(Elasticsearch::new(transport));
        let key_format = if pk_indices.is_empty() {
            let name = &schema
                .fields()
                .get(0)
                .ok_or_else(|| {
                    SinkError::ElasticSearchOpenSearch(anyhow!(
                        "no value find in sink schema, index is 0"
                    ))
                })?
                .name;
            format!("{{{}}}", name)
        } else if pk_indices.len() == 1 {
            let index = *pk_indices.get(0).unwrap();
            let name = &schema
                .fields()
                .get(index)
                .ok_or_else(|| {
                    SinkError::ElasticSearchOpenSearch(anyhow!(
                        "no value find in sink schema, index is {:?}",
                        index
                    ))
                })?
                .name;
            format!("{{{}}}", name)
        } else {
            let delimiter = config.delimiter
                .as_ref()
                .ok_or_else(|| anyhow!("please set the separator in the with option, when there are multiple primary key values"))?
                .clone();
            let mut names = Vec::with_capacity(pk_indices.len());
            for index in &pk_indices {
                names.push(format!(
                    "{{{}}}",
                    schema
                        .fields()
                        .get(*index)
                        .ok_or_else(|| {
                            SinkError::ElasticSearchOpenSearch(anyhow!(
                                "no value find in sink schema, index is {:?}",
                                index
                            ))
                        })?
                        .name
                ));
            }
            names.join(&delimiter)
        };
        let col_indices = if let Some(index) = config.index_column {
            let mut col_indices: Vec<usize> = (0..schema.len()).collect();
            col_indices.remove(index);
            Some(col_indices)
        } else {
            None
        };
        let key_encoder = TemplateEncoder::new(schema.clone(), col_indices.clone(), key_format);
        let value_encoder = JsonEncoder::new_with_es(schema.clone(), col_indices.clone());
        Ok(Self {
            config,
            schema,
            pk_indices,
            client,
            key_encoder,
            value_encoder,
        })
    }
}

impl AsyncTruncateSinkWriter for ElasticSearchSinkWriter {
    type DeliveryFuture = ElasticSearchSinkDeliveryFuture;

    async fn write_chunk<'a>(
        &'a mut self,
        chunk: StreamChunk,
        mut add_future: DeliveryFutureManagerAddFuture<'a, Self::DeliveryFuture>,
    ) -> Result<()> {
        let mut bulks: Vec<BulkOperation<_>> = Vec::with_capacity(chunk.capacity());
        for (op, rows) in chunk.rows() {
            let index = if let Some(index_column) = self.config.index_column {
                rows.datum_at(index_column)
                    .ok_or_else(|| {
                        SinkError::ElasticSearchOpenSearch(anyhow!(
                            "no value find in sink schema, index is {:?}",
                            index_column
                        ))
                    })?
                    .into_utf8()
            } else {
                self.config.index.as_ref().unwrap()
            };
            match op {
                Op::Insert | Op::UpdateInsert => {
                    let key = self.key_encoder.encode(rows)?;
                    let value = self.value_encoder.encode(rows)?;
                    bulks.push(BulkOperation::index(value).index(index).id(key).into());
                }
                Op::Delete => {
                    let key = self.key_encoder.encode(rows)?;
                    bulks.push(BulkOperation::delete(key).index(index).into());
                }
                Op::UpdateDelete => continue,
            }
        }
        let clent_clone = self.client.clone();
        let future = async move {
            let result = clent_clone.bulk(BulkParts::None).body(bulks).send().await?;
            let json = result.json::<Value>().await?;
            if json["errors"].as_bool().unwrap() {
                let failed: Vec<&Value> = json["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|v| !v["error"].is_null())
                    .collect();
                Err(SinkError::ElasticSearchOpenSearch(anyhow!(
                    "send bulk to elasticsearch failed: {:?}",
                    failed
                )))
            } else {
                Ok(())
            }
        }
        .boxed();
        add_future.add_future_may_await(future).await?;
        Ok(())
    }
}

// impl Sink for OpenSearchSink {
//     const SINK_NAME: &'static str = OPENSEARCH_SINK;
//     type Coordinator = DummySinkCommitCoordinator;
//     type LogSinker = AsyncTruncateLogSinkerOf<ElasticSearchOpenSearchSinkWriter>;

//     async fn validate(&self) -> Result<()> {
//         validate_config(&self.config,&self.schema)?;
//         Ok(())
//     }

//     async fn new_log_sinker(&self, writer_param: SinkWriterParam) -> Result<Self::LogSinker> {
//         self.inner.new_log_sinker(writer_param,ElasticSearchOpenSearchSinkType::OpenSearch).await
//     }
// }
