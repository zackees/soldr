//! Capture a bounded Tokio-console task snapshot and rank async off-CPU time.

use console_api::{
    async_ops::{AsyncOp, Stats as AsyncOpStats},
    field,
    instrument::{instrument_client::InstrumentClient, InstrumentRequest},
    resources::Resource,
    tasks::{Stats as TaskStats, Task},
    Field, Location, Metadata,
};
use futures::StreamExt;
use serde::Serialize;
use std::{
    collections::HashMap,
    env,
    fs::File,
    io::{self, Write},
    path::PathBuf,
    time::{Duration, Instant},
};

#[derive(Clone)]
struct TaskIdentity {
    metadata_id: Option<u64>,
    explicit_name: Option<String>,
    location: Option<String>,
}

#[derive(Clone)]
struct AsyncOpIdentity {
    source: String,
    resource_id: Option<u64>,
}

#[derive(Clone)]
struct ResourceIdentity {
    concrete_type: String,
    location: Option<String>,
}

#[derive(Serialize)]
struct TaskDump {
    id: u64,
    name: String,
    location: Option<String>,
    state: &'static str,
    window_ms: f64,
    busy_ms: f64,
    scheduled_ms: f64,
    off_cpu_ms: f64,
    polls: u64,
    wakes: u64,
    self_wakes: u64,
    awaiting: Vec<String>,
}

#[derive(Serialize)]
struct Dump {
    endpoint: String,
    requested_capture_ms: u64,
    updates_received: u64,
    dropped_events: u64,
    tasks: Vec<TaskDump>,
}

fn timestamp_nanos(value: Option<&prost_types::Timestamp>) -> i128 {
    value
        .map(|stamp| i128::from(stamp.seconds) * 1_000_000_000 + i128::from(stamp.nanos))
        .unwrap_or_default()
}

fn duration_nanos(value: Option<&prost_types::Duration>) -> i128 {
    value
        .map(|duration| i128::from(duration.seconds) * 1_000_000_000 + i128::from(duration.nanos))
        .unwrap_or_default()
}

fn millis(nanos: i128) -> f64 {
    nanos.max(0) as f64 / 1_000_000.0
}

fn field_value(field: &Field) -> Option<String> {
    match field.value.as_ref()? {
        field::Value::DebugVal(value) | field::Value::StrVal(value) => Some(value.clone()),
        field::Value::U64Val(value) => Some(value.to_string()),
        field::Value::I64Val(value) => Some(value.to_string()),
        field::Value::BoolVal(value) => Some(value.to_string()),
    }
}

fn field_name<'a>(field: &'a Field, metadata: &'a Metadata) -> Option<&'a str> {
    match field.name.as_ref()? {
        field::Name::StrName(name) => Some(name),
        field::Name::NameIdx(index) => metadata
            .field_names
            .get(*index as usize)
            .map(String::as_str),
    }
}

fn task_identity(task: &Task, metadata: &HashMap<u64, Metadata>) -> Option<(u64, TaskIdentity)> {
    let id = task.id?.id;
    let metadata_id = task.metadata.map(|value| value.id);
    let task_metadata = metadata_id.and_then(|value| metadata.get(&value));
    let explicit_name = task_metadata.and_then(|meta| {
        task.fields
            .iter()
            .find(|field| field_name(field, meta) == Some("task.name"))
            .and_then(field_value)
    });
    let location = task.location.as_ref().map(|location| {
        let file = location.file.as_deref().unwrap_or("?");
        let line = location.line.unwrap_or_default();
        format!("{file}:{line}")
    });
    Some((
        id,
        TaskIdentity {
            metadata_id,
            explicit_name,
            location,
        },
    ))
}

fn source_location(location: Option<&Location>) -> Option<String> {
    location.map(|location| {
        let file = location.file.as_deref().unwrap_or("?");
        let line = location.line.unwrap_or_default();
        format!("{file}:{line}")
    })
}

fn async_op_identity(op: &AsyncOp) -> Option<(u64, AsyncOpIdentity)> {
    Some((
        op.id?.id,
        AsyncOpIdentity {
            source: op.source.clone(),
            resource_id: op.resource_id.map(|value| value.id),
        },
    ))
}

fn resource_identity(resource: &Resource) -> Option<(u64, ResourceIdentity)> {
    Some((
        resource.id?.id,
        ResourceIdentity {
            concrete_type: resource.concrete_type.clone(),
            location: source_location(resource.location.as_ref()),
        },
    ))
}

