// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! Clients whose implementation is partitioned across a set of subclients
//! (e.g. timely workers).

use std::collections::HashMap;
use std::fmt;
use std::iter;

use async_trait::async_trait;
use differential_dataflow::Hashable;
use futures::StreamExt;
use timely::progress::frontier::MutableAntichain;
use tokio_stream::StreamMap;
use tracing::debug;
use uuid::Uuid;

use mz_ore::cast::CastFrom;
use mz_repr::{Diff, GlobalId, Row};

use crate::client::{
    ComputeCommand, ComputeResponse, GenericClient, PeekResponse, StorageCommand, StorageResponse,
};
use crate::{DataflowDescription, TailResponse};

/// A client whose implementation is partitioned across a number of other
/// clients.
///
/// Such a client needs to broadcast (partitioned) commands to all of its
/// clients, and await responses from each of the client partitions before it
/// can respond.
#[derive(Debug)]
pub struct Partitioned<P, C, R>
where
    (C, R): Partitionable<C, R>,
{
    /// The individual partitions representing per-worker clients.
    pub parts: Vec<P>,
    /// The number of errors observed from underlying clients.
    seen_errors: usize,
    /// The partitioned state.
    state: <(C, R) as Partitionable<C, R>>::PartitionedState,
}

impl<P, C, R> Partitioned<P, C, R>
where
    (C, R): Partitionable<C, R>,
{
    /// Create a client partitioned across multiple client shards.
    pub fn new(parts: Vec<P>) -> Self {
        Self {
            state: <(C, R) as Partitionable<C, R>>::new(parts.len()),
            parts,
            seen_errors: 0,
        }
    }
}

#[async_trait]
impl<P, C, R> GenericClient<C, R> for Partitioned<P, C, R>
where
    P: GenericClient<C, R>,
    (C, R): Partitionable<C, R>,
    C: fmt::Debug + Send,
    R: fmt::Debug + Send,
{
    async fn send(&mut self, cmd: C) -> Result<(), anyhow::Error> {
        let cmd_parts = self.state.split_command(cmd);
        for (shard, cmd_part) in self.parts.iter_mut().zip(cmd_parts) {
            shard.send(cmd_part).await?;
        }
        Ok(())
    }

    async fn recv(&mut self) -> Result<Option<R>, anyhow::Error> {
        let parts = self.parts.len();
        let mut stream: StreamMap<_, _> = self
            .parts
            .iter_mut()
            .map(|shard| shard.as_stream())
            .enumerate()
            .collect();
        while let Some((index, response)) = stream.next().await {
            match response {
                Err(e) => {
                    // Only emit one out of every `parts` errors. (If one
                    // underlying client observes an error, we expect all of
                    // the other clients to observe the same error.)
                    self.seen_errors += 1;
                    if (self.seen_errors % parts) == 0 {
                        return Err(e);
                    }
                }
                Ok(response) => {
                    if let Some(response) = self.state.absorb_response(index, response) {
                        return response.map(Some);
                    }
                }
            }
        }
        // Indicate completion of the communication.
        Ok(None)
    }
}

/// A trait for command–response pairs that can be partitioned across multiple
/// workers via [`Partitioned`].
pub trait Partitionable<C, R> {
    /// The type which functions as the state machine for the partitioning.
    type PartitionedState: PartitionedState<C, R>;

    /// Construct a [`PartitionedState`] for the command–response pair.
    fn new(parts: usize) -> Self::PartitionedState;
}

/// A state machine for a partitioned client that partitions commands across and
/// amalgamates responses from multiple partitions.
pub trait PartitionedState<C, R>: fmt::Debug + Send {
    /// Splits a command into multiple partitions.
    fn split_command(&mut self, command: C) -> Vec<C>;

    /// Absorbs a response from a single partition.
    ///
    /// If responses from all partitions have been absorbed, returns an
    /// amalgamated response.
    fn absorb_response(&mut self, shard_id: usize, response: R)
        -> Option<Result<R, anyhow::Error>>;
}

/// Maintained state for partitioned storage clients.
///
/// This helper type unifies the responses of multiple partitioned
/// workers in order to present as a single worker.
#[derive(Debug)]
pub struct PartitionedStorageState<T> {
    /// Number of partitions the state machine represents.
    parts: usize,
    /// Upper frontiers for sources.
    uppers: HashMap<GlobalId, MutableAntichain<T>>,
}

