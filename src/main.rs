// Copyright 2022 Michael Ripley
// This file is part of vrcx-optimal-time.
// vrcx-optimal-time is licensed under the MIT license (see LICENSE file for details).

use std::collections::{HashMap, HashSet};
use std::fs;
use config::Configuration;
use chrono::naive::NaiveTime;
use chrono::{Datelike, DateTime, Duration, DurationRound, Local, Timelike, Utc, Weekday};
use num_traits::cast::FromPrimitive;
use rusqlite::{Connection, OpenFlags};

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

static TABLE_SUFFIX: &str = "_feed_online_offline";

fn main() {
    // load the config
    let config_string = fs::read_to_string("config.toml").unwrap();
    let config: Configuration = toml::from_str(&config_string).unwrap();

    // derive constants from config
    let buckets_per_day: usize = usize::try_from(MINUTES_PER_DAY / config.bucket_duration_minutes).unwrap();
    let bucket_duration_seconds: u32 = config.bucket_duration_minutes * SECONDS_PER_MINUTE;
    let bucket_duration: Duration = Duration::minutes(i64::try_from(config.bucket_duration_minutes).unwrap());
    let maximum_online_time: Duration = Duration::hours(i64::try_from(config.maximum_online_time_hours).unwrap());

    // open the sqlite database
    let db = Connection::open_with_flags(
        config.vrcx_db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX).unwrap();

    // build and run the query
    let stripped_user_id = config.your_user_id.replace('-', "").replace('_', "");
    let table_name = format!("{}{}", stripped_user_id, TABLE_SUFFIX);
    let statement = format!("select created_at, user_id, display_name, type from {} order by id", table_name);
    let mut statement = db.prepare(&statement).unwrap();
    let user_online_offline_events = statement.query_map((), |row| Row::try_from(row)).unwrap();

    // set up data structures we'll need for the procedure
    let mut user_online_time: HashMap<String, DateTime<Utc>> = HashMap::new();
    let mut buckets: Vec<Vec<u32>> = build_buckets(buckets_per_day);

    // process the results
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

                            update_buckets(bucket_duration, config.bucket_duration_minutes, online_time, row.created_at, buckets.as_mut_slice());
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
fn build_buckets(buckets_per_day: usize) -> Vec<Vec<u32>> {
    vec![vec![0; buckets_per_day]; DAYS_PER_WEEK]
}

/// update buckets that a provided range encompasses
fn update_buckets(bucket_duration: Duration, bucket_duration_minutes: u32, start_time: DateTime<Utc>, end_time: DateTime<Utc>, buckets: &mut [Vec<u32>]) {
    let end_time = end_time.with_timezone(&Local);
    let mut start_time = start_time.with_timezone(&Local);
    start_time = start_time.duration_trunc(bucket_duration).unwrap();

    while start_time < end_time {
        let weekday = start_time.weekday();
        let day_index = usize::try_from(weekday.num_days_from_monday()).unwrap();
        let time = start_time.time();
        let minutes_of_day = u32::try_from(time.signed_duration_since(NaiveTime::default()).num_minutes()).unwrap();
        let bucket_index = usize::try_from(minutes_of_day / bucket_duration_minutes).unwrap();
        buckets[day_index][bucket_index] += 1;

        start_time += bucket_duration;
    }
}

/// print bucket data to console
fn print_buckets(bucket_duration_seconds: u32, buckets_per_day: usize, buckets: Vec<Vec<u32>>) {
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
            let count = buckets_for_day.get(bucket_index).unwrap();
            print!("\t{}", count);
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
