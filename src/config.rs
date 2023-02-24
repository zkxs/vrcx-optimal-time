// Copyright 2022 Michael Ripley
// This file is part of vrcx-optimal-time.
// vrcx-optimal-time is licensed under the MIT license (see LICENSE file for details).

use std::collections::HashSet;
use serde_derive::Deserialize;

#[derive(Deserialize)]
pub struct Configuration {
    pub your_user_id: String,
    pub vrcx_db_path: String,
    pub friend_ids: Option<HashSet<String>>,
    pub vrcx_running_detection_threshold_minutes: u32,
    pub bucket_duration_minutes: u32,
    pub normalize: bool,
    pub start_time: Option<String>,
    pub minimum_bucket_activations: Option<u32>,
    pub no_data_returns_zero: Option<bool>,
}
