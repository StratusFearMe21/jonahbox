use std::io::{BufReader, BufWriter};
use std::ops::DerefMut;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::Arc;

use async_compression::brotli::EncoderParams;
use async_compression::tokio::bufread::{BrotliDecoder, BrotliEncoder};
use async_compression::tokio::write;
use axum::http::uri::{Authority, Scheme};
use axum::http::HeaderMap;
use axum::http::{uri, HeaderValue};
use axum::response::{IntoResponse, Response};
use color_eyre::eyre::{eyre, Context, OptionExt};
use futures_util::TryStreamExt;
use helix_core::syntax::RopeProvider;
use helix_core::{Rope, RopeBuilder};
use helix_lsp::lsp::TextEdit;
use helix_stdx::rope::{Regex, RopeSliceExt};
use regex_cursor::Input;
use reqwest::header::{ACCEPT_ENCODING, CONTENT_ENCODING, HOST, IF_MODIFIED_SINCE, IF_NONE_MATCH};
use reqwest::header::{CONTENT_TYPE, ETAG};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::{io::AsyncBufReadExt, sync::Mutex};
use tokio_util::io::StreamReader;
use tracing::instrument;
use tree_sitter::QueryCursor;

use crate::error::{PropogateRequest, WithStatusCode};
use crate::CacheMode;

#[derive(Clone)]
pub struct HttpCache {
    pub client: reqwest::Client,
    pub ts_parser: Arc<Mutex<(tree_sitter::Parser, QueryCursor)>>,
    pub js_lang: Arc<(tree_sitter::Language, tree_sitter::Query)>,
    pub css_lang: Arc<(tree_sitter::Language, tree_sitter::Query)>,
    pub regexes: Arc<Regexes>,
}

pub struct Regexes {
    pub content_to_compress: Regex,
    pub jackbox_urls: Regex,
}

#[derive(Deserialize, Serialize)]
struct JBHttpResponse {
    etag: String,
    content_type: Option<String>,
    compressed: bool,
}

impl JBHttpResponse {
    fn from_request(
        value: &reqwest::Response,
        content_to_compress: &Regex,
    ) -> crate::error::Result<Self> {
        Ok(Self {
            etag: value
                .headers()
                .get(ETAG)
                .ok_or_eyre("Etag was not present in response")
                .with_status_code(StatusCode::INTERNAL_SERVER_ERROR)?
                .to_str()
                .wrap_err("Etag was not valid UTF-8")
                .with_status_code(StatusCode::UNPROCESSABLE_ENTITY)?
                .to_owned(),
            content_type: {
                if let Some(content_type) = value.headers().get(CONTENT_TYPE) {
                    let content_type = content_type
                        .to_str()
                        .wrap_err("Content-Type was not valid UTF-8")
                        .with_status_code(StatusCode::UNPROCESSABLE_ENTITY)?;

                    Some(content_type.to_owned())
                } else {
                    None
                }
            },
            compressed: value
                .headers()
                .get(CONTENT_TYPE)
                .is_some_and(|ct| content_to_compress.is_match(Input::new(ct.as_bytes()))),
        })
    }

    fn headers(&self) -> crate::error::Result<HeaderMap> {
        let mut headers = HeaderMap::new();
        headers.insert(
            ETAG,
            self.etag
                .as_str()
                .try_into()
                .wrap_err("Failed to convert Etag from String to HeaderValue")
                .with_status_code(StatusCode::INTERNAL_SERVER_ERROR)?,
        );
        if let Some(ref ct) = self.content_type {
            headers.insert(
                CONTENT_TYPE,
                ct.as_str()
                    .try_into()
                    .wrap_err("Failed to convert Content-Type from String to HeaderValue")
                    .with_status_code(StatusCode::INTERNAL_SERVER_ERROR)?,
            );
        }
        Ok(headers)
    }
}