fn stats_busy_nanos_at(stats: &TaskStats, at: i128) -> i128 {
    let completed = duration_nanos(
        stats
            .poll_stats
            .as_ref()
            .and_then(|polls| polls.busy_time.as_ref()),
    );
    let in_progress = stats
        .poll_stats
        .as_ref()
        .map(|polls| {
            let started = timestamp_nanos(polls.last_poll_started.as_ref());
            let ended = timestamp_nanos(polls.last_poll_ended.as_ref());
            if started > ended {
                (at - started).max(0)
            } else {
                0
            }
        })
        .unwrap_or_default();
    completed + in_progress
}

fn stats_scheduled_nanos_at(stats: &TaskStats, at: i128) -> i128 {
    let completed = duration_nanos(stats.scheduled_time.as_ref());
    let last_poll = stats
        .poll_stats
        .as_ref()
        .map(|polls| timestamp_nanos(polls.last_poll_started.as_ref()))
        .unwrap_or_default();
    let last_wake = timestamp_nanos(stats.last_wake.as_ref());
    completed
        + if last_wake > last_poll {
            (at - last_wake).max(0)
        } else {
            0
        }
}

fn task_state(stats: &TaskStats) -> &'static str {
    if stats.dropped_at.is_some() {
        return "completed";
    }
    let last_wake = timestamp_nanos(stats.last_wake.as_ref());
    let Some(polls) = stats.poll_stats.as_ref() else {
        return "idle";
    };
    let last_poll_started = timestamp_nanos(polls.last_poll_started.as_ref());
    if last_wake > last_poll_started {
        "scheduled"
    } else if last_poll_started > timestamp_nanos(polls.last_poll_ended.as_ref()) {
        "running"
    } else {
        "idle"
    }
}

fn parse_args() -> Result<(String, Duration, Option<PathBuf>, Option<PathBuf>), String> {
    let mut args = env::args().skip(1);
    let endpoint = args
        .next()
        .unwrap_or_else(|| "http://127.0.0.1:6669".to_owned());
    let capture_ms = args
        .next()
        .as_deref()
        .unwrap_or("30000")
        .parse::<u64>()
        .map_err(|_| "capture duration must be milliseconds".to_owned())?;
    if capture_ms == 0 {
        return Err("capture duration must be positive".to_owned());
    }
    let output = args.next().map(PathBuf::from);
    let stop_file = args.next().map(PathBuf::from);
    if args.next().is_some() {
        return Err(
            "usage: soldr-tokio-console-dump [endpoint] [capture-ms] [output.json] [stop-file]"
                .into(),
        );
    }
    Ok((
        endpoint,
        Duration::from_millis(capture_ms),
        output,
        stop_file,
    ))
}

