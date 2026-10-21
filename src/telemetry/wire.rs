//! Frame-local dictionaries and timestamp deltas keep records independent and compact.
use super::{DiskSample, Event, Record, Stage, Summary};
use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Serialize, Deserialize)]
struct Batch {
    epoch: u64,
    strings: Vec<String>,
    events: Vec<PackedEvent>,
}

#[derive(Serialize, Deserialize)]
enum PackedEvent {
    Operation(PackedRecord),
    Disk(u64, u64, Vec<(u32, u64)>, Vec<(u32, u64)>),
    Summary(u64, u32, Option<u32>, u64, u64),
}

#[derive(Serialize, Deserialize)]
struct PackedRecord {
    id: (u32, Option<u64>),
    kind: u32,
    repository: Option<u32>,
    target: Option<u32>,
    query: Option<u32>,
    start: u64,
    end: Option<u64>,
    status: u32,
    detailed: bool,
    stages: Vec<(u32, u64, u64, bool, u64)>,
    links: Vec<u32>,
    current_stage: Option<u32>,
    waiting: Vec<u32>,
}

#[derive(Default)]
struct Dictionary<'a> {
    ids: BTreeMap<&'a str, u32>,
    values: Vec<&'a str>,
}

impl<'a> Dictionary<'a> {
    fn id(&mut self, value: &'a str) -> u32 {
        if let Some(id) = self.ids.get(value) {
            return *id;
        }
        let id = self.values.len() as u32;
        self.values.push(value);
        self.ids.insert(value, id);
        id
    }

    fn optional(&mut self, value: &'a Option<String>) -> Option<u32> {
        value.as_deref().map(|value| self.id(value))
    }
}

pub(super) fn encode(events: &[Event]) -> Result<Vec<u8>> {
    let epoch = events
        .iter()
        .map(|event| match event {
            Event::Operation(record) => record
                .stages
                .iter()
                .map(|stage| stage.start_ms.min(stage.end_ms))
                .chain([record.start_ms])
                .min()
                .unwrap_or(record.start_ms),
            Event::Disk(sample) => sample.timestamp_ms,
            Event::Summary(summary) => summary.day.saturating_mul(86_400_000),
        })
        .min()
        .unwrap_or(0);
    let mut strings = Dictionary::default();
    let events = events
        .iter()
        .map(|event| match event {
            Event::Operation(record) => {
                let (prefix, counter) = record
                    .id
                    .rsplit_once('-')
                    .and_then(|(prefix, counter)| {
                        counter
                            .parse::<u64>()
                            .ok()
                            .filter(|value| value.to_string() == counter)
                            .map(|counter| (prefix, Some(counter)))
                    })
                    .unwrap_or((&record.id, None));
                PackedEvent::Operation(PackedRecord {
                    id: (strings.id(prefix), counter),
                    kind: strings.id(&record.kind),
                    repository: strings.optional(&record.repository),
                    target: strings.optional(&record.target),
                    query: strings.optional(&record.query),
                    start: record.start_ms.saturating_sub(epoch),
                    end: record.end_ms.map(|end| end.saturating_sub(epoch)),
                    status: strings.id(&record.status),
                    detailed: record.detailed,
                    stages: record
                        .stages
                        .iter()
                        .map(|stage| {
                            (
                                strings.id(&stage.name),
                                stage.start_ms.saturating_sub(epoch),
                                stage.end_ms.saturating_sub(epoch),
                                stage.waiting,
                                stage.duration_ms,
                            )
                        })
                        .collect(),
                    links: record.links.iter().map(|link| strings.id(link)).collect(),
                    current_stage: strings.optional(&record.current_stage),
                    waiting: record.waiting.iter().map(|name| strings.id(name)).collect(),
                })
            }
            Event::Disk(sample) => PackedEvent::Disk(
                sample.timestamp_ms.saturating_sub(epoch),
                sample.total_bytes,
                sample
                    .repositories
                    .iter()
                    .map(|(name, bytes)| (strings.id(name), *bytes))
                    .collect(),
                sample
                    .shared
                    .iter()
                    .map(|(name, bytes)| (strings.id(name), *bytes))
                    .collect(),
            ),
            Event::Summary(summary) => PackedEvent::Summary(
                summary.day,
                strings.id(&summary.kind),
                strings.optional(&summary.repository),
                summary.count,
                summary.total_duration_ms,
            ),
        })
        .collect();
    Ok(postcard::to_allocvec(&Batch {
        epoch,
        strings: strings.values.into_iter().map(str::to_owned).collect(),
        events,
    })?)
}

