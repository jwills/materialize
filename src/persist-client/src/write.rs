// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! Write capabilities and handles

use std::borrow::Borrow;
use std::fmt::Debug;
use std::sync::Arc;
use std::time::Instant;

use differential_dataflow::difference::Semigroup;
use differential_dataflow::lattice::Lattice;
use differential_dataflow::trace::Description;
use mz_persist::indexed::columnar::ColumnarRecordsVecBuilder;
use mz_persist::indexed::encoding::BlobTraceBatchPart;
use mz_persist::location::{Atomicity, BlobMulti, ExternalError};
use mz_persist_types::{Codec, Codec64};
use timely::progress::{Antichain, Timestamp};
use timely::PartialOrder;
use tracing::trace;
use uuid::Uuid;

use crate::error::InvalidUsage;
use crate::r#impl::machine::{retry_external, Machine, FOREVER};
use crate::r#impl::state::Upper;

/// A "capability" granting the ability to apply updates to some shard at times
/// greater or equal to `self.upper()`.
///
/// All async methods on ReadHandle retry for as long as they are able, but the
/// returned [std::future::Future]s implement "cancel on drop" semantics. This
/// means that callers can add a timeout using [tokio::time::timeout] or
/// [tokio::time::timeout_at].
///
/// ```rust,no_run
/// # let mut write: mz_persist_client::write::WriteHandle<String, String, u64, i64> = unimplemented!();
/// # let timeout: std::time::Duration = unimplemented!();
/// # async {
/// tokio::time::timeout(timeout, write.fetch_recent_upper()).await
/// # };
/// ```
#[derive(Debug)]
pub struct WriteHandle<K, V, T, D>
where
    T: Timestamp + Lattice + Codec64,
{
    pub(crate) machine: Machine<K, V, T, D>,
    pub(crate) blob: Arc<dyn BlobMulti + Send + Sync>,

    pub(crate) upper: Antichain<T>,
}

