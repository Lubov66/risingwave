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

use std::collections::HashMap;
use std::time::Duration;

use anyhow::anyhow;
use either::Either;
use futures::TryStreamExt;
use itertools::Itertools;
use risingwave_common::array::ArrayRef;
use risingwave_common::metrics::{LabelGuardedIntCounter, GLOBAL_ERROR_METRICS};
use risingwave_common::system_param::local_manager::SystemParamsReaderRef;
use risingwave_common::system_param::reader::SystemParamsRead;
use risingwave_common::util::epoch::{Epoch, EpochPair};
use risingwave_connector::source::reader::desc::{SourceDesc, SourceDescBuilder};
use risingwave_connector::source::reader::reader::SourceReader;
use risingwave_connector::source::{
    BoxChunkSourceStream, ConnectorState, SourceContext, SourceCtrlOpts, SplitId, SplitImpl,
    SplitMetaData, WaitCheckpointTask,
};
use risingwave_hummock_sdk::HummockReadEpoch;
use thiserror_ext::AsReport;
use tokio::sync::mpsc;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::time::Instant;

use super::executor_core::StreamSourceCore;
use super::{
    apply_rate_limit, barrier_to_message_stream, get_split_offset_col_idx,
    get_split_offset_mapping_from_chunk, prune_additional_cols,
};
use crate::common::rate_limit::limited_chunk_size;
use crate::executor::prelude::*;
use crate::executor::stream_reader::StreamReaderWithPause;
use crate::executor::{AddMutation, UpdateMutation};

/// A constant to multiply when calculating the maximum time to wait for a barrier. This is due to
/// some latencies in network and cost in meta.
pub const WAIT_BARRIER_MULTIPLE_TIMES: u128 = 5;

pub struct SourceExecutor<S: StateStore> {
    actor_ctx: ActorContextRef,

    /// Streaming source for external
    stream_source_core: Option<StreamSourceCore<S>>,

    /// Metrics for monitor.
    metrics: Arc<StreamingMetrics>,

    /// Receiver of barrier channel.
    barrier_receiver: Option<UnboundedReceiver<Barrier>>,

    /// System parameter reader to read barrier interval
    system_params: SystemParamsReaderRef,

    /// Rate limit in rows/s.
    rate_limit_rps: Option<u32>,

    is_shared: bool,
}

impl<S: StateStore> SourceExecutor<S> {
    pub fn new(
        actor_ctx: ActorContextRef,
        stream_source_core: Option<StreamSourceCore<S>>,
        metrics: Arc<StreamingMetrics>,
        barrier_receiver: UnboundedReceiver<Barrier>,
        system_params: SystemParamsReaderRef,
        rate_limit_rps: Option<u32>,
        is_shared: bool,
    ) -> Self {
        Self {
            actor_ctx,
            stream_source_core,
            metrics,
            barrier_receiver: Some(barrier_receiver),
            system_params,
            rate_limit_rps,
            is_shared,
        }
    }

    async fn spawn_wait_checkpoint_worker(
        core: &StreamSourceCore<S>,
        source_reader: SourceReader,
    ) -> StreamExecutorResult<Option<WaitCheckpointTaskBuilder>> {
        let Some(initial_task) = source_reader.create_wait_checkpoint_task().await? else {
            return Ok(None);
        };
        let (wait_checkpoint_tx, wait_checkpoint_rx) = mpsc::unbounded_channel();
        let wait_checkpoint_worker = WaitCheckpointWorker {
            wait_checkpoint_rx,
            state_store: core.split_state_store.state_table.state_store().clone(),
        };
        tokio::spawn(wait_checkpoint_worker.run());
        Ok(Some(WaitCheckpointTaskBuilder {
            wait_checkpoint_tx,
            source_reader,
            building_task: initial_task,
        }))
    }

    pub async fn build_stream_source_reader(
        &self,
        source_desc: &SourceDesc,
        state: ConnectorState,
    ) -> StreamExecutorResult<BoxChunkSourceStream> {
        let column_ids = source_desc
            .columns
            .iter()
            .map(|column_desc| column_desc.column_id)
            .collect_vec();
        let source_ctx = SourceContext::new(
            self.actor_ctx.id,
            self.stream_source_core.as_ref().unwrap().source_id,
            self.actor_ctx.fragment_id,
            self.stream_source_core
                .as_ref()
                .unwrap()
                .source_name
                .clone(),
            source_desc.metrics.clone(),
            SourceCtrlOpts {
                chunk_size: limited_chunk_size(self.rate_limit_rps),
                rate_limit: self.rate_limit_rps,
            },
            source_desc.source.config.clone(),
        );
        let stream = source_desc
            .source
            .build_stream(state, column_ids, Arc::new(source_ctx))
            .await
            .map_err(StreamExecutorError::connector_error);

        Ok(apply_rate_limit(stream?, self.rate_limit_rps).boxed())
    }