impl<T> Partitionable<StorageCommand<T>, StorageResponse<T>>
    for (StorageCommand<T>, StorageResponse<T>)
where
    T: timely::progress::Timestamp,
{
    type PartitionedState = PartitionedStorageState<T>;

    fn new(parts: usize) -> PartitionedStorageState<T> {
        PartitionedStorageState {
            parts,
            uppers: HashMap::new(),
        }
    }
}

impl<T> PartitionedStorageState<T>
where
    T: timely::progress::Timestamp,
{
    fn observe_command(&mut self, command: &StorageCommand<T>) {
        match command {
            StorageCommand::CreateSources(sources) => {
                for source in sources {
                    let mut frontier = MutableAntichain::new();
                    frontier.update_iter(iter::once((T::minimum(), self.parts as i64)));
                    let previous = self.uppers.insert(source.id, frontier);
                    assert!(previous.is_none(), "Protocol error: starting frontier tracking for already present identifier {:?} due to command {:?}", source.id, command);
                }
            }
            _ => {
                // Other commands have no known impact on frontier tracking.
            }
        }
    }
}

impl<T> PartitionedState<StorageCommand<T>, StorageResponse<T>> for PartitionedStorageState<T>
where
    T: timely::progress::Timestamp,
{
    fn split_command(&mut self, command: StorageCommand<T>) -> Vec<StorageCommand<T>> {
        self.observe_command(&command);

        match command {
            StorageCommand::Append(appends) => {
                let mut appends_parts = vec![Vec::with_capacity(appends.len()); self.parts];
                for (id, updates, upper) in appends {
                    let mut updates_parts = vec![Vec::new(); self.parts];
                    for update in updates {
                        let part = usize::cast_from(update.row.hashed()) % self.parts;
                        updates_parts[part].push(update);
                    }
                    for (part, updates) in appends_parts.iter_mut().zip(updates_parts) {
                        part.push((id, updates, upper.clone()));
                    }
                }
                appends_parts
                    .into_iter()
                    .map(StorageCommand::Append)
                    .collect()
            }
            command => vec![command; self.parts],
        }
    }

    fn absorb_response(
        &mut self,
        _shard_id: usize,
        response: StorageResponse<T>,
    ) -> Option<Result<StorageResponse<T>, anyhow::Error>> {
        match response {
            // Avoid multiple retractions of minimum time, to present as updates from one worker.
            StorageResponse::TimestampBindings(mut feedback) => {
                for (id, changes) in feedback.changes.iter_mut() {
                    if let Some(frontier) = self.uppers.get_mut(id) {
                        let iter = frontier.update_iter(changes.drain());
                        changes.extend(iter);
                    } else {
                        changes.clear();
                    }
                }
                // The following block implements a `list.retain()` of non-empty change batches.
                // This is more verbose than `list.retain()` because that method cannot mutate
                // its argument, and `is_empty()` may need to do this (as it is lazily compacted).
                let mut cursor = 0;
                while let Some((_id, changes)) = feedback.changes.get_mut(cursor) {
                    if changes.is_empty() {
                        feedback.changes.swap_remove(cursor);
                    } else {
                        cursor += 1;
                    }
                }

                Some(Ok(StorageResponse::TimestampBindings(feedback)))
            }
            // TODO(guswynn): is this the correct implementation?
            StorageResponse::LinearizedTimestamps(feedback) => {
                Some(Ok(StorageResponse::LinearizedTimestamps(feedback)))
            }
        }
    }
}

/// Maintained state for partitioned compute clients.
///
/// This helper type unifies the responses of multiple partitioned
/// workers in order to present as a single worker.
#[derive(Debug)]
pub struct PartitionedComputeState<T> {
    /// Number of partitions the state machine represents.
    parts: usize,
    /// Upper frontiers for indexes and sinks.
    uppers: HashMap<GlobalId, MutableAntichain<T>>,
    /// Pending responses for a peek; returnable once all are available.
    peek_responses: HashMap<Uuid, HashMap<usize, PeekResponse>>,
    /// Tracks in-progress `TAIL`s, and the stashed rows we are holding
    /// back until their timestamps are complete.
    pending_tails: HashMap<GlobalId, Option<(MutableAntichain<T>, Vec<(T, Row, Diff)>)>>,
}

