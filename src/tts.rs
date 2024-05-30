use std::{
    hash::{DefaultHasher, Hasher},
    io::Write,
    process::Stdio,
};

use axum::Json;
use color_eyre::eyre::{eyre, Context, OptionExt};
use nanoid::nanoid;
use rand::{seq::IteratorRandom, SeedableRng};
use rand_xoshiro::Xoroshiro128PlusPlus;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use tracing::instrument;

use crate::{
    error::{PropogateRequest, WithStatusCode},
    OpMode, State,
};

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

#[instrument(skip(state))]
pub async fn generate_handler(
    axum::extract::State(state): axum::extract::State<State>,
    Json(tts): Json<TTSRequest>,
) -> crate::error::Result<Json<TTSResponse>> {
    match state.config.tts.op_mode {
        OpMode::Proxy => Ok(Json(
            state
                .http_cache
                .client
                .post("https://blobcast.jackboxgames.com/tts/generate")
                .json(&tts)
                .send()
                .await
                .wrap_err("Failed to request TTS from blobcast")
                .with_status_code(StatusCode::INTERNAL_SERVER_ERROR)?
                .propogate_request_if_err()?
                .json()
                .await
                .wrap_err("Failed to convert to TTS response from JSON")
                .with_status_code(StatusCode::INTERNAL_SERVER_ERROR)?,
        )),
        OpMode::Native => {
            let mut hasher = DefaultHasher::default();
            hasher.write(tts.voice.as_bytes());
            let mut rng = Xoroshiro128PlusPlus::seed_from_u64(hasher.finish());

            let voice_glob = format!("{}/*.onnx", &state.config.tts.voices_path.display());
            let voice = glob::glob(&voice_glob)
                .wrap_err_with(|| format!("Failed to parse glob from voices path: {}", voice_glob))
                .with_status_code(StatusCode::UNPROCESSABLE_ENTITY)?
                .choose(&mut rng)
                .ok_or_else(|| eyre!("Failed to find a voice from glob: {}", voice_glob))
                .with_status_code(StatusCode::NOT_FOUND)?
                .wrap_err_with(|| {
                    format!(
                        "Path grabbed from random choice from glob was not valid: {}",
                        voice_glob
                    )
                })
                .with_status_code(StatusCode::INTERNAL_SERVER_ERROR)?;

            let length_scale = 100.0
                / tts
                    .rate
                    .trim_matches('%')
                    .parse::<f32>()
                    .wrap_err_with(|| {
                        format!(
                            "`{}` is not a valid floating point number",
                            tts.rate.trim_matches('%')
                        )
                    })
                    .with_status_code(StatusCode::UNPROCESSABLE_ENTITY)?;
            let out_name = format!("{}.{}", nanoid!(), tts.file_format);
            let out = state.config.tts.tts_dir.join(format!("tts/{}", out_name));

            let out_dir = out
                .parent()
                .ok_or_else(|| eyre!("The file `{}` has no parent", out.display()))
                .with_status_code(StatusCode::INTERNAL_SERVER_ERROR)?;
            tokio::fs::create_dir_all(out_dir)
                .await
                .wrap_err_with(|| {
                    format!("The directory `{}` could not be created", out_dir.display())
                })
                .with_status_code(StatusCode::INTERNAL_SERVER_ERROR)?;

            tracing::debug!(voice = %voice.display(), out = %out.display(), "Generating TTS");
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
                .wrap_err_with(|| {
                    format!("Could not spawn `{}`", state.config.tts.piper_bin.display())
                })
                .with_status_code(StatusCode::INTERNAL_SERVER_ERROR)?;
            let mut ffmpeg = std::process::Command::new(state.config.tts.ffmpeg_bin.as_path())
                .stdin(
                    piper
                        .stdout
                        .take()
                        .ok_or_eyre("piper has no stdout")
                        .with_status_code(StatusCode::INTERNAL_SERVER_ERROR)?,
                )
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
                .wrap_err_with(|| {
                    format!(
                        "Could not spawn `{}`",
                        state.config.tts.ffmpeg_bin.display()
                    )
                })
                .with_status_code(StatusCode::INTERNAL_SERVER_ERROR)?;

            piper
                .stdin
                .take()
                .ok_or_eyre("piper has no stdin")
                .with_status_code(StatusCode::INTERNAL_SERVER_ERROR)?
                .write_all(tts.text.as_bytes())
                .unwrap();

            ffmpeg
                .wait()
                .wrap_err("ffmpeg command failed")
                .with_status_code(StatusCode::INTERNAL_SERVER_ERROR)?;

            Ok(Json(TTSResponse {
                success: true,
                url: format!("https://{}/tts/{}", state.config.accessible_host, out_name),
            }))
        }
    }
}