async fn connect(
    endpoint: &str,
) -> Result<InstrumentClient<tonic::transport::Channel>, tonic::transport::Error> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match InstrumentClient::connect(endpoint.to_owned()).await {
            Ok(client) => return Ok(client),
            Err(error) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(20)).await;
                drop(error);
            }
            Err(error) => return Err(error),
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (endpoint, capture_duration, output_path, stop_file) =
        parse_args().map_err(io::Error::other)?;
    let mut client = connect(&endpoint).await?;
    let request = tonic::Request::new(InstrumentRequest {});
    let mut stream = client.watch_updates(request).await?.into_inner();
    let deadline = tokio::time::Instant::now() + capture_duration;

    let mut metadata = HashMap::new();
    let mut identities = HashMap::new();
    let mut baseline = HashMap::<u64, TaskStats>::new();
    let mut latest = HashMap::<u64, TaskStats>::new();
    let mut async_identities = HashMap::<u64, AsyncOpIdentity>::new();
    let mut latest_async = HashMap::<u64, AsyncOpStats>::new();
    let mut resources = HashMap::<u64, ResourceIdentity>::new();
    let mut capture_start = None;
    let mut capture_end = None;
    let mut updates_received = 0_u64;
    let mut dropped_events = 0_u64;

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let Some(update) = tokio::time::timeout(remaining, stream.next())
            .await
            .ok()
            .flatten()
        else {
            break;
        };
        let update = update?;
        updates_received += 1;
        let now = timestamp_nanos(update.now.as_ref());
        capture_start.get_or_insert(now);
        capture_end = Some(now);

        if let Some(register) = update.new_metadata {
            for entry in register.metadata {
                if let (Some(id), Some(value)) = (entry.id, entry.metadata) {
                    metadata.insert(id.id, value);
                }
            }
        }
        if let Some(tasks) = update.task_update {
            dropped_events = dropped_events.saturating_add(tasks.dropped_events);
            for task in tasks.new_tasks {
                if let Some((id, identity)) = task_identity(&task, &metadata) {
                    identities.insert(id, identity);
                }
            }
            for (id, stats) in tasks.stats_update {
                if updates_received == 1 {
                    baseline.insert(id, stats);
                }
                latest.insert(id, stats);
            }
        }
        if let Some(async_ops) = update.async_op_update {
            dropped_events = dropped_events.saturating_add(async_ops.dropped_events);
            for op in async_ops.new_async_ops {
                if let Some((id, identity)) = async_op_identity(&op) {
                    async_identities.insert(id, identity);
                }
            }
            latest_async.extend(async_ops.stats_update);
        }
        if let Some(resource_update) = update.resource_update {
            dropped_events = dropped_events.saturating_add(resource_update.dropped_events);
            for resource in resource_update.new_resources {
                if let Some((id, identity)) = resource_identity(&resource) {
                    resources.insert(id, identity);
                }
            }
        }
        if stop_file.as_ref().is_some_and(|path| path.exists()) {
            break;
        }
    }

    let capture_start = capture_start.unwrap_or_default();
    let capture_end = capture_end.unwrap_or(capture_start);
    let mut tasks = latest
        .iter()
        .map(|(&id, stats)| {
            let initial = baseline.get(&id);
            let started = initial
                .map(|_| capture_start)
                .unwrap_or_else(|| timestamp_nanos(stats.created_at.as_ref()).max(capture_start));
            let ended = stats
                .dropped_at
                .as_ref()
                .map(|value| timestamp_nanos(Some(value)))
                .unwrap_or(capture_end)
                .min(capture_end);
            let window = (ended - started).max(0);
            let busy = (stats_busy_nanos_at(stats, ended)
                - initial
                    .map(|value| stats_busy_nanos_at(value, capture_start))
                    .unwrap_or_default())
            .clamp(0, window);
            let scheduled = (stats_scheduled_nanos_at(stats, ended)
                - initial
                    .map(|value| stats_scheduled_nanos_at(value, capture_start))
                    .unwrap_or_default())
            .clamp(0, window - busy);
            let off_cpu = (window - busy - scheduled).max(0);
            let identity = identities.get(&id);
            let meta = identity
                .and_then(|identity| identity.metadata_id)
                .and_then(|metadata_id| metadata.get(&metadata_id));
            let name = identity
                .and_then(|identity| identity.explicit_name.clone())
                .filter(|value| !value.is_empty())
                .or_else(|| meta.map(|value| value.name.clone()))
                .unwrap_or_else(|| "unknown-task".to_owned());
            let mut awaiting = latest_async
                .iter()
                .filter(|(_, stats)| stats.dropped_at.is_none())
                .filter(|(_, stats)| stats.task_id.map(|value| value.id) == Some(id))
                .filter_map(|(async_id, _)| async_identities.get(async_id))
                .map(|op| {
                    let resource = op
                        .resource_id
                        .and_then(|resource_id| resources.get(&resource_id));
                    match resource {
                        Some(resource) => format!(
                            "{} on {}{}",
                            op.source,
                            resource.concrete_type,
                            resource
                                .location
                                .as_ref()
                                .map(|location| format!(" at {location}"))
                                .unwrap_or_default()
                        ),
                        None => op.source.clone(),
                    }
                })
                .collect::<Vec<_>>();
            awaiting.sort();
            awaiting.dedup();
            TaskDump {
                id,
                name,
                location: identity.and_then(|identity| identity.location.clone()),
                state: task_state(stats),
                window_ms: millis(window),
                busy_ms: millis(busy),
                scheduled_ms: millis(scheduled),
                off_cpu_ms: millis(off_cpu),
                polls: stats
                    .poll_stats
                    .map(|polls| polls.polls)
                    .unwrap_or_default(),
                wakes: stats.wakes,
                self_wakes: stats.self_wakes,
                awaiting,
            }
        })
        .collect::<Vec<_>>();
    tasks.sort_by(|left, right| right.off_cpu_ms.total_cmp(&left.off_cpu_ms));

    let dump = Dump {
        endpoint,
        requested_capture_ms: capture_duration.as_millis() as u64,
        updates_received,
        dropped_events,
        tasks,
    };
    let json = serde_json::to_string_pretty(&dump)?;
    match output_path {
        Some(path) => {
            let mut file = File::create(path)?;
            file.write_all(json.as_bytes())?;
            file.write_all(b"\n")?;
        }
        None => println!("{json}"),
    }
    Ok(())
}