    /// `source_id | source_name | actor_id | fragment_id`
    #[inline]
    fn get_metric_labels(&self) -> [String; 4] {
        [
            self.stream_source_core
                .as_ref()
                .unwrap()
                .source_id
                .to_string(),
            self.stream_source_core
                .as_ref()
                .unwrap()
                .source_name
                .clone(),
            self.actor_ctx.id.to_string(),
            self.actor_ctx.fragment_id.to_string(),
        ]
    }

    /// - `should_trim_state`: whether to trim state for dropped splits.
    ///
    ///    For scaling, the connector splits can be migrated to other actors, but
    ///    won't be added or removed. Actors should not trim states for splits that
    ///    are moved to other actors.
    ///
    ///    For source split change, split will not be migrated and we can trim states
    ///    for deleted splits.
    async fn apply_split_change<const BIASED: bool>(
        &mut self,
        source_desc: &SourceDesc,
        stream: &mut StreamReaderWithPause<BIASED, StreamChunk>,
        split_assignment: &HashMap<ActorId, Vec<SplitImpl>>,
        should_trim_state: bool,
        source_split_change_count_metrics: &LabelGuardedIntCounter<4>,
    ) -> StreamExecutorResult<()> {
        source_split_change_count_metrics.inc();
        if let Some(target_splits) = split_assignment.get(&self.actor_ctx.id).cloned() {
            if self
                .update_state_if_changed(target_splits, should_trim_state)
                .await?
            {
                self.rebuild_stream_reader(source_desc, stream).await?;
            }
        }

        Ok(())
    }

    /// Returns `true` if split changed. Otherwise `false`.
    async fn update_state_if_changed(
        &mut self,
        target_splits: Vec<SplitImpl>,
        should_trim_state: bool,
    ) -> StreamExecutorResult<bool> {
        let core = self.stream_source_core.as_mut().unwrap();

        let target_splits: HashMap<_, _> = target_splits
            .into_iter()
            .map(|split| (split.id(), split))
            .collect();

        let mut target_state: HashMap<SplitId, SplitImpl> =
            HashMap::with_capacity(target_splits.len());

        let mut split_changed = false;

        // Checks added splits
        for (split_id, split) in target_splits {
            if let Some(s) = core.latest_split_info.get(&split_id) {
                // For existing splits, we should use the latest offset from the cache.
                // `target_splits` is from meta and contains the initial offset.
                target_state.insert(split_id, s.clone());
            } else {
                split_changed = true;
                // write new assigned split to state cache. snapshot is base on cache.

                let initial_state = if let Some(recover_state) = core
                    .split_state_store
                    .try_recover_from_state_store(&split)
                    .await?
                {
                    recover_state
                } else {
                    split
                };

                core.updated_splits_in_epoch
                    .entry(split_id.clone())
                    .or_insert_with(|| initial_state.clone());

                target_state.insert(split_id, initial_state);
            }
        }

        // Checks dropped splits
        for existing_split_id in core.latest_split_info.keys() {
            if !target_state.contains_key(existing_split_id) {
                tracing::info!("split dropping detected: {}", existing_split_id);
                split_changed = true;
            }
        }

        if split_changed {
            tracing::info!(
                actor_id = self.actor_ctx.id,
                state = ?target_state,
                "apply split change"
            );

            core.updated_splits_in_epoch
                .retain(|split_id, _| target_state.contains_key(split_id));

            let dropped_splits = core
                .latest_split_info
                .extract_if(|split_id, _| !target_state.contains_key(split_id))
                .map(|(_, split)| split)
                .collect_vec();

            if should_trim_state && !dropped_splits.is_empty() {
                // trim dropped splits' state
                core.split_state_store.trim_state(&dropped_splits).await?;
            }

            core.latest_split_info = target_state;
        }

        Ok(split_changed)
    }