impl<T> Partitionable<ComputeCommand<T>, ComputeResponse<T>>
    for (ComputeCommand<T>, ComputeResponse<T>)
where
    T: timely::progress::Timestamp + Copy,
{
    type PartitionedState = PartitionedComputeState<T>;

    fn new(parts: usize) -> PartitionedComputeState<T> {
        PartitionedComputeState {
            parts,
            uppers: HashMap::new(),
            peek_responses: HashMap::new(),
            pending_tails: HashMap::new(),
        }
    }
}

impl<T> PartitionedComputeState<T>
where
    T: timely::progress::Timestamp + Copy,
{
    fn reset(&mut self) {
        let PartitionedComputeState {
            parts: _,
            uppers,
            peek_responses,
            pending_tails,
        } = self;
        uppers.clear();
        peek_responses.clear();
        pending_tails.clear();
    }

    /// Observes commands that move past, and prepares state for responses.
    ///
    /// In particular, this method installs and removes upper frontier maintenance.
    pub fn observe_command(&mut self, command: &ComputeCommand<T>) {
        match command {
            ComputeCommand::CreateInstance(_) | ComputeCommand::DropInstance => {
                self.reset();
            }
            _ => (),
        }

        // Temporary storage for identifiers to add to and remove from frontier tracking.
        let mut start = Vec::new();
        let mut cease = Vec::new();
        command.frontier_tracking(&mut start, &mut cease);
        // Apply the determined effects of the command to `self.uppers`.
        for id in start.into_iter() {
            let mut frontier = timely::progress::frontier::MutableAntichain::new();
            frontier.update_iter(Some((T::minimum(), self.parts as i64)));
            let previous = self.uppers.insert(id, frontier);
            assert!(previous.is_none(), "Protocol error: starting frontier tracking for already present identifier {:?} due to command {:?}", id, command);
        }
        for id in cease.into_iter() {
            let previous = self.uppers.remove(&id);
            if previous.is_none() {
                debug!("Protocol error: ceasing frontier tracking for absent identifier {:?} due to command {:?}", id, command);
            }
        }
    }
}

