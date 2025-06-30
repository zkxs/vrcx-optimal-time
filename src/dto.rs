// Copyright 2022-2024 Michael Ripley
// This file is part of vrcx-optimal-time.
// vrcx-optimal-time is licensed under the MIT license (see LICENSE file for details).

use std::collections::HashSet;

use chrono::{DateTime, Duration, Local, Utc};

use crate::constants::{
    COLUMN_INDEX_CREATED_AT, COLUMN_INDEX_DISPLAY_NAME, COLUMN_INDEX_EVENT_TYPE, COLUMN_INDEX_USER_ID,
};

/// value of a bucket. This represents an n-minute window on a certain day of the week. For example, 8:00 to 8:10 on a Monday.
#[derive(Clone, Default)]
pub struct BucketValue {
    /// total number of online friends seen for this bucket
    pub online_count: u32,
    /// records individual dates VRCX has been active on for this bucket
    pub vrcx_activity_dates: HashSet<DateTime<Local>>,
}

impl BucketValue {
    /// indicate that a friend is online during this bucket
    pub fn increment(&mut self) {
        self.online_count += 1;
    }

    /// remember that VRCX was running during the provided date for this bucket
    pub fn register_date(&mut self, datetime: DateTime<Local>) {
        self.vrcx_activity_dates.insert(datetime);
    }

    /// number of distinct dates VRCX was running during for this bucket
    pub fn total_dates(&self) -> usize {
        self.vrcx_activity_dates.len()
    }
}

/// represents a row from the friend online/offline table
pub struct Row {
    pub created_at: DateTime<Utc>,
    pub user_id: String,
    pub display_name: String,
    pub event_type: OnlineOfflineEventType,
}

impl TryFrom<&rusqlite::Row<'_>> for Row {
    type Error = rusqlite::Error;

    fn try_from(row: &rusqlite::Row<'_>) -> Result<Self, Self::Error> {
        let created_at: String = row.get(COLUMN_INDEX_CREATED_AT)?;
        let created_at: DateTime<Utc> = created_at.parse::<DateTime<Utc>>().unwrap();

        let user_id: String = row.get(COLUMN_INDEX_USER_ID)?;

        let display_name: String = row.get(COLUMN_INDEX_DISPLAY_NAME)?;

        let event_type: String = row.get(COLUMN_INDEX_EVENT_TYPE)?;
        let event_type: OnlineOfflineEventType = event_type.as_str().try_into()?;

        Ok(Self {
            created_at,
            user_id,
            display_name,
            event_type,
        })
    }
}

/// the type of an online/offline event
pub enum OnlineOfflineEventType {
    Online,
    Offline,
}

impl TryFrom<&str> for OnlineOfflineEventType {
    type Error = rusqlite::Error;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "Online" => Ok(Self::Online),
            "Offline" => Ok(Self::Offline),
            _ => Err(rusqlite::Error::InvalidColumnType(
                COLUMN_INDEX_EVENT_TYPE,
                value.to_string(),
                rusqlite::types::Type::Text,
            )),
        }
    }
}

pub struct VrcxStartStopEvent {
    pub timestamp: DateTime<Utc>,
    pub event: VrcxStartStopEventType,
}

impl VrcxStartStopEvent {
    pub const fn start(timestamp: DateTime<Utc>) -> Self {
        Self {
            timestamp,
            event: VrcxStartStopEventType::Start,
        }
    }

    pub const fn stop(timestamp: DateTime<Utc>) -> Self {
        Self {
            timestamp,
            event: VrcxStartStopEventType::Stop,
        }
    }
}

pub enum VrcxStartStopEventType {
    Start,
    Stop,
}

#[derive(Copy, Clone)]
pub struct TimeSpan {
    pub start: DateTime<Utc>,
    pub stop: DateTime<Utc>,
}

impl TimeSpan {
    pub const fn new(start: DateTime<Utc>, stop: DateTime<Utc>) -> Self {
        Self { start, stop }
    }

    pub fn is_negative_or_zero(self) -> bool {
        self.stop <= self.start
    }

    pub fn duration(self) -> Duration {
        self.stop.signed_duration_since(self.start)
    }
}