    /// Rebuild stream if there is a err in stream
    async fn rebuild_stream_reader_from_error<const BIASED: bool>(
        &mut self,
        source_desc: &SourceDesc,
        stream: &mut StreamReaderWithPause<BIASED, StreamChunk>,
        e: StreamExecutorError,
    ) -> StreamExecutorResult<()> {
        let core = self.stream_source_core.as_mut().unwrap();
        tracing::warn!(
            error = ?e.as_report(),
            actor_id = self.actor_ctx.id,
            source_id = %core.source_id,
            "stream source reader error",
        );
        GLOBAL_ERROR_METRICS.user_source_error.report([
            e.variant_name().to_owned(),
            core.source_id.to_string(),
            core.source_name.to_owned(),
            self.actor_ctx.fragment_id.to_string(),
        ]);

        self.rebuild_stream_reader(source_desc, stream).await
    }

    async fn rebuild_stream_reader<const BIASED: bool>(
        &mut self,
        source_desc: &SourceDesc,
        stream: &mut StreamReaderWithPause<BIASED, StreamChunk>,
    ) -> StreamExecutorResult<()> {
        let core = self.stream_source_core.as_mut().unwrap();
        let target_state: Vec<SplitImpl> = core.latest_split_info.values().cloned().collect();

        tracing::info!(
            "actor {:?} apply source split change to {:?}",
            self.actor_ctx.id,
            target_state
        );

        // Replace the source reader with a new one of the new state.
        let reader = self
            .build_stream_source_reader(source_desc, Some(target_state.clone()))
            .await?
            .map_err(StreamExecutorError::connector_error);

        stream.replace_data_stream(reader);

        Ok(())
    }

    async fn persist_state_and_clear_cache(
        &mut self,
        epoch: EpochPair,
    ) -> StreamExecutorResult<HashMap<SplitId, SplitImpl>> {
        let core = self.stream_source_core.as_mut().unwrap();

        let cache = core
            .updated_splits_in_epoch
            .values()
            .map(|split_impl| split_impl.to_owned())
            .collect_vec();

        if !cache.is_empty() {
            tracing::debug!(state = ?cache, "take snapshot");
            core.split_state_store.set_states(cache).await?;
        }

        // commit anyway, even if no message saved
        core.split_state_store.state_table.commit(epoch).await?;

        let updated_splits = core.updated_splits_in_epoch.clone();

        core.updated_splits_in_epoch.clear();

        Ok(updated_splits)
    }

    /// try mem table spill
    async fn try_flush_data(&mut self) -> StreamExecutorResult<()> {
        let core = self.stream_source_core.as_mut().unwrap();
        core.split_state_store.state_table.try_flush().await?;

        Ok(())
    }