impl<K, V, T, D> WriteHandle<K, V, T, D>
where
    K: Debug + Codec,
    V: Debug + Codec,
    T: Timestamp + Lattice + Codec64,
    D: Semigroup + Codec64,
{
    /// This handle's `upper` frontier.
    ///
    /// This will always be greater or equal to the shard-global `upper`.
    pub fn upper(&self) -> &Antichain<T> {
        &self.upper
    }

    /// Fetches and returns a recent shard-global `upper`. Importantly, this operation is not
    /// linearized with other write operations.
    ///
    /// This requires fetching the latest state from consensus and is therefore a potentially
    /// expensive operation.
    pub async fn fetch_recent_upper(&mut self) -> Antichain<T> {
        trace!("WriteHandle::fetch_recent_upper");
        self.machine.fetch_upper().await
    }

    /// Applies `updates` to this shard and downgrades this handle's upper to
    /// `new_upper`.
    ///
    /// The innermost `Result` is `Ok` if the updates were successfully written.
    /// If not, an `Upper` err containing the current writer upper is returned.
    /// If that happens, we also update our local `upper` to match the current
    /// upper. This is useful in cases where a timeout happens in between a
    /// successful write and returning that to the client.
    ///
    /// In contrast to [Self::compare_and_append], multiple [WriteHandle]s may
    /// be used concurrently to write to the same shard, but in this case, the
    /// data being written must be identical (in the sense of "definite"-ness).
    /// It's intended for replicated use by source ingestion, sinks, etc.
    ///
    /// All times in `updates` must be greater or equal to `self.upper()` and
    /// not greater or equal to `new_upper`. A `new_upper` of the empty
    /// antichain "finishes" this shard, promising that no more data is ever
    /// incoming.
    ///
    /// `updates` may be empty, which allows for downgrading `upper` to
    /// communicate progress. It is possible to heartbeat a writer lease by
    /// calling this with `new_upper` equal to `self.upper()` and an empty
    /// `updates` (making the call a no-op).
    ///
    /// This uses a bounded amount of memory, even when `updates` is very large.
    /// Individual records, however, should be small enough that we can
    /// reasonably chunk them up: O(KB) is definitely fine, O(MB) come talk to
    /// us.
    ///
    /// The clunky multi-level Result is to enable more obvious error handling
    /// in the caller. See <http://sled.rs/errors.html> for details.
    ///
    /// TODO: Introduce an AsyncIterator (futures::Stream) variant of this. Or,
    /// given that the AsyncIterator version would be strictly more general,
    /// alter this one if it turns out that the compiler can optimize out the
    /// overhead.
    pub async fn append<SB, KB, VB, TB, DB, I>(
        &mut self,
        updates: I,
        lower: Antichain<T>,
        upper: Antichain<T>,
    ) -> Result<Result<Result<(), Upper<T>>, InvalidUsage<T>>, ExternalError>
    where
        SB: Borrow<((KB, VB), TB, DB)>,
        KB: Borrow<K>,
        VB: Borrow<V>,
        TB: Borrow<T>,
        DB: Borrow<D>,
        I: IntoIterator<Item = SB>,
    {
        trace!("WriteHandle::append lower={:?} upper={:?}", lower, upper);

        let upper = upper;
        let since = Antichain::from_elem(T::minimum());
        let mut desc = Description::new(lower.clone(), upper, since);

        // TODO: Instead construct a Vec of batches here so it can be bounded
        // memory usage (if updates is large).
        let value = match Self::encode_batch(&desc, updates) {
            Ok(x) => x,
            Err(err) => return Ok(Err(err)),
        };
        let keys = if let Some(value) = value {
            let key = Uuid::new_v4().to_string();
            let () = retry_external("append::set", || async {
                // If MultiBlob::set took value as a ref, then we wouldn't have
                // to clone here.
                self.blob
                    .set(
                        Instant::now() + FOREVER,
                        &key,
                        value.clone(),
                        Atomicity::RequireAtomic,
                    )
                    .await
            })
            .await;
            vec![key]
        } else {
            vec![]
        };

        loop {
            let res = self.machine.compare_and_append(&keys, &desc).await?;
            match res {
                Ok(Ok(_seqno)) => {
                    self.upper = desc.upper().clone();
                    return Ok(Ok(Ok(())));
                }
                // TODO(aljoscha): This seems useless now because we have to read from consensus to
                // get an up-to-date version of the upper.
                Ok(Err(_current_upper)) => {
                    // If the state machine thinks that the shard upper is not far enough along, it
                    // could be because the caller of this method has found out that it advanced
                    // via some some side-channel that didn't update our local cache of the machine
                    // state. So, fetch the latest state and try again if we indeed get something
                    // different.
                    self.machine.fetch_and_update_state().await;
                    let current_upper = self.machine.upper();

                    // We tried to to a non-contiguous append, that won't work.
                    if PartialOrder::less_than(&current_upper, &lower) {
                        self.upper = current_upper.clone();
                        return Ok(Ok(Err(Upper(current_upper))));
                    } else if PartialOrder::less_than(&current_upper, desc.upper()) {
                        // Cut down the Description by advancing its lower to the current shard
                        // upper and try again. IMPORTANT: We can only advance the lower, meaning
                        // we cut updates away, we must not "extend" the batch by changing to a
                        // lower that is not beyond the current lower. This invariant is checked by
                        // the first if branch: if `!(current_upper < lower)` then it holds that
                        // `lower <= current_upper`.
                        desc = Description::new(
                            current_upper,
                            desc.upper().clone(),
                            desc.since().clone(),
                        );
                    } else {
                        // We already have updates past this batch's upper, the append is a no-op.
                        self.upper = current_upper;
                        return Ok(Ok(Ok(())));
                    }
                }
                Err(err) => return Ok(Err(err)),
            }
        }
    }

    /// Applies `updates` to this shard and downgrades this handle's upper to
    /// `new_upper` iff the current global upper of this shard is
    /// `expected_upper`.
    ///
    /// The innermost `Result` is `Ok` if the updates were successfully written.
    /// If not, an `Upper` err containing the current global upper is returned.
    ///
    /// In contrast to [Self::append], this linearizes mutations from all
    /// writers. It's intended for use as an atomic primitive for timestamp
    /// bindings, SQL tables, etc.
    ///
    /// All times in `updates` must be greater or equal to `expected_upper` and
    /// not greater or equal to `new_upper`. A `new_upper` of the empty
    /// antichain "finishes" this shard, promising that no more data is ever
    /// incoming.
    ///
    /// `updates` may be empty, which allows for downgrading `upper` to
    /// communicate progress. It is possible to heartbeat a writer lease by
    /// calling this with `new_upper` equal to `self.upper()` and an empty
    /// `updates` (making the call a no-op).
    ///
    /// This uses a bounded amount of memory, even when `updates` is very large.
    /// Individual records, however, should be small enough that we can
    /// reasonably chunk them up: O(KB) is definitely fine, O(MB) come talk to
    /// us.
    ///
    /// The clunky multi-level Result is to enable more obvious error handling
    /// in the caller. See <http://sled.rs/errors.html> for details.
    ///
    /// SUBTLE! Unlike the other methods on WriteHandle, it is not always safe
    /// to retry [ExternalError]s in compare_and_append (depends on the usage
    /// pattern). We should be able to structure timestamp binding, source, and
    /// sink code so it is always safe to retry [ExternalError]s, but SQL txns
    /// will have to pass the error back to the user (or risk double committing
    /// the txn).
    ///
    /// TODO: This already retries [mz_persist::location::Determinate] errors,
    /// so the signature could be changed to only return Indeterminate, but
    /// leaving it as ExternalError for now to save churn on storage PR rebases.
    pub async fn compare_and_append<SB, KB, VB, TB, DB, I>(
        &mut self,
        updates: I,
        expected_upper: Antichain<T>,
        new_upper: Antichain<T>,
    ) -> Result<Result<Result<(), Upper<T>>, InvalidUsage<T>>, ExternalError>
    where
        SB: Borrow<((KB, VB), TB, DB)>,
        KB: Borrow<K>,
        VB: Borrow<V>,
        TB: Borrow<T>,
        DB: Borrow<D>,
        I: IntoIterator<Item = SB>,
    {
        trace!(
            "WriteHandle::compare_and_append expected_upper={:?} new_upper={:?}",
            expected_upper,
            new_upper
        );

        let lower = expected_upper.clone();
        let upper = new_upper;
        let since = Antichain::from_elem(T::minimum());
        let desc = Description::new(lower, upper, since);

        // TODO: Instead construct a Vec of batches here so it can be bounded
        // memory usage (if updates is large).
        let value = match Self::encode_batch(&desc, updates) {
            Ok(x) => x,
            Err(err) => return Ok(Err(err)),
        };
        let keys = if let Some(value) = value {
            let key = Uuid::new_v4().to_string();
            let () = retry_external("compare_and_append::set", || async {
                // If MultiBlob::set took value as a ref, then we wouldn't have
                // to clone here.
                self.blob
                    .set(
                        Instant::now() + FOREVER,
                        &key,
                        value.clone(),
                        Atomicity::RequireAtomic,
                    )
                    .await
            })
            .await;
            vec![key]
        } else {
            vec![]
        };

        loop {
            let res = self.machine.compare_and_append(&keys, &desc).await?;
            match res {
                Ok(Ok(_seqno)) => {
                    self.upper = desc.upper().clone();
                    return Ok(Ok(Ok(())));
                }
                // TODO(aljoscha): This seems useless now because we have to read from consensus to
                // get an up-to-date version of the upper.
                Ok(Err(_current_upper)) => {
                    // If the state machine thinks that the shard upper is not far enough along, it
                    // could be because the caller of this method has found out that it advanced
                    // via some some side-channel that didn't update our local cache of the machine
                    // state. So, fetch the latest state and try again if we indeed get something
                    // different.
                    self.machine.fetch_and_update_state().await;
                    let current_upper = self.machine.upper();

                    // We tried to to a compare_and_append with the wrong expected upper, that
                    // won't work.
                    if current_upper != expected_upper {
                        self.upper = current_upper.clone();
                        return Ok(Ok(Err(Upper(current_upper))));
                    } else {
                        // The upper stored in state was outdated. Retry after updating.
                    }
                }
                Err(err) => return Ok(Err(err)),
            }
        }
    }

    fn encode_batch<SB, KB, VB, TB, DB, I>(
        desc: &Description<T>,
        updates: I,
    ) -> Result<Option<Vec<u8>>, InvalidUsage<T>>
    where
        SB: Borrow<((KB, VB), TB, DB)>,
        KB: Borrow<K>,
        VB: Borrow<V>,
        TB: Borrow<T>,
        DB: Borrow<D>,
        I: IntoIterator<Item = SB>,
    {
        let iter = updates.into_iter();
        let size_hint = iter.size_hint();

        let (mut key_buf, mut val_buf) = (Vec::new(), Vec::new());
        let mut builder = ColumnarRecordsVecBuilder::default();
        for tuple in iter {
            let ((k, v), t, d) = tuple.borrow();
            let (k, v, t, d) = (k.borrow(), v.borrow(), t.borrow(), d.borrow());
            if !desc.lower().less_equal(t) || desc.upper().less_equal(t) {
                return Err(InvalidUsage::UpdateNotWithinBounds {
                    ts: t.clone(),
                    lower: desc.lower().clone(),
                    upper: desc.upper().clone(),
                });
            }

            trace!("writing update {:?}", ((k, v), t, d));
            key_buf.clear();
            val_buf.clear();
            K::encode(k, &mut key_buf);
            V::encode(v, &mut val_buf);
            // TODO: Get rid of the from_le_bytes.
            let t = u64::from_le_bytes(T::encode(t));
            let d = i64::from_le_bytes(D::encode(d));

            if builder.len() == 0 {
                // Use the first record to attempt to pre-size the builder
                // allocations. This uses the iter's size_hint's lower+1 to
                // match the logic in Vec.
                let (lower, _) = size_hint;
                let additional = usize::saturating_add(lower, 1);
                builder.reserve(additional, key_buf.len(), val_buf.len());
            }
            builder.push(((&key_buf, &val_buf), t, d))
        }

        // TODO: Get rid of the from_le_bytes.
        let desc = Description::new(
            Antichain::from(
                desc.lower()
                    .elements()
                    .iter()
                    .map(|x| u64::from_le_bytes(T::encode(x)))
                    .collect::<Vec<_>>(),
            ),
            Antichain::from(
                desc.upper()
                    .elements()
                    .iter()
                    .map(|x| u64::from_le_bytes(T::encode(x)))
                    .collect::<Vec<_>>(),
            ),
            Antichain::from(
                desc.since()
                    .elements()
                    .iter()
                    .map(|x| u64::from_le_bytes(T::encode(x)))
                    .collect::<Vec<_>>(),
            ),
        );

        let batch = BlobTraceBatchPart {
            desc,
            updates: builder.finish(),
            index: 0,
        };
        if batch.updates.len() == 0 {
            return Ok(None);
        }

        let mut buf = Vec::new();
        batch.encode(&mut buf);
        Ok(Some(buf))
    }

    /// Test helper for an [Self::append] call that is expected to succeed.
    #[cfg(test)]
    #[track_caller]
    pub async fn expect_append<L, U>(&mut self, updates: &[((K, V), T, D)], lower: L, new_upper: U)
    where
        L: Into<Antichain<T>>,
        U: Into<Antichain<T>>,
    {
        self.append(
            updates.iter().map(|((k, v), t, d)| ((k, v), t, d)),
            lower.into(),
            new_upper.into(),
        )
        .await
        .expect("external durability failed")
        .expect("invalid usage")
        .expect("unexpected upper");
    }

    /// Test helper for a [Self::compare_and_append] call that is expected to
    /// succeed.
    #[cfg(test)]
    #[track_caller]
    pub async fn expect_compare_and_append(
        &mut self,
        updates: &[((K, V), T, D)],
        expected_upper: T,
        new_upper: T,
    ) {
        self.compare_and_append(
            updates.iter().map(|((k, v), t, d)| ((k, v), t, d)),
            Antichain::from_elem(expected_upper),
            Antichain::from_elem(new_upper),
        )
        .await
        .expect("external durability failed")
        .expect("invalid usage")
        .expect("unexpected upper")
    }
}

