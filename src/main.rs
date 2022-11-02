// Copyright 2022 Michael Ripley
// This file is part of vrcx-optimal-time.
// vrcx-optimal-time is licensed under the MIT license (see LICENSE file for details).

use std::collections::{HashMap, HashSet};
use std::fs;

use chrono::{Datelike, DateTime, Duration, DurationRound, Local, NaiveDate, Timelike, Utc, Weekday};
use chrono::naive::NaiveTime;
use num_traits::cast::FromPrimitive;
use rusqlite::{Connection, OpenFlags};

use config::Configuration;

mod config;

const DAYS_PER_WEEK: usize = 7;
const HOURS_PER_DAY: u32 = 24;
const MINUTES_PER_HOUR: u32 = 60;
const SECONDS_PER_MINUTE: u32 = 60;
const MINUTES_PER_DAY: u32 = HOURS_PER_DAY * MINUTES_PER_HOUR;

// indices of the columns we get back in our sqlite query result set
const COLUMN_INDEX_CREATED_AT: usize = 0;
const COLUMN_INDEX_USER_ID: usize = 1;
const COLUMN_INDEX_DISPLAY_NAME: usize = 2;
const COLUMN_INDEX_EVENT_TYPE: usize = 3;

fn main() {
    // load the config
    let config_string = fs::read_to_string("config.toml").unwrap();
    let config: Configuration = toml::from_str(&config_string).unwrap();

    // derive constants from config
    let (buckets_per_day, buckets_per_day_remainder) = (MINUTES_PER_DAY / config.bucket_duration_minutes, MINUTES_PER_DAY % config.bucket_duration_minutes);
    assert_eq!(buckets_per_day_remainder, 0, "bucket_duration_minutes does not perfectly divide a day");
    let buckets_per_day: usize = usize::try_from(buckets_per_day).unwrap();
    let bucket_duration_seconds: u32 = config.bucket_duration_minutes * SECONDS_PER_MINUTE;
    let bucket_duration: Duration = Duration::minutes(i64::try_from(config.bucket_duration_minutes).unwrap());
    let maximum_online_time: Duration = Duration::hours(i64::try_from(config.maximum_online_time_hours).unwrap());
    let vrcx_running_detection_threshold: Duration = Duration::minutes(i64::try_from(config.vrcx_running_detection_threshold_minutes).unwrap());

    // open the sqlite database
    let db = Connection::open_with_flags(
        config.vrcx_db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX).unwrap();

    // build and run the all events query
    let stripped_user_id = config.your_user_id.replace('-', "").replace('_', "");
    let all_events_statement = format!("select created_at from {stripped_user_id}_feed_avatar union select created_at from {stripped_user_id}_feed_gps union select created_at from {stripped_user_id}_feed_status union select created_at from {stripped_user_id}_friend_log_history order by created_at asc");
    let mut all_events_statement = db.prepare(&all_events_statement).unwrap();
    let all_event_timestamps = all_events_statement.query_map((), parse_created_at).unwrap();
    let all_event_timestamps: Vec<DateTime<Utc>> = all_event_timestamps
        .map(|event| event.unwrap())
        .collect();

    // set up data structures we'll need for the VRCX running analysis
    let mut buckets = build_daily_buckets(buckets_per_day);

    // process all event timestamps
    for window in all_event_timestamps.windows(2) {
        match window {
            &[event_timestamp_1, event_timestamp_2] => {
                let duration = event_timestamp_2.signed_duration_since(event_timestamp_1);
                if duration <= vrcx_running_detection_threshold {
                    // use any VRCX events available to reason that VRCX is running during a given time
                    update_bucket_dates_for_range(bucket_duration, config.bucket_duration_minutes, event_timestamp_1, event_timestamp_2, buckets.as_mut_slice());
                }
            }
            _ => unreachable!()
        }
    }

    // build and run the online/offline query
    let online_offline_statement = format!("select created_at, user_id, display_name, type from {stripped_user_id}_feed_online_offline order by id");
    let mut online_offline_statement = db.prepare(&online_offline_statement).unwrap();
    let user_online_offline_events = online_offline_statement.query_map((), |row| Row::try_from(row)).unwrap();

    // set up data structures we'll need for the online/offline analysis
    let mut user_online_time: HashMap<String, DateTime<Utc>> = HashMap::new();

    // process the user online/offline events
    for row in user_online_offline_events {
        let row = row.unwrap();
        if is_user_allowed(&row.user_id, &config.friend_ids) {
            match row.event_type {
                EventType::Online => {
                    // it is intentional that this overwrites previous Online events,
                    // because given two Online events in a row we should drop the first one
                    user_online_time.insert(row.user_id, row.created_at);
                }
                EventType::Offline => {
                    let online_time = user_online_time.remove(&row.user_id);
                    if let Some(online_time) = online_time {
                        let range = row.created_at.signed_duration_since(online_time);
                        if range <= Duration::zero() {
                            // this should not happen as long as events are indexed in the table in chronological order
                            panic!("got a non-positive duration {} for {}", range, row.display_name);
                        } else if range <= maximum_online_time {
                            // perfect, we got a valid event. We need to update buckets!

                            // debug print this event
                            //println!("{:<18} {}", range.to_string(), row.display_name);

                            update_bucket_counts_for_range(bucket_duration, config.bucket_duration_minutes, online_time, row.created_at, buckets.as_mut_slice());
                        } // else, the range was too long, so drop the event
                    } // else, no matching online time, so drop the event
                }
            };
        }
    }

    // output the results
    print_buckets(bucket_duration_seconds, buckets_per_day, buckets);
}