    /// A source executor with a stream source receives:
    /// 1. Barrier messages
    /// 2. Data from external source
    /// and acts accordingly.
    #[try_stream(ok = Message, error = StreamExecutorError)]
    async fn execute_with_stream_source(mut self) {
        let mut barrier_receiver = self.barrier_receiver.take().unwrap();
        let barrier = barrier_receiver
            .recv()
            .instrument_await("source_recv_first_barrier")
            .await
            .ok_or_else(|| {
                anyhow!(
                    "failed to receive the first barrier, actor_id: {:?}, source_id: {:?}",
                    self.actor_ctx.id,
                    self.stream_source_core.as_ref().unwrap().source_id
                )
            })?;

        let mut core = self.stream_source_core.unwrap();

        // Build source description from the builder.
        let source_desc_builder: SourceDescBuilder = core.source_desc_builder.take().unwrap();
        let source_desc = source_desc_builder
            .build()
            .map_err(StreamExecutorError::connector_error)?;

        let mut wait_checkpoint_task_builder =
            Self::spawn_wait_checkpoint_worker(&core, source_desc.source.clone()).await?;

        let (Some(split_idx), Some(offset_idx)) = get_split_offset_col_idx(&source_desc.columns)
        else {
            unreachable!("Partition and offset columns must be set.");
        };

        let mut boot_state = Vec::default();
        if let Some(
            Mutation::Add(AddMutation { splits, .. })
            | Mutation::Update(UpdateMutation {
                actor_splits: splits,
                ..
            }),
        ) = barrier.mutation.as_deref()
        {
            if let Some(splits) = splits.get(&self.actor_ctx.id) {
                tracing::debug!(
                    "source exector: actor {:?} boot with splits: {:?}",
                    self.actor_ctx.id,
                    splits
                );
                boot_state.clone_from(splits);
            }
        }

        core.split_state_store.init_epoch(barrier.epoch);

        for ele in &mut boot_state {
            if let Some(recover_state) = core
                .split_state_store
                .try_recover_from_state_store(ele)
                .await?
            {
                *ele = recover_state;
            } else {
                // This is a new split, not in state table.
                if self.is_shared {
                    // For shared source, we start from latest and let the downstream SourceBackfillExecutors to read historical data.
                    // It's highly probable that the work of scanning historical data cannot be shared,
                    // so don't waste work on it.
                    // For more details, see https://github.com/risingwavelabs/risingwave/issues/16576#issuecomment-2095413297
                    if ele.is_cdc_split() {
                        // shared CDC source already starts from latest.
                        continue;
                    }
                    match ele {
                        SplitImpl::Kafka(split) => {
                            split.seek_to_latest_offset();
                        }
                        _ => unreachable!("only kafka source can be shared, got {:?}", ele),
                    }
                }
            }
        }

        // init in-memory split states with persisted state if any
        core.init_split_state(boot_state.clone());
        let mut is_uninitialized = self.actor_ctx.initial_dispatch_num == 0;

        // Return the ownership of `stream_source_core` to the source executor.
        self.stream_source_core = Some(core);

        let recover_state: ConnectorState = (!boot_state.is_empty()).then_some(boot_state);
        tracing::debug!(state = ?recover_state, "start with state");
        let source_chunk_reader = self
            .build_stream_source_reader(&source_desc, recover_state)
            .instrument_await("source_build_reader")
            .await?
            .map_err(StreamExecutorError::connector_error);

        // Merge the chunks from source and the barriers into a single stream. We prioritize
        // barriers over source data chunks here.
        let barrier_stream = barrier_to_message_stream(barrier_receiver).boxed();
        let mut stream =
            StreamReaderWithPause::<true, StreamChunk>::new(barrier_stream, source_chunk_reader);

        // - For shared source, pause until there's a MV.
        // - If the first barrier requires us to pause on startup, pause the stream.
        if (self.is_shared && is_uninitialized) || barrier.is_pause_on_startup() {
            tracing::info!(
                is_shared = self.is_shared,
                is_uninitialized = is_uninitialized,
                "source paused on startup"
            );
            stream.pause_stream();
        }

        yield Message::Barrier(barrier);

        // We allow data to flow for `WAIT_BARRIER_MULTIPLE_TIMES` * `expected_barrier_latency_ms`
        // milliseconds, considering some other latencies like network and cost in Meta.
        let mut max_wait_barrier_time_ms =
            self.system_params.load().barrier_interval_ms() as u128 * WAIT_BARRIER_MULTIPLE_TIMES;
        let mut last_barrier_time = Instant::now();
        let mut self_paused = false;

        let source_output_row_count = self
            .metrics
            .source_output_row_count
            .with_guarded_label_values(&self.get_metric_labels().each_ref().map(AsRef::as_ref));

        let source_split_change_count = self
            .metrics
            .source_split_change_count
            .with_guarded_label_values(&self.get_metric_labels().each_ref().map(AsRef::as_ref));

        while let Some(msg) = stream.next().await {
            let Ok(msg) = msg else {
                tokio::time::sleep(Duration::from_millis(1000)).await;
                self.rebuild_stream_reader_from_error(&source_desc, &mut stream, msg.unwrap_err())
                    .await?;
                continue;
            };

            match msg {
                // This branch will be preferred.
                Either::Left(Message::Barrier(barrier)) => {
                    last_barrier_time = Instant::now();

                    if self_paused {
                        stream.resume_stream();
                        self_paused = false;
                    }

                    let epoch = barrier.epoch;

                    if self.is_shared
                        && is_uninitialized
                        && barrier.has_more_downstream_fragments(self.actor_ctx.id)
                    {
                        stream.resume_stream();
                        is_uninitialized = false;
                    }

                    if let Some(mutation) = barrier.mutation.as_deref() {
                        match mutation {
                            // XXX: Is it possible that the stream is self_paused, and we have pause mutation now? In this case, it will panic.
                            Mutation::Pause => stream.pause_stream(),
                            Mutation::Resume => stream.resume_stream(),
                            Mutation::SourceChangeSplit(actor_splits) => {
                                tracing::info!(
                                    actor_id = self.actor_ctx.id,
                                    actor_splits = ?actor_splits,
                                    "source change split received"
                                );

                                self.apply_split_change(
                                    &source_desc,
                                    &mut stream,
                                    actor_splits,
                                    true,
                                    &source_split_change_count,
                                )
                                .await?;
                            }

                            Mutation::Update(UpdateMutation { actor_splits, .. }) => {
                                self.apply_split_change(
                                    &source_desc,
                                    &mut stream,
                                    actor_splits,
                                    false,
                                    &source_split_change_count,
                                )
                                .await?;
                            }
                            Mutation::Throttle(actor_to_apply) => {
                                if let Some(new_rate_limit) = actor_to_apply.get(&self.actor_ctx.id)
                                    && *new_rate_limit != self.rate_limit_rps
                                {
                                    self.rate_limit_rps = *new_rate_limit;
                                    // recreate from latest_split_info
                                    self.rebuild_stream_reader(&source_desc, &mut stream)
                                        .await?;
                                }
                            }
                            _ => {}
                        }
                    }

                    let updated_splits = self.persist_state_and_clear_cache(epoch).await?;

                    // when handle a checkpoint barrier, spawn a task to wait for epoch commit notification
                    if barrier.kind.is_checkpoint()
                        && let Some(task_builder) = &mut wait_checkpoint_task_builder
                    {
                        task_builder.update_task_on_checkpoint(updated_splits);

                        tracing::debug!("epoch to wait {:?}", epoch);
                        task_builder.send(Epoch(epoch.prev)).await?
                    }

                    yield Message::Barrier(barrier);
                }
                Either::Left(_) => {
                    // For the source executor, the message we receive from this arm
                    // should always be barrier message.
                    unreachable!();
                }

                Either::Right(chunk) => {
                    if let Some(task_builder) = &mut wait_checkpoint_task_builder {
                        let offset_col = chunk.column_at(offset_idx);
                        task_builder.update_task_on_chunk(offset_col.clone());
                    }
                    // TODO: confirm when split_offset_mapping is None
                    let split_offset_mapping =
                        get_split_offset_mapping_from_chunk(&chunk, split_idx, offset_idx);
                    if last_barrier_time.elapsed().as_millis() > max_wait_barrier_time_ms {
                        // Exceeds the max wait barrier time, the source will be paused.
                        // Currently we can guarantee the
                        // source is not paused since it received stream
                        // chunks.
                        self_paused = true;
                        tracing::warn!(
                            "source paused, wait barrier for {:?}",
                            last_barrier_time.elapsed()
                        );
                        stream.pause_stream();

                        // Only update `max_wait_barrier_time_ms` to capture
                        // `barrier_interval_ms`
                        // changes here to avoid frequently accessing the shared
                        // `system_params`.
                        max_wait_barrier_time_ms = self.system_params.load().barrier_interval_ms()
                            as u128
                            * WAIT_BARRIER_MULTIPLE_TIMES;
                    }
                    if let Some(mapping) = split_offset_mapping {
                        let state: HashMap<_, _> = mapping
                            .iter()
                            .flat_map(|(split_id, offset)| {
                                self.stream_source_core
                                    .as_mut()
                                    .unwrap()
                                    .latest_split_info
                                    .get_mut(split_id)
                                    .map(|original_split_impl| {
                                        original_split_impl.update_in_place(offset.clone())?;
                                        Ok::<_, anyhow::Error>((
                                            split_id.clone(),
                                            original_split_impl.clone(),
                                        ))
                                    })
                            })
                            .try_collect()?;

                        self.stream_source_core
                            .as_mut()
                            .unwrap()
                            .updated_splits_in_epoch
                            .extend(state);
                    }

                    source_output_row_count.inc_by(chunk.cardinality() as u64);
                    let chunk =
                        prune_additional_cols(&chunk, split_idx, offset_idx, &source_desc.columns);
                    yield Message::Chunk(chunk);
                    self.try_flush_data().await?;
                }
            }
        }

        // The source executor should only be stopped by the actor when finding a `Stop` mutation.
        tracing::error!(
            actor_id = self.actor_ctx.id,
            "source executor exited unexpectedly"
        )
    }

