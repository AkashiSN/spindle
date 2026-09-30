//! spindle 本体と spindle-agent が共有するワイヤ型。serde 以外に依存しない。

use serde::{Deserialize, Serialize};

/// ワイヤプロトコルの版。
pub const PROTO_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PairRequest {
    pub code: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PairResponse {
    pub device_uuid: String,
    pub device_name: String,
    pub token: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ManifestResponse {
    pub device_uuid: String,
    pub device_name: String,
    pub generation: i64,
    pub plan_token: String,
    pub pending_reevaluation: bool,
    pub items: Vec<ManifestItem>,
    pub playlists: Vec<ManifestPlaylist>,
    pub diff: DiffView,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ManifestItem {
    pub track_id: i64,
    pub dest_path: String,
    pub token: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ManifestPlaylist {
    pub playlist_id: i64,
    pub name: String,
    pub token: String,
    pub tracks: Vec<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiffView {
    pub items: Vec<ItemOp>,
    pub held: Vec<Held>,
    pub playlists: Vec<PlaylistOp>,
    pub playlist_errors: Vec<PlaylistError>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpKind {
    Delete,
    Move,
    UpdateMove,
    Update,
    Add,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlaylistOpKind {
    Add,
    Update,
    Delete,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ItemOp {
    pub op: OpKind,
    pub track_id: i64,
    pub from: Option<String>,
    pub to: Option<String>,
    pub token: Option<String>,
    pub size: u64,
    pub sha256: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Held {
    pub track_id: i64,
    pub reason: String,
    pub waiting: bool,
    pub has_copy: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlaylistOp {
    pub op: PlaylistOpKind,
    pub playlist_id: i64,
    pub from: Option<String>,
    pub to: Option<String>,
    pub token: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlaylistError {
    pub playlist_id: i64,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfirmRequest {
    pub plan_token: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    pub plan_id: i64,
    pub generation: i64,
    pub plan_token: String,
    pub items: Vec<PlanItem>,
    pub playlists: Vec<PlanPlaylist>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanItem {
    pub op_id: String,
    pub op: OpKind,
    pub track_id: i64,
    pub from: Option<String>,
    pub to: Option<String>,
    pub token: Option<String>,
    pub size: u64,
    pub sha256: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanPlaylist {
    pub op_id: String,
    pub op: PlaylistOpKind,
    pub playlist_id: i64,
    pub from: Option<String>,
    pub to: Option<String>,
    pub token: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReportRequest {
    pub generation: i64,
    pub plan_id: i64,
    pub state: ReportState,
    pub errors: Vec<ReportError>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReportState {
    pub tracks: Vec<ReportTrack>,
    pub playlists: Vec<ReportPlaylist>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReportTrack {
    pub track_id: i64,
    pub dest_path: String,
    pub token: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReportPlaylist {
    pub playlist_id: i64,
    pub name: String,
    pub token: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    Track,
    Playlist,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReportError {
    pub kind: ErrorKind,
    pub ref_id: i64,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AbandonRequest {
    #[serde(flatten)]
    pub report: ReportRequest,
    pub pending_ops: u64,
    pub pending_batches: u64,
}

/// エラー応答（本体の `error_response_with_message` と同じ形）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub error: String,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub plan_token: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn op_kinds_are_snake_case() {
        assert_eq!(
            serde_json::to_string(&OpKind::UpdateMove).unwrap(),
            "\"update_move\""
        );
        assert_eq!(
            serde_json::to_string(&ErrorKind::Playlist).unwrap(),
            "\"playlist\""
        );
    }

    #[test]
    fn abandon_request_flattens_report() {
        let v = serde_json::json!({
            "generation": 3, "plan_id": 9,
            "state": {"tracks": [], "playlists": []}, "errors": [],
            "pending_ops": 0, "pending_batches": 0
        });
        let a: AbandonRequest = serde_json::from_value(v).unwrap();
        assert_eq!(a.report.plan_id, 9);
        assert_eq!(a.pending_batches, 0);
    }
}
