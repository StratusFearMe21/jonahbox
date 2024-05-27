use axum::Json;
use serde::{Deserialize, Serialize};

use crate::State;

#[derive(Deserialize, Serialize)]
pub struct TTSResponse {
    success: bool,
    url: String,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TTSRequest {
    engine: String,
    fail_sim: String,
    file_format: String,
    rate: String,
    text: String,
    text_format: String,
    voice: String,
}

pub async fn generate_handler(
    axum::extract::State(state): axum::extract::State<State>,
    Json(tts): Json<TTSRequest>,
) -> Json<TTSResponse> {
    Json(
        state
            .http_cache
            .client
            .post("https://blobcast.jackboxgames.com/tts/generate")
            .json(&tts)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap(),
    )
}