impl<T> PartitionedState<ComputeCommand<T>, ComputeResponse<T>> for PartitionedComputeState<T>
where
    T: timely::progress::Timestamp + Copy,
{
    fn split_command(&mut self, command: ComputeCommand<T>) -> Vec<ComputeCommand<T>> {
        self.observe_command(&command);

        match command {
            ComputeCommand::CreateDataflows(dataflows) => {
                let mut dataflows_parts = vec![Vec::new(); self.parts];

                for dataflow in dataflows {
                    // A list of descriptions of objects for each part to build.
                    let mut builds_parts = vec![Vec::new(); self.parts];
                    // Partition each build description among `parts`.
                    for build_desc in dataflow.objects_to_build {
                        let build_part = build_desc.plan.partition_among(self.parts);
                        for (plan, objects_to_build) in
                            build_part.into_iter().zip(builds_parts.iter_mut())
                        {
                            objects_to_build.push(crate::BuildDesc {
                                id: build_desc.id,
                                plan,
                            });
                        }
                    }
                    // Each list of build descriptions results in a dataflow description.
                    for (dataflows_part, objects_to_build) in
                        dataflows_parts.iter_mut().zip(builds_parts)
                    {
                        dataflows_part.push(DataflowDescription {
                            source_imports: dataflow.source_imports.clone(),
                            index_imports: dataflow.index_imports.clone(),
                            objects_to_build,
                            index_exports: dataflow.index_exports.clone(),
                            sink_exports: dataflow.sink_exports.clone(),
                            as_of: dataflow.as_of.clone(),
                            debug_name: dataflow.debug_name.clone(),
                            id: dataflow.id,
                        });
                    }
                }
                dataflows_parts
                    .into_iter()
                    .map(ComputeCommand::CreateDataflows)
                    .collect()
            }
            command => vec![command; self.parts],
        }
    }

    fn absorb_response(
        &mut self,
        shard_id: usize,
        message: ComputeResponse<T>,
    ) -> Option<Result<ComputeResponse<T>, anyhow::Error>> {
        match message {
            ComputeResponse::FrontierUppers(mut list) => {
                for (id, changes) in list.iter_mut() {
                    if let Some(frontier) = self.uppers.get_mut(id) {
                        let iter = frontier.update_iter(changes.drain());
                        changes.extend(iter);
                    } else {
                        changes.clear();
                    }
                }

                // The following block implements a `list.retain()` of non-empty change batches.
                // This is more verbose than `list.retain()` because that method cannot mutate
                // its argument, and `is_empty()` may need to do this (as it is lazily compacted).
                let mut cursor = 0;
                while let Some((_id, changes)) = list.get_mut(cursor) {
                    if changes.is_empty() {
                        list.swap_remove(cursor);
                    } else {
                        cursor += 1;
                    }
                }

                if list.is_empty() {
                    None
                } else {
                    Some(Ok(ComputeResponse::FrontierUppers(list)))
                }
            }
            ComputeResponse::PeekResponse(uuid, response) => {
                // Incorporate new peek responses; awaiting all responses.
                let entry = self
                    .peek_responses
                    .entry(uuid)
                    .or_insert_with(Default::default);
                let novel = entry.insert(shard_id, response);
                assert!(novel.is_none(), "Duplicate peek response");
                // We may be ready to respond.
                if entry.len() == self.parts {
                    let mut response = PeekResponse::Rows(Vec::new());
                    for (_part, r) in std::mem::take(entry).into_iter() {
                        response = match (response, r) {
                            (_, PeekResponse::Canceled) => PeekResponse::Canceled,
                            (PeekResponse::Canceled, _) => PeekResponse::Canceled,
                            (_, PeekResponse::Error(e)) => PeekResponse::Error(e),
                            (PeekResponse::Error(e), _) => PeekResponse::Error(e),
                            (PeekResponse::Rows(mut rows), PeekResponse::Rows(r)) => {
                                rows.extend(r.into_iter());
                                PeekResponse::Rows(rows)
                            }
                        };
                    }
                    self.peek_responses.remove(&uuid);
                    Some(Ok(ComputeResponse::PeekResponse(uuid, response)))
                } else {
                    None
                }
            }
            ComputeResponse::TailResponse(id, response) => {
                let maybe_entry = self.pending_tails.entry(id).or_insert_with(|| {
                    let mut frontier = MutableAntichain::new();
                    frontier.update_iter(std::iter::once((T::minimum(), self.parts as i64)));
                    Some((frontier, Vec::new()))
                });

                let entry = match maybe_entry {
                    None => {
                        // This tail has been dropped;
                        // we should permanently block
                        // any messages from it
                        return None;
                    }
                    Some(entry) => entry,
                };

                use crate::TailBatch;
                use differential_dataflow::consolidation::consolidate_updates;
                match response {
                    TailResponse::Batch(TailBatch {
                        lower,
                        upper,
                        mut updates,
                    }) => {
                        let old_frontier = entry.0.frontier().to_owned();
                        entry.0.update_iter(lower.iter().map(|t| (t.clone(), -1)));
                        entry.0.update_iter(upper.iter().map(|t| (t.clone(), 1)));
                        entry.1.append(&mut updates);
                        let new_frontier = entry.0.frontier().to_owned();
                        if old_frontier != new_frontier {
                            consolidate_updates(&mut entry.1);
                            let mut ship = Vec::new();
                            let mut keep = Vec::new();
                            for (time, data, diff) in entry.1.drain(..) {
                                if new_frontier.less_equal(&time) {
                                    keep.push((time, data, diff));
                                } else {
                                    ship.push((time, data, diff));
                                }
                            }
                            entry.1 = keep;
                            Some(Ok(ComputeResponse::TailResponse(
                                id,
                                TailResponse::Batch(TailBatch {
                                    lower: old_frontier,
                                    upper: new_frontier,
                                    updates: ship,
                                }),
                            )))
                        } else {
                            None
                        }
                    }
                    TailResponse::DroppedAt(frontier) => {
                        *maybe_entry = None;
                        Some(Ok(ComputeResponse::TailResponse(
                            id,
                            TailResponse::DroppedAt(frontier),
                        )))
                    }
                }
            }
        }
    }
}