    /// A source executor without stream source only receives barrier messages and sends them to
    /// the downstream executor.
    #[try_stream(ok = Message, error = StreamExecutorError)]
    async fn execute_without_stream_source(mut self) {
        let mut barrier_receiver = self.barrier_receiver.take().unwrap();
        let barrier = barrier_receiver
            .recv()
            .instrument_await("source_recv_first_barrier")
            .await
            .ok_or_else(|| {
                anyhow!(
                    "failed to receive the first barrier, actor_id: {:?} with no stream source",
                    self.actor_ctx.id
                )
            })?;
        yield Message::Barrier(barrier);

        while let Some(barrier) = barrier_receiver.recv().await {
            yield Message::Barrier(barrier);
        }
    }
}

impl<S: StateStore> Execute for SourceExecutor<S> {
    fn execute(self: Box<Self>) -> BoxedMessageStream {
        if self.stream_source_core.is_some() {
            self.execute_with_stream_source().boxed()
        } else {
            self.execute_without_stream_source().boxed()
        }
    }
}

impl<S: StateStore> Debug for SourceExecutor<S> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        if let Some(core) = &self.stream_source_core {
            f.debug_struct("SourceExecutor")
                .field("source_id", &core.source_id)
                .field("column_ids", &core.column_ids)
                .finish()
        } else {
            f.debug_struct("SourceExecutor").finish()
        }
    }
}