/// build buckets according to configured bucket size
fn build_daily_buckets(buckets_per_day: usize) -> Vec<Vec<BucketValue>> {
    vec![vec![BucketValue::default(); buckets_per_day]; DAYS_PER_WEEK]
}

/// update bucket counts that a provided range encompasses
fn update_bucket_counts_for_range(bucket_duration: Duration, bucket_duration_minutes: u32, start_time: DateTime<Utc>, end_time: DateTime<Utc>, buckets: &mut [Vec<BucketValue>]) {
    let end_time = end_time.with_timezone(&Local);
    let mut start_time = start_time.with_timezone(&Local);
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
fn update_bucket_dates_for_range(bucket_duration: Duration, bucket_duration_minutes: u32, start_time: DateTime<Utc>, end_time: DateTime<Utc>, buckets: &mut [Vec<BucketValue>]) {
    let end_time = end_time.with_timezone(&Local);
    let mut start_time = start_time.with_timezone(&Local);
    start_time = start_time.duration_trunc(bucket_duration).unwrap();

    while start_time < end_time {
        let weekday = start_time.weekday();
        let day_index = usize::try_from(weekday.num_days_from_monday()).unwrap();
        let time = start_time.time();
        let minutes_of_day = u32::try_from(time.signed_duration_since(NaiveTime::default()).num_minutes()).unwrap();
        let bucket_index = usize::try_from(minutes_of_day / bucket_duration_minutes).unwrap();
        buckets[day_index][bucket_index].register_date(start_time);

        start_time += bucket_duration;
    }
}

/// register this event's date as active for the event's bucket
fn update_bucket_dates_for_event(bucket_duration_minutes: u32, event_timestamp: DateTime<Utc>, buckets: &mut [Vec<BucketValue>]) {
    let event_timestamp = event_timestamp.with_timezone(&Local);
    let weekday = event_timestamp.weekday();
    let day_index = usize::try_from(weekday.num_days_from_monday()).unwrap();
    let time = event_timestamp.time();
    let minutes_of_day = u32::try_from(time.signed_duration_since(NaiveTime::default()).num_minutes()).unwrap();
    let bucket_index = usize::try_from(minutes_of_day / bucket_duration_minutes).unwrap();
    buckets[day_index][bucket_index].register_date(event_timestamp);
}

/// print bucket data to console
fn print_buckets(bucket_duration_seconds: u32, buckets_per_day: usize, buckets: Vec<Vec<BucketValue>>) {
    // header
    print!("bucket");
    for day in 0..DAYS_PER_WEEK {
        let weekday = Weekday::from_usize(day).unwrap();
        print!("\t{}", weekday);
    }
    println!();

    for bucket_index in 0..buckets_per_day {
        print!("{}", bucket_index_to_label(bucket_duration_seconds, bucket_index));
        for day in 0..DAYS_PER_WEEK {
            let buckets_for_day = buckets.get(day).unwrap();
            let bucket_value = buckets_for_day.get(bucket_index).unwrap();
            let vrcx_activity_count = bucket_value.total_dates();
            let online_count = bucket_value.online_count;

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
            let normalized_online_activity: f64 = online_count as f64 / vrcx_activity_count as f64;

            print!("\t{}", normalized_online_activity);
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
    NaiveTime::from_num_seconds_from_midnight(seconds_from_midnight, 0)
}

/// check if a given user has been filtered out by our configuration
fn is_user_allowed(user_id: &str, friend_ids: &Option<HashSet<String>>) -> bool {
    if let Some(friend_ids) = friend_ids {
        friend_ids.contains(user_id)
    } else {
        // if friend ids is unset, then allow every user id
        true
    }
}

/// value of a bucket. This represents an n-minute window on a certain day of the week. For example, 8:00 to 8:10 on a Monday.
#[derive(Clone, Default)]
struct BucketValue {
    /// total number of online friends seen for this bucket
    online_count: u32,
    /// records individual dates VRCX has been active on for this bucket
    vrcx_activity_dates: HashSet<NaiveDate>,
}

impl BucketValue {
    /// indicate that a friend is online during this bucket
    fn increment(&mut self) {
        self.online_count += 1;
    }

    /// remember that VRCX was running during the provided date for this bucket
    fn register_date(&mut self, datetime: DateTime<Local>) {
        self.vrcx_activity_dates.insert(datetime.date_naive());
    }

    /// number of distinct dates VRCX was running during for this bucket
    fn total_dates(&self) -> usize {
        self.vrcx_activity_dates.len()
    }
}

/// represents a row from the friend online/offline table
struct Row {
    created_at: DateTime<Utc>,
    user_id: String,
    display_name: String,
    event_type: EventType,
}

impl TryFrom<&rusqlite::Row<'_>> for Row {
    type Error = rusqlite::Error;

    fn try_from(row: &rusqlite::Row<'_>) -> Result<Self, Self::Error> {
        let created_at: String = row.get(COLUMN_INDEX_CREATED_AT)?;
        let created_at: DateTime<Utc> = created_at.parse::<DateTime<Utc>>().unwrap();

        let user_id: String = row.get(COLUMN_INDEX_USER_ID)?;

        let display_name: String = row.get(COLUMN_INDEX_DISPLAY_NAME)?;

        let event_type: String = row.get(COLUMN_INDEX_EVENT_TYPE)?;
        let event_type: EventType = event_type.as_str().try_into()?;

        Ok(Row {
            created_at,
            user_id,
            display_name,
            event_type,
        })
    }
}

/// the type of an online/offline event
enum EventType {
    Online,
    Offline,
}

impl TryFrom<&str> for EventType {
    type Error = rusqlite::Error;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "Online" => Ok(EventType::Online),
            "Offline" => Ok(EventType::Offline),
            _ => Err(rusqlite::Error::InvalidColumnType(COLUMN_INDEX_EVENT_TYPE, value.to_string(), rusqlite::types::Type::Text))
        }
    }
}

/// parse a timestamp from a sqlite result
fn parse_created_at(row: &rusqlite::Row<'_>) -> Result<DateTime<Utc>, rusqlite::Error> {
    let created_at: String = row.get(COLUMN_INDEX_CREATED_AT)?;
    Ok(created_at.parse::<DateTime<Utc>>().unwrap())
}
