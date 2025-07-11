// Copyright 2022-2024 Michael Ripley
// This file is part of vrcx-optimal-time.
// vrcx-optimal-time is licensed under the MIT license (see LICENSE file for details).

use std::collections::{HashMap, HashSet};
use std::fs;
use std::time::Instant;

use chrono::naive::NaiveTime;
use chrono::{DateTime, Datelike, Duration, DurationRound, Local, Timelike, Utc, Weekday};
use num_traits::cast::FromPrimitive;
use rusqlite::{Connection, DropBehavior, OpenFlags};

use config::Configuration;

use crate::constants::{
    COLUMN_INDEX_CREATED_AT, DAYS_PER_WEEK, MILLISECONDS_PER_HOUR, MINUTES_PER_DAY, SECONDS_PER_MINUTE,
};
use crate::dto::{BucketValue, OnlineOfflineEventType, Row, TimeSpan, VrcxStartStopEvent, VrcxStartStopEventType};

mod config;
mod constants;
mod dto;

fn main() {
    // record application start time
    let application_start_time = Instant::now();

    // load the config
    let config_string = fs::read_to_string("config.toml").unwrap();
    let config: Configuration = toml::from_str(&config_string).unwrap();

    // derive constants from config
    let (buckets_per_day, buckets_per_day_remainder) = (
        MINUTES_PER_DAY / config.bucket_duration_minutes,
        MINUTES_PER_DAY % config.bucket_duration_minutes,
    );
    assert_eq!(
        buckets_per_day_remainder, 0,
        "bucket_duration_minutes does not perfectly divide a day"
    );
    let buckets_per_day: usize = usize::try_from(buckets_per_day).unwrap();
    let bucket_duration_seconds: u32 = config.bucket_duration_minutes * SECONDS_PER_MINUTE;
    let bucket_duration: Duration = Duration::minutes(i64::from(config.bucket_duration_minutes));
    let vrcx_running_detection_threshold: Duration =
        Duration::minutes(i64::from(config.vrcx_running_detection_threshold_minutes));
    let start_time = config
        .start_time
        .map(|t| DateTime::parse_from_rfc3339(&t).unwrap().with_timezone(&Utc));
    let minimum_bucket_activations = config.minimum_bucket_activations.unwrap_or(1).max(1);
    let no_data_returns_zero = config.no_data_returns_zero.unwrap_or(false);
    let should_print_statistics = config.print_statistics.unwrap_or(false);
    let should_print_runtime = config.print_runtime.unwrap_or(false);

    // open the sqlite database
    let mut db = Connection::open_with_flags(
        config.vrcx_db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .unwrap();

    // set up data structures we'll need for the VRCX running analysis
    let mut buckets = build_daily_buckets(buckets_per_day);
    let mut vrcx_start_stop_events: Vec<VrcxStartStopEvent> = Vec::new();
    let first_event_timestamp: Option<DateTime<Utc>>;
    let last_event_timestamp: Option<DateTime<Utc>>;
    let all_event_count: usize;
    let mut online_offline_event_count: usize = 0;

    // build and run the all events query
    let stripped_user_id = config.your_user_id.replace(['-', '_'], "");
    let all_events_statement = format!(
        "select created_at from {stripped_user_id}_feed_avatar union select created_at from {stripped_user_id}_feed_gps union select created_at from {stripped_user_id}_feed_online_offline union select created_at from {stripped_user_id}_feed_status union select created_at from {stripped_user_id}_friend_log_history order by created_at asc;"
    );

    // run a big transactional read
    {
        let mut transaction = db.transaction().unwrap();
        transaction.set_drop_behavior(DropBehavior::Commit);
        let mut all_events_statement = transaction.prepare(&all_events_statement).unwrap();
        let all_event_timestamps = all_events_statement.query_map((), parse_created_at).unwrap();
        let all_event_timestamps: Vec<DateTime<Utc>> = all_event_timestamps.map(|event| event.unwrap()).collect();

        all_event_count = all_event_timestamps.len();
        first_event_timestamp = all_event_timestamps.first().map(|ts| ts.to_owned());
        last_event_timestamp = all_event_timestamps.last().map(|ts| ts.to_owned());

        // process all event timestamps
        let mut vrcx_running: bool = false;
        for window in all_event_timestamps.windows(2) {
            match window {
                &[event_timestamp_1, event_timestamp_2] => {
                    let duration = event_timestamp_2.signed_duration_since(event_timestamp_1);
                    assert!(duration >= Duration::zero()); // assert that data is, in fact, ascending
                    if duration <= vrcx_running_detection_threshold && duration >= Duration::zero() {
                        // we can skip over zero-length durations
                        // duration between events was within the threshold, so assume VRCX is running for this entire time range

                        if !vrcx_running {
                            // vrcx just started running

                            // the previous event should have been a stop event (or empty)
                            debug_assert!(
                                vrcx_start_stop_events.last().is_none()
                                    || matches!(
                                        vrcx_start_stop_events.last(),
                                        Some(VrcxStartStopEvent {
                                            event: VrcxStartStopEventType::Stop,
                                            ..
                                        })
                                    )
                            );

                            vrcx_running = true;
                            vrcx_start_stop_events.push(VrcxStartStopEvent::start(event_timestamp_1));
                        } // else, if vrcx was already running there's nothing for us to do

                        // use any VRCX events available to reason that VRCX is running during a given time range
                        let time_span = TimeSpan::new(event_timestamp_1, event_timestamp_2);
                        register_bucket_dates_for_range(
                            bucket_duration,
                            config.bucket_duration_minutes,
                            time_span,
                            buckets.as_mut_slice(),
                        );
                    } else if vrcx_running {
                        // duration was outside threshold, so assume VRCX is *not* running for this range (which may be quite long)
                        // also, VRCX was running in the previous range, therefore we need to push a stop event

                        // the previous event should have been a start event
                        debug_assert!(matches!(
                            vrcx_start_stop_events.last(),
                            Some(VrcxStartStopEvent {
                                event: VrcxStartStopEventType::Start,
                                ..
                            })
                        ));

                        vrcx_running = false;
                        vrcx_start_stop_events.push(VrcxStartStopEvent::stop(event_timestamp_1));
                    }
                }
                _ => unreachable!(),
            }
        }

        // push the final stop event, if needed
        if !matches!(
            vrcx_start_stop_events.last().unwrap().event,
            VrcxStartStopEventType::Stop
        ) {
            vrcx_start_stop_events.push(VrcxStartStopEvent::stop(*all_event_timestamps.last().unwrap()));
        }

        // build and run the online/offline query
        let online_offline_statement = format!(
            "select created_at, user_id, display_name, type from {stripped_user_id}_feed_online_offline order by created_at asc"
        );
        let mut online_offline_statement = transaction.prepare(&online_offline_statement).unwrap();
        let user_online_offline_events = online_offline_statement
            .query_map((), |row| Row::try_from(row))
            .unwrap();

        // set up data structures we'll need for the online/offline analysis
        let mut user_online_time: HashMap<String, DateTime<Utc>> = HashMap::new();

        // process the user online/offline events
        for row in user_online_offline_events {
            let row = row.unwrap();
            online_offline_event_count += 1;

            // apply start_time filter
            if start_time.map_or(false, |start| start > row.created_at) {
                continue;
            }

            if is_user_allowed(&row.user_id, &config.friend_ids) {
                match row.event_type {
                    OnlineOfflineEventType::Online => {
                        // it is intentional that this overwrites previous Online events,
                        // because given two Online events in a row we should drop the first one
                        user_online_time.insert(row.user_id, row.created_at);
                    }
                    OnlineOfflineEventType::Offline => {
                        let online_time = user_online_time.remove(&row.user_id);
                        if let Some(online_time) = online_time {
                            let offline_time = row.created_at;
                            let time_span = TimeSpan::new(online_time, offline_time);
                            if time_span.stop < time_span.start {
                                panic!(
                                    "Got a negative ({}ms) duration for {}. This should not happen.",
                                    time_span.duration().num_milliseconds(),
                                    row.display_name
                                );
                            }
                            if time_span.stop > time_span.start {
                                if let Ok(events) =
                                    clamp_range_to_vrcx_uptime(time_span, vrcx_start_stop_events.as_slice())
                                {
                                    // perfect, we got a usable event. We need to update buckets!
                                    for time_span in events.into_iter() {
                                        if time_span.is_negative_or_zero() {
                                            panic!(
                                                "Got a non-positive clamped duration ({}ms) for {}. This should not happen if my clamping code actually works.",
                                                time_span.duration().num_milliseconds(),
                                                row.display_name
                                            );
                                        }
                                        update_bucket_counts_for_range(
                                            bucket_duration,
                                            config.bucket_duration_minutes,
                                            time_span,
                                            buckets.as_mut_slice(),
                                        );
                                    }
                                } // else, the range is too long, so drop the event
                            } // else, the time_span doesn't have positive duration so we skip it. Note that this also drops the Online event, because a simultaneous Online+Offline event is nonsensical.
                        } // else, no matching online time, so drop the event
                    }
                };
            }
        }
    }

    if should_print_statistics {
        print_statistics(
            bucket_duration_seconds,
            start_time,
            first_event_timestamp,
            last_event_timestamp,
            all_event_count,
            online_offline_event_count,
            &buckets,
        );
    }

    // output the results
    print_buckets(
        bucket_duration_seconds,
        buckets_per_day,
        config.normalize,
        minimum_bucket_activations,
        no_data_returns_zero,
        buckets,
    );

    if should_print_runtime {
        eprintln!(
            "Finished in {:.3}s.",
            application_start_time.elapsed().as_millis() as f64 / 1000.0
        );
    }
}

/// clamps a time range to when VRCX was running
/// if vrcx was running for the entire range, returns the input range
/// otherwise, return the range truncated to when VRCX was known to be running
/// if the range cannot be truncated, returns Err
fn clamp_range_to_vrcx_uptime(
    time_span: TimeSpan,
    vrcx_start_stop_events: &[VrcxStartStopEvent],
) -> Result<Vec<TimeSpan>, ()> {
    // Compute index of the VRCX start/stop event preceding this time range.
    let start_idx = vrcx_start_stop_events
        .binary_search_by_key(&time_span.start, |event| event.timestamp)
        .unwrap_or_else(|insert_idx| insert_idx.checked_sub(1).unwrap());

    // Compute index of the VRCX start/stop event following this time range.
    // In certain edge cases (VRCX is currently running?) this might be out of the slice bounds.
    let stop_idx = vrcx_start_stop_events
        .binary_search_by_key(&time_span.stop, |event| event.timestamp)
        .unwrap_or_else(|insert_idx| insert_idx);

    // now, if VRCX was running the entire time, then all of following should be true
    // A) start_idx == stop_idx - 1
    // B) vrcx_start_stop_events[start_idx] is a start event
    // C) vrcx_start_stop_events[stop_idx] is a stop event

    // VRCX was not running at the beginning of the event
    // [..., vrcx_stop, event_start, ...]
    if !matches!(vrcx_start_stop_events[start_idx].event, VrcxStartStopEventType::Start) {
        // also, VRCX started after the event ended
        return if stop_idx >= vrcx_start_stop_events.len()
            || !matches!(vrcx_start_stop_events[stop_idx].event, VrcxStartStopEventType::Stop)
        {
            // yeah, I have no idea how to deal with this
            // [..., vrcx_stop, event_start, ..., event_stop, vrcx_start, ...]

            // debug print the mystery event:
            // println!(
            //     "wat:\n  vrcx_stop={}\n  event_start={}\n  events={}\n  event_stop={}\n  vrcx_start={}",
            //     vrcx_start_stop_events[start_idx].timestamp,
            //     time_span.start,
            //     stop_idx - start_idx - 1,
            //     time_span.start,
            //     vrcx_start_stop_events[stop_idx].timestamp,
            // );
            Err(())
        } else {
            // VRCX was not running at the beginning of the event, but the stop is normal
            // [..., vrcx_stop, event_start, ..., vrcx_start, event_stop, vrcx_stop, ...]
            // we can use the tail end of this event
            let event_2_start = &vrcx_start_stop_events[stop_idx - 1];
            assert!(matches!(event_2_start.event, VrcxStartStopEventType::Start));
            Ok(vec![TimeSpan::new(event_2_start.timestamp, time_span.stop)])
        };
    }

    // VRCX started after the event ended, but the start is normal
    // [..., vrcx_start, event_start, vrcx_stop, ..., event_stop, vrcx_start, ...]
    if stop_idx >= vrcx_start_stop_events.len()
        || !matches!(vrcx_start_stop_events[stop_idx].event, VrcxStartStopEventType::Stop)
    {
        // we can use the front end of this event
        let event_1_stop = &vrcx_start_stop_events[start_idx + 1];
        assert!(matches!(event_1_stop.event, VrcxStartStopEventType::Stop));
        return Ok(vec![TimeSpan::new(time_span.start, event_1_stop.timestamp)]);
    }

    // VRCX restarted during the event
    // [..., vrcx_start, event_start, vrcx_stop, ..., vrcx_start, event_stop, vrcx_stop, ...]
    if start_idx != stop_idx.checked_sub(1).unwrap() {
        // we can use both ends of the event as separate events
        let event_1_stop = &vrcx_start_stop_events[start_idx + 1];
        assert!(matches!(event_1_stop.event, VrcxStartStopEventType::Stop));
        let event_2_start = &vrcx_start_stop_events[stop_idx - 1];
        assert!(matches!(event_2_start.event, VrcxStartStopEventType::Start));
        return Ok(vec![
            TimeSpan::new(time_span.start, event_1_stop.timestamp),
            TimeSpan::new(event_2_start.timestamp, time_span.stop),
        ]);
    }

    // none of the edge cases occurred
    // [..., vrcx_start, event_start, event_stop, vrcx_stop, ...]
    Ok(vec![time_span])
}

/// build buckets according to configured bucket size
fn build_daily_buckets(buckets_per_day: usize) -> Vec<Vec<BucketValue>> {
    vec![vec![BucketValue::default(); buckets_per_day]; DAYS_PER_WEEK]
}

/// update bucket counts that a provided range encompasses
fn update_bucket_counts_for_range(
    bucket_duration: Duration,
    bucket_duration_minutes: u32,
    time_span: TimeSpan,
    buckets: &mut [Vec<BucketValue>],
) {
    let end_time = time_span.stop.with_timezone(&Local);
    let mut start_time = time_span.start.with_timezone(&Local);
    start_time = start_time.duration_trunc(bucket_duration).unwrap();

    while start_time < end_time {
        let weekday = start_time.weekday();
        let day_index = usize::try_from(weekday.num_days_from_monday()).unwrap();
        let time = start_time.time();
        let minutes_of_day = u32::try_from(time.signed_duration_since(NaiveTime::default()).num_minutes()).unwrap();
        let bucket_index = usize::try_from(minutes_of_day / bucket_duration_minutes).unwrap();

        // increment the friend online count
        buckets[day_index][bucket_index].increment();

        // we're assuming that VRCX is actually running for this whole range, so update the VRCX running dates as well...
        buckets[day_index][bucket_index].register_date(start_time);

        start_time += bucket_duration;
    }
}

/// register this range's dates as active for the relevant buckets
fn register_bucket_dates_for_range(
    bucket_duration: Duration,
    bucket_duration_minutes: u32,
    time_span: TimeSpan,
    buckets: &mut [Vec<BucketValue>],
) {
    let end_time = time_span.stop.with_timezone(&Local);
    let start_time = time_span.start.with_timezone(&Local);
    let first_bucket_start_time = start_time.duration_trunc(bucket_duration).unwrap();
    // start at first WHOLE bucket
    let mut current_time = if first_bucket_start_time == start_time {
        first_bucket_start_time
    } else {
        let second_bucket_start_time = first_bucket_start_time + bucket_duration;

        // handle the first, partial bucket
        let first_bucket_duration = TimeSpan::new(
            first_bucket_start_time.with_timezone(&Utc),
            second_bucket_start_time.with_timezone(&Utc),
        )
        .duration();
        if first_bucket_duration > bucket_duration / 2 {
            register_bucket_date(bucket_duration_minutes, second_bucket_start_time, buckets);
        }

        second_bucket_start_time
    };

    // process each WHOLE bucket
    while current_time < end_time {
        register_bucket_date(bucket_duration_minutes, current_time, buckets);
        current_time += bucket_duration;
    }

    // handle any remaining time
    let last_bucket_start_time = current_time;
    let last_bucket_duration = TimeSpan::new(last_bucket_start_time.with_timezone(&Utc), time_span.stop).duration();
    if last_bucket_duration > bucket_duration / 2 {
        register_bucket_date(bucket_duration_minutes, last_bucket_start_time, buckets);
    }
}

#[inline]
fn register_bucket_date(bucket_duration_minutes: u32, bucket_time: DateTime<Local>, buckets: &mut [Vec<BucketValue>]) {
    let weekday = bucket_time.weekday();
    let day_index = usize::try_from(weekday.num_days_from_monday()).unwrap();
    let time = bucket_time.time();
    let minutes_of_day = u32::try_from(time.signed_duration_since(NaiveTime::default()).num_minutes()).unwrap();
    let bucket_index = usize::try_from(minutes_of_day / bucket_duration_minutes).unwrap();
    buckets[day_index][bucket_index].register_date(bucket_time);
}

fn print_statistics(
    bucket_duration_seconds: u32,
    start_time: Option<DateTime<Utc>>,
    first_event_timestamp: Option<DateTime<Utc>>,
    last_event_timestamp: Option<DateTime<Utc>>,
    all_event_count: usize,
    online_offline_event_count: usize,
    buckets: &[Vec<BucketValue>],
) {
    let current_time = Utc::now();

    eprintln!("Processed {all_event_count} timestamps and {online_offline_event_count} online/offline events.");

    if let Some(first_event_timestamp) = first_event_timestamp {
        if let Some(last_event_timestamp) = last_event_timestamp {
            let vrcx_duration: Duration = current_time.signed_duration_since(first_event_timestamp);
            let vrcx_hours: f64 =
                f64::from_i64(vrcx_duration.num_milliseconds()).unwrap() / f64::from(MILLISECONDS_PER_HOUR);

            let vrcx_offline_duration = current_time.signed_duration_since(last_event_timestamp);
            let vrcx_offline_hours: f64 =
                f64::from_i64(vrcx_offline_duration.num_milliseconds()).unwrap() / f64::from(MILLISECONDS_PER_HOUR);

            let activations: usize = buckets
                .iter()
                .flatten()
                .map(|bucket_value| bucket_value.total_dates())
                .sum();
            let active_seconds: i64 = i64::try_from(activations).unwrap() * i64::from(bucket_duration_seconds);
            let active_duration: Duration = Duration::seconds(active_seconds);
            let active_hours: f64 =
                f64::from_i64(active_duration.num_milliseconds()).unwrap() / f64::from(MILLISECONDS_PER_HOUR);

            let active_percent: f64 = 100.0 * active_hours / vrcx_hours;

            eprintln!(
                "VRCX range: {vrcx_hours:.2} hours. VRCX actually active for {active_hours:.2} hours. That's {active_percent:.1}% uptime. Last VRCX data is {vrcx_offline_hours:.2} hours old."
            );
        }
    }

    if let Some(start_time) = start_time {
        let desired_duration = current_time.signed_duration_since(start_time);
        let desired_hours: f64 =
            f64::from_i64(desired_duration.num_milliseconds()).unwrap() / f64::from(MILLISECONDS_PER_HOUR);

        let activations: usize = buckets
            .iter()
            .flatten()
            .flat_map(|bucket_value| bucket_value.vrcx_activity_dates.iter())
            .filter(|time| time >= &&start_time)
            .count();
        let active_seconds: i64 = i64::try_from(activations).unwrap() * i64::from(bucket_duration_seconds);
        let active_duration: Duration = Duration::seconds(active_seconds);
        let active_hours: f64 =
            f64::from_i64(active_duration.num_milliseconds()).unwrap() / f64::from(MILLISECONDS_PER_HOUR);

        let active_percent: f64 = 100.0 * active_hours / desired_hours;

        eprintln!(
            "Desired range: {desired_hours:.2} hours. VRCX actually active for {active_hours:.2} hours. That's {active_percent:.1}% uptime."
        );
    }
}

/// print bucket data to console
fn print_buckets(
    bucket_duration_seconds: u32,
    buckets_per_day: usize,
    normalize: bool,
    minimum_bucket_activations: u32,
    no_data_returns_zero: bool,
    buckets: Vec<Vec<BucketValue>>,
) {
    // header
    print!("bucket");
    for day in 0..DAYS_PER_WEEK {
        let weekday = Weekday::from_usize(day).unwrap();
        print!("\t{weekday}");
    }
    println!();

    for bucket_index in 0..buckets_per_day {
        print!("{}", bucket_index_to_label(bucket_duration_seconds, bucket_index));
        for day in 0..DAYS_PER_WEEK {
            let buckets_for_day = buckets.get(day).unwrap();
            let bucket_value = buckets_for_day.get(bucket_index).unwrap();
            let online_count = bucket_value.online_count;

            let vrcx_activity_count = bucket_value.total_dates();
            if vrcx_activity_count == 0 && online_count != 0 {
                panic!(
                    "We somehow have vrcx_activity_count={vrcx_activity_count} and online_count={online_count}, which is nonsensical."
                );
            }

            if u32::try_from(vrcx_activity_count).unwrap() < minimum_bucket_activations {
                // not enough activity, so return no data
                if no_data_returns_zero {
                    print!("\t0");
                } else {
                    print!("\t");
                }
            } else if normalize {
                // we're normalizing, so we have to do floating point math
                // we'll just do this in a completely separate branch than the un-normalized stuff

                /* This next line requires some explanation. TL;DR: it's to account for bias in when data is recorded.
                 *
                 * Imagine you started using VRCX 100 weeks ago (nearly two years). You don't always run VRCX, because you
                 * turn your computer off sometimes. Lets say that on Saturdays you have a 90% chance of having VRCX running,
                 * while on Wednesdays you only have a 5% chance. Lets call a bucket "active" for a day if VRCX was running.
                 * This means a given Saturday bucket would have been active for ~90 days, but a Wednesday bucket would only have
                 * been active for ~5 days.
                 *
                 * Next, imagine you have a friend who has zero reason to their schedule, and has a perfectly equal chance of being online
                 * at any given time. Without accounting for the bias introduced by when you run VRCX, this friend would appear 18x more
                 * active on Sundays than Wednesdays, which is clearly not true. So you'd see say, 180 hits for Sunday and 10 hits for Wednesday.
                 *
                 * The solution is to record the number of days for which a bucket is "active", and divide the friend online count by that activity count.
                 * This normalizes the data. For Sunday, 180 / 90 = 2. For Wednesday, 10 / 5 = 2.
                 */
                let normalized_online_activity: f64 =
                    f64::from(online_count) / f64::from_usize(vrcx_activity_count).unwrap();
                print!("\t{normalized_online_activity}");
            } else {
                // we aren't normalizing, so we just return the online_count integer
                print!("\t{online_count}");
            }
        }
        println!();
    }
}

/// convert a bucket index into a label string
fn bucket_index_to_label(bucket_duration_seconds: u32, bucket_index: usize) -> String {
    let time = bucket_index_to_time(bucket_duration_seconds, bucket_index);
    format!("{:02}:{:02}", time.hour(), time.minute())
}

/// convert a bucket index to the time of day
fn bucket_index_to_time(bucket_duration_seconds: u32, bucket_index: usize) -> NaiveTime {
    let seconds_from_midnight = bucket_duration_seconds * u32::try_from(bucket_index).unwrap();
    NaiveTime::from_num_seconds_from_midnight_opt(seconds_from_midnight, 0).unwrap()
}

/// check if a given user has been filtered out by our configuration
fn is_user_allowed(user_id: &str, friend_ids: &Option<HashSet<String>>) -> bool {
    // if friend ids is unset, then allow every user id
    friend_ids
        .as_ref()
        .map_or(true, |friend_ids| friend_ids.contains(user_id))
}

/// parse a timestamp from a sqlite result
fn parse_created_at(row: &rusqlite::Row<'_>) -> Result<DateTime<Utc>, rusqlite::Error> {
    let created_at: String = row.get(COLUMN_INDEX_CREATED_AT)?;
    Ok(created_at.parse::<DateTime<Utc>>().unwrap())
}