impl HttpCache {
    #[instrument(skip(self, headers, uri))]
    pub async fn get_cached(
        &self,
        mut uri: uri::Uri,
        mut headers: HeaderMap,
        cache_mode: CacheMode,
        accessible_host: &str,
        cache_path: &Path,
    ) -> crate::error::Result<Response> {
        let mut uri_parts = uri.into_parts();
        if uri_parts
            .path_and_query
            .as_ref()
            .map(|pq| pq.path().starts_with("/@"))
            .unwrap_or_default()
        {
            let path = uri_parts.path_and_query.as_ref().unwrap().as_str();

            let path_split = path.split_once('@').unwrap().1;
            let path_split = path_split.split_once('/').unwrap_or((path_split, ""));
            let host = path_split.0;
            if !self.regexes.jackbox_urls.is_match(Input::new(host)) {
                return Err(eyre!("Proxy can only be used with Jackbox services"))
                    .with_status_code(StatusCode::BAD_REQUEST);
            }
            uri_parts.authority = Some(
                host.to_owned()
                    .try_into()
                    .wrap_err_with(|| format!("Failed to convert host `{}` into Authority", host))
                    .with_status_code(StatusCode::INTERNAL_SERVER_ERROR)?,
            );
            headers.insert(
                HOST,
                host.to_owned()
                    .try_into()
                    .wrap_err_with(|| format!("Failed to convert host `{}` into HeaderValue", host))
                    .with_status_code(StatusCode::INTERNAL_SERVER_ERROR)?,
            );
            let path_and_query = format!("/{}", path_split.1);
            *uri_parts.path_and_query.as_mut().unwrap() = path_and_query
                .as_str()
                .try_into()
                .wrap_err_with(|| {
                    format!(
                        "Failed to convert `{}` to a URI path & query",
                        path_and_query
                    )
                })
                .with_status_code(StatusCode::INTERNAL_SERVER_ERROR)?;
        } else {
            uri_parts.authority = Some(Authority::from_static("jackbox.tv"));
            headers.insert(HOST, HeaderValue::from_static("jackbox.tv"));
        }
        uri_parts.scheme = Some(Scheme::HTTPS);
        uri = uri::Uri::from_parts(uri_parts)
            .wrap_err("URI parts did not make a valid URI")
            .with_status_code(StatusCode::INTERNAL_SERVER_ERROR)?;

        headers.remove(IF_MODIFIED_SINCE);

        let br = headers
            .get(ACCEPT_ENCODING)
            .as_ref()
            .map(|e| e.to_str())
            .into_iter()
            .flatten()
            .flat_map(|e| e.split(','))
            .any(|s| s.trim() == "br");

        let cached_resource_raw = cache_path.join(format!(
            "{}/{}",
            uri.host()
                .ok_or_else(|| eyre!("URI `{}` did not have a host", uri))
                .with_status_code(StatusCode::INTERNAL_SERVER_ERROR)?,
            if uri.path() == "/" {
                "index.html"
            } else {
                uri.path()
            }
        ));
        let mut reqwest_resp = if matches!(cache_mode, CacheMode::Offline)
            || (matches!(cache_mode, CacheMode::Oneshot) && cached_resource_raw.exists())
        {
            None
        } else {
            Some(
                self.client
                    .get(format!("{}", uri))
                    .headers(headers.clone())
                    .send()
                    .await
                    .wrap_err_with(|| format!("Failed to acquire resource from `{}`", uri))
                    .with_status_code(StatusCode::INTERNAL_SERVER_ERROR)?
                    .propogate_request_if_err()?,
            )
        };
        let mut resp = if let Some(ref resp) = reqwest_resp {
            JBHttpResponse::from_request(resp, &self.regexes.content_to_compress)?
        } else {
            serde_json::from_reader(BufReader::new(
                std::fs::File::open(&cached_resource_raw)
                    .wrap_err_with(|| {
                        format!(
                            "The offline resource `{}` could not be found",
                            cached_resource_raw.display()
                        )
                    })
                    .with_status_code(StatusCode::NOT_FOUND)?,
            ))
            .wrap_err_with(|| {
                format!(
                    "The offline resource `{}` could not be deserialized",
                    cached_resource_raw.display()
                )
            })
            .with_status_code(StatusCode::INTERNAL_SERVER_ERROR)?
        };

        let mut cached_resource = if resp.compressed {
            cache_path.join(format!("{}/{}.br", uri.host().unwrap(), resp.etag))
        } else {
            cache_path.join(format!("{}/{}", uri.host().unwrap(), resp.etag))
        };

        let part_path = cached_resource.with_extension("part.br");
        let cached_resource_dir = cached_resource
            .parent()
            .ok_or_else(|| {
                eyre!(
                    "The cached resource `{}` has no parent directory",
                    cached_resource.display()
                )
            })
            .with_status_code(StatusCode::INTERNAL_SERVER_ERROR)?;
        tokio::fs::create_dir_all(cached_resource_dir)
            .await
            .wrap_err_with(|| {
                format!(
                    "The directory `{}` could not be created for the cached resource",
                    cached_resource_dir.display()
                )
            })
            .with_status_code(StatusCode::INTERNAL_SERVER_ERROR)?;

        if reqwest_resp
            .as_ref()
            .map(|r| r.status() == StatusCode::NOT_MODIFIED)
            .unwrap_or(true)
            && !cached_resource.exists()
        {
            headers.remove(IF_NONE_MATCH);
            reqwest_resp = if matches!(cache_mode, CacheMode::Online) {
                None
            } else {
                Some(
                    self.client
                        .get(format!("{}", uri))
                        .headers(headers.clone())
                        .send()
                        .await
                        .wrap_err_with(|| {
                            format!(
                                "Failed to acquire resource from `{}` (with stripped Etag)",
                                uri
                            )
                        })
                        .with_status_code(StatusCode::INTERNAL_SERVER_ERROR)?
                        .propogate_request_if_err()?,
                )
            };
            resp = if let Some(ref resp) = reqwest_resp {
                JBHttpResponse::from_request(resp, &self.regexes.content_to_compress)?
            } else {
                serde_json::from_reader(BufReader::new(
                    std::fs::File::open(&cached_resource_raw)
                        .wrap_err_with(|| {
                            format!(
                                "The offline resource `{}` could not be found (stripped Etag)",
                                cached_resource_raw.display()
                            )
                        })
                        .with_status_code(StatusCode::NOT_FOUND)?,
                ))
                .wrap_err_with(|| {
                    format!(
                        "The offline resource `{}` could not be deserialized (stripped Etag)",
                        cached_resource_raw.display()
                    )
                })
                .with_status_code(StatusCode::INTERNAL_SERVER_ERROR)?
            };
            cached_resource = if resp.compressed {
                cache_path.join(format!("{}/{}.br", uri.host().unwrap(), resp.etag))
            } else {
                cache_path.join(format!("{}/{}", uri.host().unwrap(), resp.etag))
            };
        }

        if !cached_resource.exists() {
            let mut part_file = fd_lock::RwLock::new(
                tokio::fs::OpenOptions::new()
                    .create(true)
                    .write(true)
                    .open(&part_path)
                    .await
                    .wrap_err_with(|| {
                        format!(
                            "Failed to open/create part file for resource `{}`",
                            part_path.display()
                        )
                    })
                    .with_status_code(StatusCode::INTERNAL_SERVER_ERROR)?,
            );
            let mut part_file_w = part_file
                .write()
                .wrap_err_with(|| {
                    format!(
                        "Failed to open part file for writing for resource `{}`",
                        part_path.display()
                    )
                })
                .with_status_code(StatusCode::INTERNAL_SERVER_ERROR)?;
            if reqwest_resp
                .as_ref()
                .map(|r| r.status() == StatusCode::NOT_MODIFIED)
                .unwrap_or(true)
                || cached_resource.exists()
            {
                drop(part_file_w);
                let _ = tokio::fs::remove_file(&part_path).await;
            } else {
                match resp.content_type.as_ref().map(|ct| ct.as_ref()) {
                    Some("text/javascript") | Some("application/json") => {
                        let mut rope = rope_from_reader(StreamReader::new(
                            reqwest_resp.unwrap().bytes_stream().map_err(|e| {
                                std::io::Error::new(std::io::ErrorKind::ConnectionReset, e)
                            }),
                        ))
                        .await
                        .wrap_err("Failed to convert JavaScript response byte stream to Rope")
                        .with_status_code(StatusCode::INTERNAL_SERVER_ERROR)?;

                        let mut lock = self.ts_parser.lock().await;
                        let (parser, query_cursor) = lock.deref_mut();

                        parser
                            .set_language(&self.js_lang.0)
                            .wrap_err("Failed to initialize JavaScript parser")
                            .with_status_code(StatusCode::INTERNAL_SERVER_ERROR)?;

                        let tree = parser
                            .parse_with(
                                &mut |byte, _| {
                                    if byte <= rope.len_bytes() {
                                        let (chunk, start_byte, _, _) = rope.chunk_at_byte(byte);
                                        &chunk.as_bytes()[byte - start_byte..]
                                    } else {
                                        // out of range
                                        &[]
                                    }
                                },
                                None,
                            )
                            .ok_or_eyre("Failed to parse JavaScript resource")
                            .with_status_code(StatusCode::INTERNAL_SERVER_ERROR)?;

                        let mut edits: Vec<TextEdit> = Vec::new();
                        for query_match in query_cursor.matches(
                            &self.js_lang.1,
                            tree.root_node(),
                            RopeProvider(rope.slice(..)),
                        ) {
                            let range = query_match.captures[0].node.byte_range();
                            let slice = rope.byte_slice(range.clone());
                            let range =
                                rope.byte_to_char(range.start)..rope.byte_to_char(range.end);
                            for regex_match in
                                self.regexes.jackbox_urls.find_iter(slice.regex_input())
                            {
                                edits.push(TextEdit {
                                    range: helix_lsp::util::range_to_lsp_range(
                                        &rope,
                                        helix_core::selection::Range {
                                            anchor: range.start + regex_match.start(),
                                            head: range.start + regex_match.start(),
                                            // head: (range.start + regex_match.end()),
                                            old_visual_position: None,
                                        },
                                        helix_lsp::OffsetEncoding::Utf8,
                                    ),
                                    new_text: format!("{}/@", accessible_host),
                                });
                            }
                        }

                        let transaction = helix_lsp::util::generate_transaction_from_edits(
                            &rope,
                            edits,
                            helix_lsp::OffsetEncoding::Utf8,
                        );

                        if !transaction.apply(&mut rope) {
                            return Err(eyre!("Failed to patch JavaScript file"))
                                .with_status_code(StatusCode::INTERNAL_SERVER_ERROR);
                        }

                        if resp.compressed {
                            let mut stream = write::BrotliEncoder::with_quality_and_params(
                                tokio::io::BufWriter::new(part_file_w.deref_mut()),
                                async_compression::Level::Best,
                                EncoderParams::default().text_mode(),
                            );

                            for chunk in rope.chunks() {
                                stream
                                    .write_all(chunk.as_bytes())
                                    .await
                                    .wrap_err("Failed to write Rope chunk to brotli compressor")
                                    .with_status_code(StatusCode::INTERNAL_SERVER_ERROR)?;
                            }

                            stream
                                .shutdown()
                                .await
                                .wrap_err("Failed to shutdown brotli compression stream")
                                .with_status_code(StatusCode::INTERNAL_SERVER_ERROR)?;
                        } else {
                            let mut stream = tokio::io::BufWriter::new(part_file_w.deref_mut());

                            for chunk in rope.chunks() {
                                stream
                                    .write_all(chunk.as_bytes())
                                    .await
                                    .wrap_err("Failed to write Rope chunk to uncompressed stream")
                                    .with_status_code(StatusCode::INTERNAL_SERVER_ERROR)?;
                            }

                            stream
                                .shutdown()
                                .await
                                .wrap_err("Failed to shutdown uncompressed stream")
                                .with_status_code(StatusCode::INTERNAL_SERVER_ERROR)?;
                        }
                    }
                    Some("text/css") => {
                        let mut rope = rope_from_reader(StreamReader::new(
                            reqwest_resp.unwrap().bytes_stream().map_err(|e| {
                                std::io::Error::new(std::io::ErrorKind::ConnectionReset, e)
                            }),
                        ))
                        .await
                        .wrap_err("Failed to convert CSS response byte stream to Rope")
                        .with_status_code(StatusCode::INTERNAL_SERVER_ERROR)?;

                        let mut lock = self.ts_parser.lock().await;
                        let (parser, query_cursor) = lock.deref_mut();

                        parser
                            .set_language(&self.css_lang.0)
                            .wrap_err("Failed to initialize JavaScript parser")
                            .with_status_code(StatusCode::INTERNAL_SERVER_ERROR)?;

                        let tree = parser
                            .parse_with(
                                &mut |byte, _| {
                                    if byte <= rope.len_bytes() {
                                        let (chunk, start_byte, _, _) = rope.chunk_at_byte(byte);
                                        &chunk.as_bytes()[byte - start_byte..]
                                    } else {
                                        // out of range
                                        &[]
                                    }
                                },
                                None,
                            )
                            .ok_or_eyre("Failed to parse CSS resource")
                            .with_status_code(StatusCode::INTERNAL_SERVER_ERROR)?;

                        let mut edits: Vec<TextEdit> = Vec::new();
                        for query_match in query_cursor.matches(
                            &self.css_lang.1,
                            tree.root_node(),
                            RopeProvider(rope.slice(..)),
                        ) {
                            let range = query_match.captures[0].node.byte_range();
                            let slice = rope.byte_slice(range.clone());
                            let range =
                                rope.byte_to_char(range.start)..rope.byte_to_char(range.end);
                            for regex_match in
                                self.regexes.jackbox_urls.find_iter(slice.regex_input())
                            {
                                edits.push(TextEdit {
                                    range: helix_lsp::util::range_to_lsp_range(
                                        &rope,
                                        helix_core::selection::Range {
                                            anchor: range.start + regex_match.start(),
                                            head: range.start + regex_match.start(),
                                            // head: (range.start + regex_match.end()),
                                            old_visual_position: None,
                                        },
                                        helix_lsp::OffsetEncoding::Utf8,
                                    ),
                                    new_text: format!("{}/@", accessible_host),
                                });
                            }
                        }

                        let transaction = helix_lsp::util::generate_transaction_from_edits(
                            &rope,
                            edits,
                            helix_lsp::OffsetEncoding::Utf8,
                        );

                        if !transaction.apply(&mut rope) {
                            return Err(eyre!("Failed to patch CSS file"))
                                .with_status_code(StatusCode::INTERNAL_SERVER_ERROR);
                        }

                        if resp.compressed {
                            let mut stream = write::BrotliEncoder::with_quality_and_params(
                                tokio::io::BufWriter::new(part_file_w.deref_mut()),
                                async_compression::Level::Best,
                                EncoderParams::default().text_mode(),
                            );

                            for chunk in rope.chunks() {
                                stream
                                    .write_all(chunk.as_bytes())
                                    .await
                                    .wrap_err("Failed to write Rope chunk to brotli compressor")
                                    .with_status_code(StatusCode::INTERNAL_SERVER_ERROR)?;
                            }

                            stream
                                .shutdown()
                                .await
                                .wrap_err("Failed to shutdown brotli compression stream")
                                .with_status_code(StatusCode::INTERNAL_SERVER_ERROR)?;
                        } else {
                            let mut stream = tokio::io::BufWriter::new(part_file_w.deref_mut());

                            for chunk in rope.chunks() {
                                stream
                                    .write_all(chunk.as_bytes())
                                    .await
                                    .wrap_err("Failed to write Rope chunk to uncompressed stream")
                                    .with_status_code(StatusCode::INTERNAL_SERVER_ERROR)?;
                            }

                            stream
                                .shutdown()
                                .await
                                .wrap_err("Failed to shutdown uncompressed stream")
                                .with_status_code(StatusCode::INTERNAL_SERVER_ERROR)?;
                        }
                    }
                    _ if resp.compressed => {
                        let mut stream = BrotliEncoder::with_quality(
                            StreamReader::new(reqwest_resp.unwrap().bytes_stream().map_err(|e| {
                                std::io::Error::new(std::io::ErrorKind::ConnectionReset, e)
                            })),
                            async_compression::Level::Best,
                        );

                        let mut file = tokio::io::BufWriter::new(part_file_w.deref_mut());

                        tokio::io::copy(&mut stream, &mut file)
                            .await
                            .wrap_err("Failed to copy byte stream to a compressed brotli file")
                            .with_status_code(StatusCode::INTERNAL_SERVER_ERROR)?;

                        file.shutdown()
                            .await
                            .wrap_err("Failed to shutdown compressed brotli file stream")
                            .with_status_code(StatusCode::INTERNAL_SERVER_ERROR)?;
                    }
                    _ => {
                        let mut stream =
                            StreamReader::new(reqwest_resp.unwrap().bytes_stream().map_err(|e| {
                                std::io::Error::new(std::io::ErrorKind::ConnectionReset, e)
                            }));
                        let mut file = tokio::io::BufWriter::new(part_file_w.deref_mut());

                        tokio::io::copy(&mut stream, &mut file)
                            .await
                            .wrap_err("Failed to copy byte stream to an uncompressed file")
                            .with_status_code(StatusCode::INTERNAL_SERVER_ERROR)?;

                        file.shutdown()
                            .await
                            .wrap_err("Failed to shutdown uncompressed file stream")
                            .with_status_code(StatusCode::INTERNAL_SERVER_ERROR)?;
                    }
                }
                let cached_resource_raw_dir = cached_resource_raw
                    .parent()
                    .ok_or_else(|| {
                        eyre!(
                            "Raw cached resource `{}` does not have a parent directory",
                            cached_resource_raw.display()
                        )
                    })
                    .with_status_code(StatusCode::INTERNAL_SERVER_ERROR)?;
                tokio::fs::create_dir_all(cached_resource_raw_dir)
                    .await
                    .wrap_err_with(|| {
                        format!(
                            "Failed to create directory for raw resource `{}`",
                            cached_resource_raw_dir.display()
                        )
                    })
                    .with_status_code(StatusCode::INTERNAL_SERVER_ERROR)?;
                serde_json::to_writer(
                    BufWriter::new(
                        std::fs::File::create(&cached_resource_raw)
                            .wrap_err_with(|| {
                                format!(
                                    "Failed to create file for raw resource `{}`",
                                    cached_resource_raw.display()
                                )
                            })
                            .with_status_code(StatusCode::INTERNAL_SERVER_ERROR)?,
                    ),
                    &resp,
                )
                .wrap_err_with(|| {
                    format!(
                        "Failed to serialize raw resource `{}`",
                        cached_resource_raw.display()
                    )
                })
                .with_status_code(StatusCode::INTERNAL_SERVER_ERROR)?;
                tokio::fs::rename(&part_path, &cached_resource)
                    .await
                    .wrap_err_with(|| {
                        format!(
                            "Failed to move part file `{}` into it's cached resource path `{}`",
                            part_path.display(),
                            cached_resource.display()
                        )
                    })
                    .with_status_code(StatusCode::INTERNAL_SERVER_ERROR)?;
            }
        }

        // let status_code = if headers
        //     .get(IF_NONE_MATCH)
        //     .map(|ct| ct.as_bytes() != resp.etag.as_bytes())
        //     .unwrap_or(true)
        // {
        //     StatusCode::OK
        // } else {
        //     StatusCode::NOT_MODIFIED
        // };

        let status_code = StatusCode::OK;

        let mut resp_headers = resp.headers()?;
        let content_type = resp_headers.get(CONTENT_TYPE).cloned();
        if br {
            if resp.compressed {
                resp_headers.insert(CONTENT_ENCODING, HeaderValue::from_static("br"));
            }
            let mut resp = (
                status_code,
                resp_headers,
                tokio::fs::read(&cached_resource)
                    .await
                    .wrap_err_with(|| {
                        format!(
                            "Failed to read cached resource `{}`",
                            cached_resource.display()
                        )
                    })
                    .with_status_code(StatusCode::INTERNAL_SERVER_ERROR)?,
            )
                .into_response();
            if let Some(t) = content_type {
                resp.headers_mut().insert(CONTENT_TYPE, t);
            }
            return Ok(resp);
        } else {
            resp_headers.remove(CONTENT_ENCODING);
            let mut buf = Vec::with_capacity(
                tokio::fs::metadata(&cached_resource)
                    .await
                    .wrap_err_with(|| {
                        format!(
                            "Failed to read metadata for cached resource `{}`",
                            cached_resource.display()
                        )
                    })
                    .with_status_code(StatusCode::INTERNAL_SERVER_ERROR)?
                    .size() as usize,
            );
            let mut resp = if resp.compressed {
                (status_code, resp_headers, {
                    BrotliDecoder::new(tokio::io::BufReader::new(
                        tokio::fs::File::open(&cached_resource)
                            .await
                            .wrap_err_with(|| {
                                format!(
                                    "Failed to open cached resource `{}`",
                                    cached_resource.display()
                                )
                            })
                            .with_status_code(StatusCode::INTERNAL_SERVER_ERROR)?,
                    ))
                    .read_to_end(&mut buf)
                    .await
                    .wrap_err_with(|| {
                        format!(
                            "Failed to decompress cached resource `{}`",
                            cached_resource.display()
                        )
                    })
                    .with_status_code(StatusCode::INTERNAL_SERVER_ERROR)?;
                    buf
                })
                    .into_response()
            } else {
                (status_code, resp_headers, {
                    tokio::io::BufReader::new(
                        tokio::fs::File::open(&cached_resource)
                            .await
                            .wrap_err_with(|| {
                                format!(
                                    "Failed to open cached resource `{}`",
                                    cached_resource.display()
                                )
                            })
                            .with_status_code(StatusCode::INTERNAL_SERVER_ERROR)?,
                    )
                    .read_to_end(&mut buf)
                    .await
                    .wrap_err_with(|| {
                        format!(
                            "Failed to read cached resource `{}`",
                            cached_resource.display()
                        )
                    })
                    .with_status_code(StatusCode::INTERNAL_SERVER_ERROR)?;
                    buf
                })
                    .into_response()
            };
            if let Some(t) = content_type {
                resp.headers_mut().insert(CONTENT_TYPE, t);
            }
            return Ok(resp);
        }
    }
}