pub(super) fn decode(bytes: &[u8]) -> Result<Vec<Event>> {
    let batch: Batch = postcard::from_bytes(bytes)?;
    let remaining = std::cell::Cell::new(8 * 1024 * 1024usize);
    let string = |id: u32| {
        let value = batch
            .strings
            .get(id as usize)
            .ok_or_else(|| anyhow!("invalid monitor dictionary reference"))?;
        let available = remaining
            .get()
            .checked_sub(value.len())
            .ok_or_else(|| anyhow!("monitor dictionary expansion exceeds limit"))?;
        remaining.set(available);
        Ok(value.clone())
    };
    let optional = |id: Option<u32>| id.map(string).transpose();
    let timestamp = |delta: u64| {
        batch
            .epoch
            .checked_add(delta)
            .ok_or_else(|| anyhow!("monitor timestamp overflow"))
    };
    batch
        .events
        .into_iter()
        .map(|event| {
            Ok(match event {
                PackedEvent::Operation(record) => Event::Operation(Record {
                    id: match record.id {
                        (prefix, Some(counter)) => format!("{}-{counter}", string(prefix)?),
                        (value, None) => string(value)?,
                    },
                    kind: string(record.kind)?,
                    repository: optional(record.repository)?,
                    target: optional(record.target)?,
                    query: optional(record.query)?,
                    start_ms: timestamp(record.start)?,
                    end_ms: record.end.map(timestamp).transpose()?,
                    status: string(record.status)?,
                    detailed: record.detailed,
                    stages: record
                        .stages
                        .into_iter()
                        .map(|(name, start, end, waiting, duration_ms)| {
                            Ok(Stage {
                                name: string(name)?,
                                start_ms: timestamp(start)?,
                                end_ms: timestamp(end)?,
                                waiting,
                                duration_ms,
                            })
                        })
                        .collect::<Result<_>>()?,
                    links: record
                        .links
                        .into_iter()
                        .map(string)
                        .collect::<Result<_>>()?,
                    current_stage: optional(record.current_stage)?,
                    waiting: record
                        .waiting
                        .into_iter()
                        .map(string)
                        .collect::<Result<_>>()?,
                }),
                PackedEvent::Disk(time, total_bytes, repositories, shared) => {
                    Event::Disk(DiskSample {
                        timestamp_ms: timestamp(time)?,
                        total_bytes,
                        repositories: repositories
                            .into_iter()
                            .map(|(name, bytes)| Ok((string(name)?, bytes)))
                            .collect::<Result<_>>()?,
                        shared: shared
                            .into_iter()
                            .map(|(name, bytes)| Ok((string(name)?, bytes)))
                            .collect::<Result<_>>()?,
                    })
                }
                PackedEvent::Summary(day, kind, repository, count, total_duration_ms) => {
                    Event::Summary(Summary {
                        day,
                        kind: string(kind)?,
                        repository: optional(repository)?,
                        count,
                        total_duration_ms,
                    })
                }
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_records_preserve_targets_stages_and_shared_storage() {
        let events = vec![
            Event::Operation(Record {
                id: "run-42".into(),
                kind: "search".into(),
                repository: Some("repo".into()),
                target: Some("revision/path".into()),
                query: Some("text:文字列".into()),
                start_ms: 10_000,
                end_ms: Some(11_000),
                status: "success".into(),
                detailed: true,
                stages: vec![Stage {
                    name: "writer".into(),
                    start_ms: 10_010,
                    end_ms: 10_900,
                    waiting: true,
                    duration_ms: 100,
                }],
                links: vec!["run-41".into()],
                current_stage: None,
                waiting: Vec::new(),
            }),
            Event::Disk(DiskSample {
                timestamp_ms: 12_000,
                total_bytes: 100,
                repositories: BTreeMap::from([("repo".into(), 40)]),
                shared: BTreeMap::from([("analysis".into(), 60)]),
            }),
            Event::Summary(Summary {
                day: 0,
                kind: "search".into(),
                repository: Some("repo".into()),
                count: 3,
                total_duration_ms: 900,
            }),
        ];
        let decoded = decode(&encode(&events).unwrap()).unwrap();
        assert_eq!(
            serde_json::to_value(&events).unwrap(),
            serde_json::to_value(decoded).unwrap()
        );
    }

    #[test]
    fn invalid_dictionary_ids_and_timestamp_overflow_are_rejected() {
        let bad_id = Batch {
            epoch: 0,
            strings: Vec::new(),
            events: vec![PackedEvent::Summary(0, 1, None, 1, 1)],
        };
        assert!(decode(&postcard::to_allocvec(&bad_id).unwrap()).is_err());
        let overflow = Batch {
            epoch: u64::MAX,
            strings: Vec::new(),
            events: vec![PackedEvent::Disk(1, 0, Vec::new(), Vec::new())],
        };
        assert!(decode(&postcard::to_allocvec(&overflow).unwrap()).is_err());
    }
}
