use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use futures::future::try_join_all;
use itertools::Itertools as _;
use rand::Rng;
use segment::data_types::order_by::{Direction, OrderBy, OrderValue};
use segment::types::{
    ExtendedPointId, Filter, ScoredPoint, WithPayload, WithPayloadInterface, WithVector,
};
use tokio::runtime::Handle;

use super::LocalShard;
use crate::collection_manager::holders::segment_holder::LockedSegment;
use crate::collection_manager::segments_searcher::SegmentsSearcher;
use crate::operations::types::{
    CollectionError, CollectionResult, QueryScrollRequestInternal, Record, ScrollOrder,
};

impl LocalShard {
    /// Basic parallel batching, it is conveniently used for the universal query API.
    pub(super) async fn query_scroll_batch(
        &self,
        batch: Arc<Vec<QueryScrollRequestInternal>>,
        search_runtime_handle: &Handle,
        timeout: Duration,
    ) -> CollectionResult<Vec<Vec<ScoredPoint>>> {
        let scrolls = batch
            .iter()
            .map(|request| self.query_scroll(request, search_runtime_handle));

        // execute all the scrolls concurrently
        let all_scroll_results = try_join_all(scrolls);
        tokio::time::timeout(timeout, all_scroll_results)
            .await
            .map_err(|_| {
                log::debug!(
                    "Query scroll timeout reached: {} seconds",
                    timeout.as_secs()
                );
                CollectionError::timeout(timeout.as_secs() as usize, "Query scroll")
            })?
    }

    /// Scroll a single page, to be used for the universal query API only.
    async fn query_scroll(
        &self,
        request: &QueryScrollRequestInternal,
        search_runtime_handle: &Handle,
    ) -> CollectionResult<Vec<ScoredPoint>> {
        let QueryScrollRequestInternal {
            limit,
            with_vector,
            filter,
            scroll_order,
            with_payload,
        } = request;

        let limit = *limit;

        let offset_id = None;

        let point_results = match scroll_order {
            ScrollOrder::ById => self
                .scroll_by_id(
                    offset_id,
                    limit,
                    with_payload,
                    with_vector,
                    filter.as_ref(),
                    search_runtime_handle,
                )
                .await?
                .into_iter()
                .map(|record| ScoredPoint {
                    id: record.id,
                    version: 0,
                    score: 0.0,
                    payload: record.payload,
                    vector: record.vector,
                    shard_key: record.shard_key,
                    order_value: None,
                })
                .collect(),
            ScrollOrder::ByField(order_by) => {
                let (records, values) = self
                    .scroll_by_field(
                        limit,
                        with_payload,
                        with_vector,
                        filter.as_ref(),
                        search_runtime_handle,
                        order_by,
                    )
                    .await?;

                records
                    .into_iter()
                    .zip(values)
                    .map(|(record, value)| ScoredPoint {
                        id: record.id,
                        version: 0,
                        score: 0.0,
                        payload: record.payload,
                        vector: record.vector,
                        shard_key: record.shard_key,
                        order_value: Some(value),
                    })
                    .collect()
            }
            ScrollOrder::Random => {
                let records = self
                    .scroll_randomly(
                        limit,
                        with_payload,
                        with_vector,
                        filter.as_ref(),
                        search_runtime_handle,
                    )
                    .await?;

                records
                    .into_iter()
                    .map(|record| ScoredPoint {
                        id: record.id,
                        version: 0,
                        score: 0.0,
                        payload: record.payload,
                        vector: record.vector,
                        shard_key: record.shard_key,
                        order_value: None,
                    })
                    .collect()
            }
        };

        Ok(point_results)
    }

    pub async fn scroll_by_id(
        &self,
        offset: Option<ExtendedPointId>,
        limit: usize,
        with_payload_interface: &WithPayloadInterface,
        with_vector: &WithVector,
        filter: Option<&Filter>,
        search_runtime_handle: &Handle,
    ) -> CollectionResult<Vec<Record>> {
        let segments = self.segments();

        let (non_appendable, appendable) = segments.read().split_segments();

        let read_filtered = |segment: LockedSegment| {
            let filter = filter.cloned();

            search_runtime_handle.spawn_blocking(move || {
                segment
                    .get()
                    .read()
                    .read_filtered(offset, Some(limit), filter.as_ref())
            })
        };

        let non_appendable = try_join_all(non_appendable.into_iter().map(read_filtered)).await?;
        let appendable = try_join_all(appendable.into_iter().map(read_filtered)).await?;

        let point_ids = non_appendable
            .into_iter()
            .chain(appendable)
            .flatten()
            .sorted()
            .dedup()
            .take(limit)
            .collect_vec();

        let with_payload = WithPayload::from(with_payload_interface);
        let records_map =
            SegmentsSearcher::retrieve(segments, &point_ids, &with_payload, with_vector)?;

        let ordered_records = point_ids
            .iter()
            .filter_map(|point| records_map.get(point).cloned())
            .collect();

        Ok(ordered_records)
    }