struct WaitCheckpointTaskBuilder {
    wait_checkpoint_tx: UnboundedSender<(Epoch, WaitCheckpointTask)>,
    source_reader: SourceReader,
    building_task: WaitCheckpointTask,
}

impl WaitCheckpointTaskBuilder {
    fn update_task_on_chunk(&mut self, offset_col: ArrayRef) {
        #[expect(clippy::single_match)]
        match &mut self.building_task {
            WaitCheckpointTask::AckPubsubMessage(_, arrays) => {
                arrays.push(offset_col);
            }
            _ => {}
        }
    }

    fn update_task_on_checkpoint(&mut self, updated_splits: HashMap<SplitId, SplitImpl>) {
        #[expect(clippy::single_match)]
        match &mut self.building_task {
            WaitCheckpointTask::CommitCdcOffset(offsets) => {
                if !updated_splits.is_empty() {
                    // cdc source only has one split
                    assert_eq!(1, updated_splits.len());
                    for (split_id, split_impl) in updated_splits {
                        if split_impl.is_cdc_split() {
                            *offsets = Some((split_id, split_impl.get_cdc_split_offset()));
                        } else {
                            unreachable!()
                        }
                    }
                }
            }
            _ => {}
        }
    }

    /// Send and reset the building task to a new one.
    async fn send(&mut self, epoch: Epoch) -> Result<(), anyhow::Error> {
        let new_task = self
            .source_reader
            .create_wait_checkpoint_task()
            .await?
            .expect("wait checkpoint task should be created");
        self.wait_checkpoint_tx
            .send((epoch, std::mem::replace(&mut self.building_task, new_task)))
            .expect("wait_checkpoint_tx send should succeed");
        Ok(())
    }
}

/// A worker used to do some work after each checkpoint epoch is committed.
///
/// # Usage Cases
///
/// Typically there are 2 issues related with ack on checkpoint:
///
/// 1. Correctness (at-least-once), or don't let upstream clean uncommitted data.
///    For message queueing semantics (delete after ack), we should ack to avoid redelivery,
///    and only ack after checkpoint to avoid data loss.
///
/// 2. Allow upstream to clean data after commit.
///
/// See also <https://github.com/risingwavelabs/risingwave/issues/16736#issuecomment-2109379790>
///
/// ## CDC
///
/// Commit last consumed offset to upstream DB, so that old data can be discarded.
///
/// ## Google Pub/Sub
///
/// Due to queueing semantics.
/// Although Pub/Sub supports `retain_acked_messages` and `seek` functionality,
/// it's quite limited unlike Kafka.
///
/// See also <https://cloud.google.com/pubsub/docs/subscribe-best-practices#process-messages>
struct WaitCheckpointWorker<S: StateStore> {
    wait_checkpoint_rx: UnboundedReceiver<(Epoch, WaitCheckpointTask)>,
    state_store: S,
}

