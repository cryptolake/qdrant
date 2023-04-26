use std::collections::{HashMap, HashSet};
use std::hash::Hash;

use segment::types::{ExtendedPointId, ScoredPoint};
use serde_json::Value;

#[derive(Debug, Eq, PartialEq)]
pub(super) struct GroupKey(pub serde_json::Value);

impl Hash for GroupKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        match &self.0 {
            Value::Number(n) => n.hash(state),
            Value::String(s) => s.hash(state),
            _ => unreachable!("GroupKey should only be a number or a string"),
        }
    }
}

impl From<&str> for GroupKey {
    fn from(s: &str) -> Self {
        Self(serde_json::Value::String(s.to_string()))
    }
}

type Hits = HashSet<ScoredPoint>;

#[allow(dead_code)] // temporary
pub(super) struct GroupsAggregator {
    groups: HashMap<GroupKey, Hits>,
    max_group_size: usize,
    grouped_by: String,
    max_groups: usize,
}

#[allow(dead_code)] // temporary
impl GroupsAggregator {
    pub(super) fn new(groups: usize, group_size: usize, grouped_by: String) -> Self {
        Self {
            groups: HashMap::with_capacity(groups),
            max_group_size: group_size,
            grouped_by,
            max_groups: groups,
        }
    }

    /// Adds a point to the group that corresponds based on the group_by field, assumes that the point has the group_by field
    pub(super) fn add_point(&mut self, point: &ScoredPoint) {
        // if the key contains multiple values, grabs the first one
        let group_key = point
            .payload
            .as_ref()
            .and_then(|p| p.get_value(&self.grouped_by).values().first().cloned());

        // ignore if no such payload
        let group_key = match group_key {
            Some(group_key) => group_key,
            None => return,
        };

        // ignore arrays, objects and null values
        let group_key = match group_key {
            serde_json::Value::String(_) | serde_json::Value::Number(_) => {
                GroupKey(group_key.clone())
            }
            _ => return,
        };

        if !self.groups.contains_key(&group_key) && self.groups.len() >= self.max_groups {
            return;
        }

        let group = self
            .groups
            .entry(group_key)
            .or_insert_with(|| HashSet::with_capacity(self.max_group_size));

        if group.len() == self.max_group_size {
            return;
        }

        group.insert(point.clone());
    }

    /// Adds multiple points to the group that they corresponds based on the group_by field, assumes that the points always have the grouped_by field
    pub(super) fn add_points(&mut self, points: &[ScoredPoint]) {
        points.iter().for_each(|point| self.add_point(point));
    }

    pub(super) fn len(&self) -> usize {
        self.groups.len()
    }

    // gets the keys of the groups that have less than the max group size
    pub(super) fn keys_of_unfilled_groups(&self) -> Vec<Value> {
        self.groups
            .iter()
            .filter(|(_, hits)| hits.len() < self.max_group_size)
            .map(|(key, _)| key.0.clone())
            .collect()
    }

    // gets the keys of the groups that have reached or exceeded the max group size
    pub(super) fn keys_of_filled_groups(&self) -> Vec<Value> {
        self.groups
            .iter()
            .filter(|(_, hits)| hits.len() >= self.max_group_size)
            .map(|(key, _)| key.0.clone())
            .collect()
    }

    /// Gets the ids of the already present points across the groups
    pub(super) fn ids(&self) -> HashSet<ExtendedPointId> {
        self.groups
            .iter()
            .flat_map(|(_, hits)| hits.iter())
            .map(|p| p.id)
            .collect()
    }

    pub(super) fn groups(&self) -> &HashMap<GroupKey, Hits> {
        &self.groups
    }

    pub(super) fn flatten(&self) -> Vec<ScoredPoint> {
        self.groups.values().flatten().cloned().collect()
    }

    /// Copies the payload and vector from the provided points to the points inside of each of the groups
    pub(super) fn hydrate_from(&mut self, points: &[ScoredPoint]) {
        for point in points {
            self.groups.iter_mut().for_each(|(_, ps)| {
                if ps.contains(point) {
                    ps.replace(point.clone())
                        .expect("The point should be in the group before replacing it! 😱");
                }
            });
        }
    }
}

#[cfg(test)]
mod unit_tests {

    use segment::types::Payload;

    use super::*;