    pub async fn scroll_by_field(
        &self,
        limit: usize,
        with_payload_interface: &WithPayloadInterface,
        with_vector: &WithVector,
        filter: Option<&Filter>,
        search_runtime_handle: &Handle,
        order_by: &OrderBy,
    ) -> CollectionResult<(Vec<Record>, Vec<OrderValue>)> {
        let segments = self.segments();

        let (non_appendable, appendable) = segments.read().split_segments();

        let read_ordered_filtered = |segment: LockedSegment| {
            let filter = filter.cloned();
            let order_by = order_by.clone();

            search_runtime_handle.spawn_blocking(move || {
                segment
                    .get()
                    .read()
                    .read_ordered_filtered(Some(limit), filter.as_ref(), &order_by)
            })
        };

        let non_appendable =
            try_join_all(non_appendable.into_iter().map(read_ordered_filtered)).await?;
        let appendable = try_join_all(appendable.into_iter().map(read_ordered_filtered)).await?;

        let all_reads = non_appendable
            .into_iter()
            .chain(appendable)
            .collect::<Result<Vec<_>, _>>()?;

        let (values, point_ids): (Vec<_>, Vec<_>) = all_reads
            .into_iter()
            .kmerge_by(|a, b| match order_by.direction() {
                Direction::Asc => a <= b,
                Direction::Desc => a >= b,
            })
            .dedup()
            .take(limit)
            .unzip();

        let with_payload = WithPayload::from(with_payload_interface);

        // Fetch with the requested vector and payload
        let records_map =
            SegmentsSearcher::retrieve(segments, &point_ids, &with_payload, with_vector)?;

        let ordered_records = point_ids
            .iter()
            .filter_map(|point| records_map.get(point).cloned())
            .collect();

        Ok((ordered_records, values))
    }

    async fn scroll_randomly(
        &self,
        limit: usize,
        with_payload_interface: &WithPayloadInterface,
        with_vector: &WithVector,
        filter: Option<&Filter>,
        search_runtime_handle: &Handle,
    ) -> CollectionResult<Vec<Record>> {
        let segments = self.segments();

        let (non_appendable, appendable) = segments.read().split_segments();

        let read_filtered = |segment: LockedSegment| {
            let filter = filter.cloned();

            search_runtime_handle.spawn_blocking(move || {
                let get_segment = segment.get();
                let read_segment = get_segment.read();

                (
                    read_segment.available_point_count(),
                    read_segment.read_random_filtered(limit, filter.as_ref()),
                )
            })
        };

        let all_reads = try_join_all(
            non_appendable
                .into_iter()
                .chain(appendable)
                .map(read_filtered),
        )
        .await?;

        let (availability, mut segments_reads): (Vec<_>, Vec<_>) = all_reads.into_iter().unzip();

        let total_available: usize = availability.iter().sum();
        let segment_weights = availability
            .into_iter()
            .map(|available| available as f64 / total_available as f64)
            .collect_vec();

        // Get `limit` points in a weighted fashion from each segment, depending on how many points each segment has.
        let mut rng = rand::thread_rng();
        let mut random_points = Vec::with_capacity(limit);
        let mut exhausted_reads = HashSet::new();
        for segment_id in (0..segments_reads.len()).cycle() {
            if exhausted_reads.contains(&segment_id) {
                continue;
            }

            // Determine weight for this segment
            let weight = segment_weights.get(segment_id).unwrap_or(&0.5);

            if !rng.gen_bool(*weight) {
                continue;
            }

            let Some(point) = segments_reads
                .get_mut(segment_id)
                .and_then(|points| points.pop())
            else {
                // reads have been exhausted, don't use this segment anymore
                exhausted_reads.insert(segment_id);
                if exhausted_reads.len() == segments_reads.len() {
                    // no points left in any read
                    break;
                }
                continue;
            };

            // Add point to result
            random_points.push(point);
            if random_points.len() == limit {
                break;
            }
        }

        let with_payload = WithPayload::from(with_payload_interface);
        let records_map =
            SegmentsSearcher::retrieve(segments, &random_points, &with_payload, with_vector)?;

        Ok(records_map.into_values().collect())
    }
}
