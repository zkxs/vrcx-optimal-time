// Copyright 2022 Michael Ripley
// This file is part of vrcx-optimal-time.
// vrcx-optimal-time is licensed under the MIT license (see LICENSE file for details).

// various time constants
pub const DAYS_PER_WEEK: usize = 7;
pub const HOURS_PER_DAY: u32 = 24;
pub const MINUTES_PER_HOUR: u32 = 60;
pub const SECONDS_PER_MINUTE: u32 = 60;
pub const MINUTES_PER_DAY: u32 = HOURS_PER_DAY * MINUTES_PER_HOUR;

// indices of the columns we get back in our sqlite query result set
pub const COLUMN_INDEX_CREATED_AT: usize = 0;
pub const COLUMN_INDEX_USER_ID: usize = 1;
pub const COLUMN_INDEX_DISPLAY_NAME: usize = 2;
pub const COLUMN_INDEX_EVENT_TYPE: usize = 3;