    #[test]
    fn it_adds_single_points() {
        let mut aggregator = GroupsAggregator::new(3, 2, "docId".to_string());

        // cases
        [
            (
                // point
                &ScoredPoint {
                    id: 1.into(),
                    version: 0,
                    score: 1.0,
                    payload: Some(Payload::from(serde_json::json!({"docId": "a"}))),
                    vector: None,
                },
                // key
                "a",
                // group size
                1,
                // groups count
                1,
            ),
            (
                &ScoredPoint {
                    id: 1.into(), // same id as the previous one
                    version: 0,
                    score: 1.0,
                    payload: Some(Payload::from(serde_json::json!({"docId": "a"}))),
                    vector: None,
                },
                "a",
                1, // should not add it because it already has a point with the same id
                1,
            ),
            (
                &ScoredPoint {
                    id: 2.into(),
                    version: 0,
                    score: 1.0,
                    payload: Some(Payload::from(serde_json::json!({"docId": "a"}))),
                    vector: None,
                },
                "a",
                2,
                1, // add it to same group
            ),
            (
                &ScoredPoint {
                    id: 3.into(),
                    version: 0,
                    score: 1.0,
                    payload: Some(Payload::from(serde_json::json!({"docId": "a"}))),
                    vector: None,
                },
                "a",
                2, // group already full
                1,
            ),
            (
                &ScoredPoint {
                    id: 4.into(),
                    version: 0,
                    score: 1.0,
                    payload: Some(Payload::from(serde_json::json!({"docId": "b"}))),
                    vector: None,
                },
                "b",
                1,
                2,
            ),
            (
                &ScoredPoint {
                    id: 5.into(),
                    version: 0,
                    score: 1.0,
                    payload: Some(Payload::from(serde_json::json!({"docId": "c"}))),
                    vector: None,
                },
                "c",
                1,
                3,
            ),
            (
                &ScoredPoint {
                    id: 6.into(),
                    version: 0,
                    score: 1.0,
                    payload: Some(Payload::from(serde_json::json!({"docId": "d"}))),
                    vector: None,
                },
                "d",
                0, // already enough groups
                3,
            ),
        ]
        .into_iter()
        .enumerate()
        .for_each(|(_case_num, (point, key, size, groups))| {
            aggregator.add_point(point);

            assert_eq!(aggregator.len(), groups);

            let key = &GroupKey::from(key);
            if size > 0 {
                assert_eq!(aggregator.groups.get(key).unwrap().len(), size);
            } else {
                assert!(aggregator.groups.get(key).is_none());
            }
        });
    }

    #[test]
    fn hydrate_from() {
        let mut aggregator = GroupsAggregator::new(2, 2, "docId".to_string());

        aggregator.groups.insert(
            GroupKey::from("a"),
            [
                ScoredPoint {
                    id: 1.into(),
                    version: 0,
                    score: 1.0,
                    payload: None,
                    vector: None,
                },
                ScoredPoint {
                    id: 2.into(),
                    version: 0,
                    score: 1.0,
                    payload: None,
                    vector: None,
                },
            ]
            .into(),
        );

        aggregator.groups.insert(
            GroupKey::from("b"),
            [
                ScoredPoint {
                    id: 3.into(),
                    version: 0,
                    score: 1.0,
                    payload: None,
                    vector: None,
                },
                ScoredPoint {
                    id: 4.into(),
                    version: 0,
                    score: 1.0,
                    payload: None,
                    vector: None,
                },
            ]
            .into(),
        );

        let payload_a = Payload::from(serde_json::json!({"some_key": "some value a"}));
        let payload_b = Payload::from(serde_json::json!({"some_key": "some value b"}));

        let hydrated = vec![
            ScoredPoint {
                id: 1.into(),
                version: 0,
                score: 1.0,
                payload: Some(payload_a.clone()),
                vector: None,
            },
            ScoredPoint {
                id: 2.into(),
                version: 0,
                score: 1.0,
                payload: Some(payload_a.clone()),
                vector: None,
            },
            ScoredPoint {
                id: 3.into(),
                version: 0,
                score: 1.0,
                payload: Some(payload_b.clone()),
                vector: None,
            },
            ScoredPoint {
                id: 4.into(),
                version: 0,
                score: 1.0,
                payload: Some(payload_b.clone()),
                vector: None,
            },
        ];

        aggregator.hydrate_from(&hydrated);

        assert_eq!(aggregator.groups.len(), 2);
        assert_eq!(
            aggregator.groups.get(&GroupKey::from("a")).unwrap().len(),
            2
        );
        assert_eq!(
            aggregator.groups.get(&GroupKey::from("b")).unwrap().len(),
            2
        );

        let a = aggregator.groups.get(&GroupKey::from("a")).unwrap();
        let b = aggregator.groups.get(&GroupKey::from("b")).unwrap();

        assert!(a.iter().all(|x| x.payload == Some(payload_a.clone())));
        assert!(b.iter().all(|x| x.payload == Some(payload_b.clone())));
    }
}