impl<S: StateStore> WaitCheckpointWorker<S> {
    pub async fn run(mut self) {
        tracing::debug!("wait epoch worker start success");
        loop {
            // poll the rx and wait for the epoch commit
            match self.wait_checkpoint_rx.recv().await {
                Some((epoch, task)) => {
                    tracing::debug!("start to wait epoch {}", epoch.0);
                    let ret = self
                        .state_store
                        .try_wait_epoch(HummockReadEpoch::Committed(epoch.0))
                        .await;

                    match ret {
                        Ok(()) => {
                            tracing::debug!(epoch = epoch.0, "wait epoch success");
                            task.run().await;
                        }
                        Err(e) => {
                            tracing::error!(
                            error = %e.as_report(),
                            "wait epoch {} failed", epoch.0
                            );
                        }
                    }
                }
                None => {
                    tracing::error!("wait epoch rx closed");
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use maplit::{btreemap, convert_args, hashmap};
    use risingwave_common::catalog::{ColumnId, Field, TableId};
    use risingwave_common::system_param::local_manager::LocalSystemParamsManager;
    use risingwave_common::test_prelude::StreamChunkTestExt;
    use risingwave_common::util::epoch::test_epoch;
    use risingwave_connector::source::datagen::DatagenSplit;
    use risingwave_connector::source::reader::desc::test_utils::create_source_desc_builder;
    use risingwave_pb::catalog::StreamSourceInfo;
    use risingwave_pb::plan_common::PbRowFormatType;
    use risingwave_storage::memory::MemoryStateStore;
    use tokio::sync::mpsc::unbounded_channel;
    use tracing_test::traced_test;

    use super::*;
    use crate::executor::source::{default_source_internal_table, SourceStateTableHandler};

    const MOCK_SOURCE_NAME: &str = "mock_source";

    #[tokio::test]
    async fn test_source_executor() {
        let table_id = TableId::default();
        let schema = Schema {
            fields: vec![Field::with_name(DataType::Int32, "sequence_int")],
        };
        let row_id_index = None;
        let source_info = StreamSourceInfo {
            row_format: PbRowFormatType::Native as i32,
            ..Default::default()
        };
        let (barrier_tx, barrier_rx) = unbounded_channel::<Barrier>();
        let column_ids = vec![0].into_iter().map(ColumnId::from).collect();

        // This datagen will generate 3 rows at one time.
        let properties = convert_args!(btreemap!(
            "connector" => "datagen",
            "datagen.rows.per.second" => "3",
            "fields.sequence_int.kind" => "sequence",
            "fields.sequence_int.start" => "11",
            "fields.sequence_int.end" => "11111",
        ));
        let source_desc_builder =
            create_source_desc_builder(&schema, row_id_index, source_info, properties, vec![]);
        let split_state_store = SourceStateTableHandler::from_table_catalog(
            &default_source_internal_table(0x2333),
            MemoryStateStore::new(),
        )
        .await;
        let core = StreamSourceCore::<MemoryStateStore> {
            source_id: table_id,
            column_ids,
            source_desc_builder: Some(source_desc_builder),
            latest_split_info: HashMap::new(),
            split_state_store,
            updated_splits_in_epoch: HashMap::new(),
            source_name: MOCK_SOURCE_NAME.to_string(),
        };

        let system_params_manager = LocalSystemParamsManager::for_test();

        let executor = SourceExecutor::new(
            ActorContext::for_test(0),
            Some(core),
            Arc::new(StreamingMetrics::unused()),
            barrier_rx,
            system_params_manager.get_params(),
            None,
            false,
        );
        let mut executor = executor.boxed().execute();

        let init_barrier =
            Barrier::new_test_barrier(test_epoch(1)).with_mutation(Mutation::Add(AddMutation {
                adds: HashMap::new(),
                added_actors: HashSet::new(),
                splits: hashmap! {
                    ActorId::default() => vec![
                        SplitImpl::Datagen(DatagenSplit {
                            split_index: 0,
                            split_num: 1,
                            start_offset: None,
                        }),
                    ],
                },
                pause: false,
                subscriptions_to_add: vec![],
            }));
        barrier_tx.send(init_barrier).unwrap();

        // Consume barrier.
        executor.next().await.unwrap().unwrap();

        // Consume data chunk.
        let msg = executor.next().await.unwrap().unwrap();

        // Row id will not be filled here.
        assert_eq!(
            msg.into_chunk().unwrap(),
            StreamChunk::from_pretty(
                " i
                + 11
                + 12
                + 13"
            )
        );
    }

    #[traced_test]
    #[tokio::test]
    async fn test_split_change_mutation() {
        let table_id = TableId::default();
        let schema = Schema {
            fields: vec![Field::with_name(DataType::Int32, "v1")],
        };
        let row_id_index = None;
        let source_info = StreamSourceInfo {
            row_format: PbRowFormatType::Native as i32,
            ..Default::default()
        };
        let properties = convert_args!(btreemap!(
            "connector" => "datagen",
            "fields.v1.kind" => "sequence",
            "fields.v1.start" => "11",
            "fields.v1.end" => "11111",
        ));

        let source_desc_builder =
            create_source_desc_builder(&schema, row_id_index, source_info, properties, vec![]);
        let mem_state_store = MemoryStateStore::new();

        let column_ids = vec![ColumnId::from(0)];
        let (barrier_tx, barrier_rx) = unbounded_channel::<Barrier>();
        let split_state_store = SourceStateTableHandler::from_table_catalog(
            &default_source_internal_table(0x2333),
            mem_state_store.clone(),
        )
        .await;

        let core = StreamSourceCore::<MemoryStateStore> {
            source_id: table_id,
            column_ids: column_ids.clone(),
            source_desc_builder: Some(source_desc_builder),
            latest_split_info: HashMap::new(),
            split_state_store,
            updated_splits_in_epoch: HashMap::new(),
            source_name: MOCK_SOURCE_NAME.to_string(),
        };

        let system_params_manager = LocalSystemParamsManager::for_test();

        let executor = SourceExecutor::new(
            ActorContext::for_test(0),
            Some(core),
            Arc::new(StreamingMetrics::unused()),
            barrier_rx,
            system_params_manager.get_params(),
            None,
            false,
        );
        let mut handler = executor.boxed().execute();

        let init_barrier =
            Barrier::new_test_barrier(test_epoch(1)).with_mutation(Mutation::Add(AddMutation {
                adds: HashMap::new(),
                added_actors: HashSet::new(),
                splits: hashmap! {
                    ActorId::default() => vec![
                        SplitImpl::Datagen(DatagenSplit {
                            split_index: 0,
                            split_num: 3,
                            start_offset: None,
                        }),
                    ],
                },
                pause: false,
                subscriptions_to_add: vec![],
            }));
        barrier_tx.send(init_barrier).unwrap();

        // Consume barrier.
        handler
            .next()
            .await
            .unwrap()
            .unwrap()
            .into_barrier()
            .unwrap();

        let mut ready_chunks = handler.ready_chunks(10);

        let _ = ready_chunks.next().await.unwrap();

        let new_assignment = vec![
            SplitImpl::Datagen(DatagenSplit {
                split_index: 0,
                split_num: 3,
                start_offset: None,
            }),
            SplitImpl::Datagen(DatagenSplit {
                split_index: 1,
                split_num: 3,
                start_offset: None,
            }),
            SplitImpl::Datagen(DatagenSplit {
                split_index: 2,
                split_num: 3,
                start_offset: None,
            }),
        ];

        let change_split_mutation = Barrier::new_test_barrier(test_epoch(2)).with_mutation(
            Mutation::SourceChangeSplit(hashmap! {
                ActorId::default() => new_assignment.clone()
            }),
        );

        barrier_tx.send(change_split_mutation).unwrap();

        let _ = ready_chunks.next().await.unwrap(); // barrier

        let mut source_state_handler = SourceStateTableHandler::from_table_catalog(
            &default_source_internal_table(0x2333),
            mem_state_store.clone(),
        )
        .await;
        // there must exist state for new add partition
        source_state_handler.init_epoch(EpochPair::new_test_epoch(test_epoch(2)));
        source_state_handler
            .get(new_assignment[1].id())
            .await
            .unwrap()
            .unwrap();

        tokio::time::sleep(Duration::from_millis(100)).await;

        let _ = ready_chunks.next().await.unwrap();

        let barrier = Barrier::new_test_barrier(test_epoch(3)).with_mutation(Mutation::Pause);
        barrier_tx.send(barrier).unwrap();

        let barrier = Barrier::new_test_barrier(test_epoch(4)).with_mutation(Mutation::Resume);
        barrier_tx.send(barrier).unwrap();

        // receive all
        ready_chunks.next().await.unwrap();

        let prev_assignment = new_assignment;
        let new_assignment = vec![prev_assignment[2].clone()];

        let drop_split_mutation = Barrier::new_test_barrier(test_epoch(5)).with_mutation(
            Mutation::SourceChangeSplit(hashmap! {
                ActorId::default() => new_assignment.clone()
            }),
        );

        barrier_tx.send(drop_split_mutation).unwrap();

        ready_chunks.next().await.unwrap(); // barrier

        let mut source_state_handler = SourceStateTableHandler::from_table_catalog(
            &default_source_internal_table(0x2333),
            mem_state_store.clone(),
        )
        .await;

        source_state_handler.init_epoch(EpochPair::new_test_epoch(5 * test_epoch(1)));

        assert!(source_state_handler
            .try_recover_from_state_store(&prev_assignment[0])
            .await
            .unwrap()
            .is_none());

        assert!(source_state_handler
            .try_recover_from_state_store(&prev_assignment[1])
            .await
            .unwrap()
            .is_none());

        assert!(source_state_handler
            .try_recover_from_state_store(&prev_assignment[2])
            .await
            .unwrap()
            .is_some());
    }
}
