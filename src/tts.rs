use std::{
    hash::{DefaultHasher, Hasher},
    io::Write,
    process::Stdio,
};

use axum::Json;
use nanoid::nanoid;
use rand::{seq::IteratorRandom, SeedableRng};
use rand_xoshiro::Xoroshiro128PlusPlus;
use serde::{Deserialize, Serialize};

use crate::{OpMode, State};

#[derive(Deserialize, Serialize)]
pub struct TTSResponse {
    success: bool,
    url: String,
}

#[derive(Deserialize, Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct TTSRequest {
    engine: String,
    fail_sim: Option<String>,
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
    match state.config.tts.op_mode {
        OpMode::Proxy => Json(
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
        ),
        OpMode::Native => {
            let mut hasher = DefaultHasher::default();
            hasher.write(tts.voice.as_bytes());
            let mut rng = Xoroshiro128PlusPlus::seed_from_u64(hasher.finish());

            let voice = glob::glob(&format!(
                "{}/*.onnx",
                &state.config.tts.voices_path.display()
            ))
            .unwrap()
            .choose(&mut rng)
            .unwrap()
            .unwrap();

            let length_scale = 100.0 / tts.rate.trim_matches('%').parse::<f32>().unwrap();
            let out_name = format!("{}.{}", nanoid!(), tts.file_format);
            let out = state.config.tts.tts_dir.join(format!("tts/{}", out_name));

            tokio::fs::create_dir_all(out.parent().unwrap())
                .await
                .unwrap();

            tracing::debug!(?tts, voice = %voice.display(), out = %out.display(), "Generating TTS");
            let mut piper = std::process::Command::new(state.config.tts.piper_bin.as_path())
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .args([
                    "--length-scale",
                    &length_scale.to_string(),
                    "--model",
                    &voice.display().to_string(),
                    "--output-raw",
                ])
                .spawn()
                .unwrap();
            let mut ffmpeg = std::process::Command::new(state.config.tts.ffmpeg_bin.as_path())
                .stdin(piper.stdout.take().unwrap())
                .args([
                    "-f",
                    "s16le",
                    "-ar",
                    "22050",
                    "-i",
                    "pipe:",
                    "-ar",
                    "44100",
                    "-filter:a",
                    "speechnorm",
                    &out.display().to_string(),
                ])
                .spawn()
                .unwrap();

            piper
                .stdin
                .take()
                .unwrap()
                .write_all(tts.text.as_bytes())
                .unwrap();

            ffmpeg.wait().unwrap();

            Json(TTSResponse {
                success: true,
                url: format!("https://{}/tts/{}", state.config.accessible_host, out_name),
            })
        }
    }
}