async fn rope_from_reader<T: tokio::io::AsyncBufRead>(reader: T) -> std::io::Result<Rope> {
    tokio::pin!(reader);
    let mut builder = RopeBuilder::new();
    let mut off_char: Option<heapless::Vec<u8, 4>> = None;
    let mut off_buff = Vec::new();
    loop {
        match (&mut reader).fill_buf().await {
            Ok(buffer) => {
                let buffer_len = buffer.len();
                if buffer_len == 0 {
                    break;
                }

                let buffer = if let Some(ref off_char) = off_char {
                    off_buff.clear();
                    off_buff.extend_from_slice(&off_char);
                    off_buff.extend_from_slice(buffer);
                    off_buff.as_slice()
                } else {
                    buffer
                };

                // Determine how much of the buffer is valid utf8.
                let valid_count = match std::str::from_utf8(buffer) {
                    Ok(_) => buffer.len(),
                    Err(e) => e.valid_up_to(),
                };

                // Append the valid part of the buffer to the rope.
                if valid_count > 0 {
                    // The unsafe block here is reinterpreting the bytes as
                    // utf8.  This is safe because the bytes being
                    // reinterpreted have already been validated as utf8
                    // just above.
                    builder
                        .append(unsafe { std::str::from_utf8_unchecked(&buffer[..valid_count]) });
                    let invalid = &buffer[valid_count..];

                    off_char = None;
                    if !invalid.is_empty() {
                        let mut buf = heapless::Vec::new();
                        buf.extend_from_slice(invalid).unwrap();
                        off_char = Some(buf);
                    }
                } else {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "stream did not contain valid UTF-8",
                    ));
                }

                // Shift the un-read part of the buffer to the beginning.
                (&mut reader).consume(buffer_len);
            }

            Err(e) => {
                // Read error
                return Err(e);
            }
        }
    }

    return Ok(builder.finish());
}
