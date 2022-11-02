use std::collections::HashSet;
use serde_derive::Deserialize;

#[derive(Deserialize)]
pub struct Configuration {
    pub your_user_id: String,
    pub vrcx_db_path: String,
    pub friend_ids: Option<HashSet<String>>,
    pub vrcx_running_detection_threshold_minutes: u32,
    pub bucket_duration_minutes: u32,
}