#[cfg(test)]
mod tests {
    use crate::tests::new_test_client;
    use crate::ShardId;

    use super::*;

    #[tokio::test]
    async fn empty_batches() {
        mz_ore::test::init_logging();

        let data = vec![
            (("1".to_owned(), "one".to_owned()), 1, 1),
            (("2".to_owned(), "two".to_owned()), 2, 1),
            (("3".to_owned(), "three".to_owned()), 3, 1),
        ];

        let (mut write, _) = new_test_client()
            .await
            .expect_open::<String, String, u64, i64>(ShardId::new())
            .await;
        let blob = Arc::clone(&write.blob);

        // Write an initial batch.
        let mut upper = 3;
        write.expect_append(&data[..2], vec![0], vec![upper]).await;

        // Write a bunch of empty batches. This shouldn't write blobs, so the count should stay the same.
        let blob_count_before = blob
            .list_keys(Instant::now() + FOREVER)
            .await
            .expect("list_keys failed")
            .len();
        for _ in 0..5 {
            let new_upper = upper + 1;
            write.expect_compare_and_append(&[], upper, new_upper).await;
            upper = new_upper;
        }
        assert_eq!(
            blob.list_keys(Instant::now() + FOREVER)
                .await
                .expect("list_keys failed")
                .len(),
            blob_count_before
        );
    }
}
