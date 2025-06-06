use crate::chat::{ChatChoice, ChatEvent, ChatState};
/// HTTP Server logic
use crate::config::Config;
use crate::infer::{Backend, Infer, InferError, InferResponse, InferStreamResponse};
#[cfg(feature = "kserve")]
use crate::kserve::{
    kerve_server_metadata, kserve_health_live, kserve_health_ready, kserve_model_infer,
    kserve_model_metadata, kserve_model_metadata_ready,
};
use crate::sagemaker::{
    sagemaker_compatibility, SagemakerRequest, SagemakerResponse, SagemakerStreamResponse,
    __path_sagemaker_compatibility,
};
use crate::validation::ValidationError;
use crate::vertex::vertex_compatibility;
use crate::ChatTokenizeResponse;
use crate::{
    usage_stats, BestOfSequence, Details, ErrorResponse, FinishReason, FunctionName,
    GenerateParameters, GenerateRequest, GenerateResponse, GrammarType, HubModelInfo,
    HubProcessorConfig, HubTokenizerConfig, Info, Message, MessageChunk, MessageContent,
    OutputMessage, PrefillToken, SimpleToken, StreamDetails, StreamOptions, StreamResponse,
    TextMessage, Token, TokenizeResponse, Tokenizer, ToolCallDelta, ToolCallMessage, Url, Usage,
    Validation,
};
use crate::{
    ChatCompletion, ChatCompletionChoice, ChatCompletionChunk, ChatCompletionComplete,
    ChatCompletionDelta, ChatCompletionLogprob, ChatCompletionLogprobs, ChatCompletionTopLogprob,
    ChatRequest, Chunk, CompatGenerateRequest, Completion, CompletionComplete, CompletionFinal,
    CompletionRequest, CompletionType, DeltaToolCall, Function, Prompt, Tool,
};
use crate::{FunctionDefinition, HubPreprocessorConfig, ToolCall, ToolChoice};
use crate::{MessageBody, ModelInfo, ModelsInfo};
use async_stream::__private::AsyncStream;
use axum::extract::{DefaultBodyLimit, Extension};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{http, Json, Router};
use axum_tracing_opentelemetry::middleware::OtelAxumLayer;
use futures::stream::StreamExt;
use futures::stream::{FuturesOrdered, FuturesUnordered};
use futures::Stream;
use futures::TryStreamExt;
use hf_hub::api::tokio::{Api, ApiBuilder, ApiRepo};
use hf_hub::{Cache, Repo, RepoType};
use http::header::AUTHORIZATION;
use metrics_exporter_prometheus::{Matcher, PrometheusBuilder, PrometheusHandle};
use pyo3::prelude::*;
use pyo3::types::IntoPyDict;
use std::convert::Infallible;
use std::fs::File;
use std::io::BufReader;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tokio::select;
use tokio::signal;
use tokio::sync::oneshot;
use tokio::time::Instant;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tracing::{info_span, instrument, Instrument};
use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

fn encoding_to_tokens(encoding: &tokenizers::Encoding, input: &str) -> Vec<SimpleToken> {
    let offsets = encoding.get_offsets();
    let input_ids = encoding.get_ids();
    if offsets.len() == input_ids.len() {
        input_ids
            .iter()
            .zip(offsets)
            .map(|(&id, &(start, stop))| {
                let text: Vec<u8> = input.bytes().skip(start).take(stop - start).collect();
                let text: String = String::from_utf8_lossy(&text).to_string();
                SimpleToken {
                    id,
                    text,
                    start,
                    stop,
                }
            })
            .collect()
    } else {
        encoding
            .get_ids()
            .iter()
            .map(|&id| SimpleToken {
                id,
                text: "".to_string(),
                start: 0,
                stop: 0,
            })
            .collect()
    }
}

/// Generate tokens if `stream == false` or a stream of token if `stream == true`
#[utoipa::path(
post,
tag = "Text Generation Inference",
path = "/",
request_body = CompatGenerateRequest,
responses(
(status = 200, description = "Generated Text",
content(
("application/json" = Vec<GenerateResponse>),
("text/event-stream" = StreamResponse),
)),
(status = 424, description = "Generation Error", body = ErrorResponse,
example = json ! ({"error": "Request failed during generation"})),
(status = 429, description = "Model is overloaded", body = ErrorResponse,
example = json ! ({"error": "Model is overloaded"})),
(status = 422, description = "Input validation error", body = ErrorResponse,
example = json ! ({"error": "Input validation error"})),
(status = 500, description = "Incomplete generation", body = ErrorResponse,
example = json ! ({"error": "Incomplete generation"})),
)
)]
#[instrument(skip(infer, req))]
pub(crate) async fn compat_generate(
    Extension(default_return_full_text): Extension<bool>,
    infer: Extension<Infer>,
    compute_type: Extension<ComputeType>,
    Json(mut req): Json<CompatGenerateRequest>,
) -> Result<Response, (StatusCode, Json<ErrorResponse>)> {
    // default return_full_text given the pipeline_tag
    if req.parameters.return_full_text.is_none() {
        req.parameters.return_full_text = Some(default_return_full_text)
    }

    // switch on stream
    if req.stream {
        Ok(generate_stream(infer, compute_type, Json(req.into()))
            .await
            .into_response())
    } else {
        let (headers, Json(generation)) = generate(infer, compute_type, Json(req.into())).await?;
        // wrap generation inside a Vec to match api-inference
        Ok((headers, Json(vec![generation])).into_response())
    }
}

/// Text Generation Inference endpoint info
#[utoipa::path(
get,
tag = "Text Generation Inference",
path = "/info",
responses((status = 200, description = "Served model info", body = Info))
)]
#[instrument]
async fn get_model_info(info: Extension<Info>) -> Json<Info> {
    Json(info.0)
}

#[utoipa::path(
get,
tag = "Text Generation Inference",
path = "/v1/models",
responses(
(status = 200, description = "Served model info", body = ModelInfo),
(status = 404, description = "Model not found", body = ErrorResponse),
)
)]
#[instrument(skip(info))]
/// Get model info
async fn openai_get_model_info(info: Extension<Info>) -> Json<ModelsInfo> {
    Json(ModelsInfo {
        data: vec![ModelInfo {
            id: info.0.model_id.clone(),
            object: "model".to_string(),
            created: 0, // TODO: determine how to get this
            owned_by: info.0.model_id.clone(),
        }],
        ..Default::default()
    })
}

/// Template and tokenize ChatRequest
#[utoipa::path(
    post,
    tag = "Text Generation Inference",
    path = "/chat_tokenize",
    request_body = ChatRequest,
    responses(
    (status = 200, description = "Templated and tokenized ChatRequest", body = ChatTokenizeResponse),
    (status = 404, description = "Failed to tokenize ChatRequest", body = ErrorResponse),
    )
)]
async fn get_chat_tokenize(
    Extension(infer): Extension<Infer>,
    Json(chat): Json<ChatRequest>,
) -> Result<(HeaderMap, Json<ChatTokenizeResponse>), (StatusCode, Json<ErrorResponse>)> {
    metrics::counter!("tgi_request_count").increment(1);

    let generate_request: GenerateRequest = chat.try_into_generate(&infer)?.0;
    let input = generate_request.inputs.clone();
    let encoding = infer.tokenize(generate_request).await?;

    let tokens = encoding_to_tokens(&encoding, &input);

    let resp = ChatTokenizeResponse {
        tokenize_response: TokenizeResponse(tokens),
        templated_text: input,
    };
    Ok((HeaderMap::new(), Json(resp)))
}

#[utoipa::path(
get,
tag = "Text Generation Inference",
path = "/health",
responses(
(status = 200, description = "Everything is working fine"),
(status = 503, description = "Text generation inference is down", body = ErrorResponse,
example = json ! ({"error": "unhealthy", "error_type": "healthcheck"})),
)
)]
#[instrument(skip(infer))]
/// Health check method
async fn health(infer: Extension<Infer>) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    match infer.health().await {
        true => Ok(()),
        false => Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse {
                error: "unhealthy".to_string(),
                error_type: "healthcheck".to_string(),
            }),
        )),
    }
}

/// Generate tokens
#[utoipa::path(
post,
tag = "Text Generation Inference",
path = "/generate",
request_body = GenerateRequest,
responses(
(status = 200, description = "Generated Text", body = GenerateResponse),
(status = 424, description = "Generation Error", body = ErrorResponse,
example = json ! ({"error": "Request failed during generation"})),
(status = 429, description = "Model is overloaded", body = ErrorResponse,
example = json ! ({"error": "Model is overloaded"})),
(status = 422, description = "Input validation error", body = ErrorResponse,
example = json ! ({"error": "Input validation error"})),
(status = 500, description = "Incomplete generation", body = ErrorResponse,
example = json ! ({"error": "Incomplete generation"})),
)
)]
#[instrument(
skip_all,
fields(
parameters = ? req.parameters,
total_time,
validation_time,
queue_time,
inference_time,
time_per_token,
seed,
)
)]
async fn generate(
    infer: Extension<Infer>,
    Extension(ComputeType(compute_type)): Extension<ComputeType>,
    Json(req): Json<GenerateRequest>,
) -> Result<(HeaderMap, Json<GenerateResponse>), (StatusCode, Json<ErrorResponse>)> {
    let span = tracing::Span::current();
    let (headers, _, response) =
        generate_internal(infer, ComputeType(compute_type), Json(req), span).await?;
    Ok((headers, response))
}

pub(crate) async fn generate_internal(
    infer: Extension<Infer>,
    ComputeType(compute_type): ComputeType,
    Json(req): Json<GenerateRequest>,
    span: tracing::Span,
) -> Result<(HeaderMap, u32, Json<GenerateResponse>), (StatusCode, Json<ErrorResponse>)> {
    let start_time = Instant::now();
    metrics::counter!("tgi_request_count").increment(1);

    // Do not long ultra long inputs, like image payloads.
    tracing::debug!(
        "Input: {}",
        &req.inputs.chars().take(1000).collect::<String>()
    );

    let compute_characters = req.inputs.chars().count();
    let mut add_prompt = None;
    if req.parameters.return_full_text.unwrap_or(false) {
        add_prompt = Some(req.inputs.clone());
    }

    let details: bool = req.parameters.details || req.parameters.decoder_input_details;

    // Inference
    let (response, best_of_responses) = match req.parameters.best_of {
        Some(best_of) if best_of > 1 => {
            let (response, best_of_responses) = infer.generate_best_of(req, best_of).await?;
            (response, Some(best_of_responses))
        }
        _ => (infer.generate(req).await?, None),
    };

    // Token details
    let input_length = response._input_length;
    let details = match details {
        true => {
            // convert best_of_responses
            let best_of_sequences = best_of_responses.map(|responses: Vec<InferResponse>| {
                responses
                    .into_iter()
                    .map(|response: InferResponse| {
                        // Add prompt if return_full_text
                        let mut output_text = response.generated_text.text;
                        if let Some(prompt) = &add_prompt {
                            output_text = prompt.clone() + &output_text;
                        }

                        BestOfSequence {
                            generated_text: output_text,
                            finish_reason: response.generated_text.finish_reason,
                            generated_tokens: response.generated_text.generated_tokens,
                            prefill: response.prefill,
                            tokens: response.tokens,
                            top_tokens: response.top_tokens,
                            seed: response.generated_text.seed,
                        }
                    })
                    .collect()
            });

            Some(Details {
                finish_reason: response.generated_text.finish_reason,
                generated_tokens: response.generated_text.generated_tokens,
                prefill: response.prefill,
                tokens: response.tokens,
                seed: response.generated_text.seed,
                best_of_sequences,
                top_tokens: response.top_tokens,
            })
        }
        false => None,
    };

    // Timings
    let total_time = start_time.elapsed();
    let validation_time = response.queued - start_time;
    let queue_time = response.start - response.queued;
    let inference_time = Instant::now() - response.start;
    let time_per_token = inference_time / response.generated_text.generated_tokens;

    // Tracing metadata
    span.record("total_time", format!("{total_time:?}"));
    span.record("validation_time", format!("{validation_time:?}"));
    span.record("queue_time", format!("{queue_time:?}"));
    span.record("inference_time", format!("{inference_time:?}"));
    span.record("time_per_token", format!("{time_per_token:?}"));
    span.record("seed", format!("{:?}", response.generated_text.seed));

    // Headers
    let mut headers = HeaderMap::new();
    headers.insert("x-compute-type", compute_type.parse().unwrap());
    headers.insert(
        "x-compute-time",
        total_time.as_secs_f64().to_string().parse().unwrap(),
    );
    headers.insert(
        "x-compute-characters",
        compute_characters.to_string().parse().unwrap(),
    );
    headers.insert(
        "x-total-time",
        total_time.as_millis().to_string().parse().unwrap(),
    );
    headers.insert(
        "x-validation-time",
        validation_time.as_millis().to_string().parse().unwrap(),
    );
    headers.insert(
        "x-queue-time",
        queue_time.as_millis().to_string().parse().unwrap(),
    );
    headers.insert(
        "x-inference-time",
        inference_time.as_millis().to_string().parse().unwrap(),
    );
    headers.insert(
        "x-time-per-token",
        time_per_token.as_millis().to_string().parse().unwrap(),
    );
    headers.insert("x-prompt-tokens", input_length.into());
    headers.insert(
        "x-generated-tokens",
        response.generated_text.generated_tokens.into(),
    );

    // Metrics
    metrics::counter!("tgi_request_success").increment(1);
    metrics::histogram!("tgi_request_duration").record(total_time.as_secs_f64());
    metrics::histogram!("tgi_request_validation_duration").record(validation_time.as_secs_f64());
    metrics::histogram!("tgi_request_queue_duration").record(queue_time.as_secs_f64());
    metrics::histogram!("tgi_request_inference_duration").record(inference_time.as_secs_f64());
    metrics::histogram!("tgi_request_mean_time_per_token_duration")
        .record(time_per_token.as_secs_f64());
    metrics::histogram!("tgi_request_generated_tokens")
        .record(response.generated_text.generated_tokens as f64);

    // Send response
    let mut output_text = response.generated_text.text;
    if let Some(prompt) = add_prompt {
        output_text = prompt + &output_text;
    }

    tracing::debug!("Output: {}", output_text);
    tracing::info!("Success");

    let response = GenerateResponse {
        generated_text: output_text,
        details,
    };
    Ok((headers, input_length, Json(response)))
}

/// Generate a stream of token using Server-Sent Events
#[utoipa::path(
post,
tag = "Text Generation Inference",
path = "/generate_stream",
request_body = GenerateRequest,
responses(
(status = 200, description = "Generated Text", body = StreamResponse,
content_type = "text/event-stream"),
(status = 424, description = "Generation Error", body = ErrorResponse,
example = json ! ({"error": "Request failed during generation"}),
content_type = "text/event-stream"),
(status = 429, description = "Model is overloaded", body = ErrorResponse,
example = json ! ({"error": "Model is overloaded"}),
content_type = "text/event-stream"),
(status = 422, description = "Input validation error", body = ErrorResponse,
example = json ! ({"error": "Input validation error"}),
content_type = "text/event-stream"),
(status = 500, description = "Incomplete generation", body = ErrorResponse,
example = json ! ({"error": "Incomplete generation"}),
content_type = "text/event-stream"),
)
)]
#[instrument(
skip_all,
fields(
parameters = ? req.parameters,
total_time,
validation_time,
queue_time,
inference_time,
time_per_token,
seed,
)
)]
async fn generate_stream(
    Extension(infer): Extension<Infer>,
    Extension(compute_type): Extension<ComputeType>,
    Json(req): Json<GenerateRequest>,
) -> (
    HeaderMap,
    Sse<impl Stream<Item = Result<Event, Infallible>>>,
) {
    let span = tracing::Span::current();
    let (headers, response_stream) =
        generate_stream_internal(infer, compute_type, Json(req), span).await;

    let response_stream = async_stream::stream! {
        let mut response_stream = Box::pin(response_stream);
        while let Some(raw_event) = response_stream.next().await {
            yield Ok(raw_event.map_or_else(Event::from, |token| {
                Event::default()
                    .json_data(token)
                    .unwrap_or_else(|e| InferError::StreamSerializationError(e.to_string()).into())
            }));
        }
    };

    let sse = Sse::new(response_stream).keep_alive(KeepAlive::default());
    (headers, sse)
}

async fn generate_stream_internal(
    infer: Infer,
    ComputeType(compute_type): ComputeType,
    Json(req): Json<GenerateRequest>,
    span: tracing::Span,
) -> (
    HeaderMap,
    impl Stream<Item = Result<StreamResponse, InferError>>,
) {
    let start_time = Instant::now();
    metrics::counter!("tgi_request_count").increment(1);

    tracing::debug!("Input: {}", req.inputs);

    let compute_characters = req.inputs.chars().count();

    let mut headers = HeaderMap::new();
    headers.insert("x-compute-type", compute_type.parse().unwrap());
    headers.insert(
        "x-compute-characters",
        compute_characters.to_string().parse().unwrap(),
    );
    headers.insert("X-Accel-Buffering", "no".parse().unwrap());

    let stream = async_stream::stream! {
        // Inference
        let mut end_reached = false;
        let mut error = false;

        let mut add_prompt = None;
        if req.parameters.return_full_text.unwrap_or(false) {
            add_prompt = Some(req.inputs.clone());
        }
        let details = req.parameters.details;

        let best_of = req.parameters.best_of.unwrap_or(1);
        if best_of != 1 {
            let err = InferError::from(ValidationError::BestOfStream);
            metrics::counter!("tgi_request_failure", "err" => "validation").increment(1);
            tracing::error!("{err}");
            yield Err(err);
        } else if req.parameters.decoder_input_details {
            let err = InferError::from(ValidationError::PrefillDetailsStream);
            metrics::counter!("tgi_request_failure", "err" => "validation").increment(1);
            tracing::error!("{err}");
            yield Err(err);
        } else {
            match infer.generate_stream(req).instrument(info_span!(parent: &span, "async_stream")).await {
                // Keep permit as long as generate_stream lives
                Ok((_permit, input_length, response_stream)) => {
                    let mut index = 0;
                    let mut response_stream = Box::pin(response_stream);
                    // Server-Sent Event stream
                    while let Some(response) = response_stream.next().await {
                        index += 1;
                        match response {
                            Ok(response) => {
                                match response {
                                    // Prefill is ignored
                                    InferStreamResponse::Prefill(_) => {}
                                    // Yield event for every new token
                                    InferStreamResponse::Intermediate{
                                        token,
                                        top_tokens,
                                    } => {
                                        tracing::debug!(parent: &span, "Token: {:?}", token);

                                        // StreamResponse
                                        let stream_token = StreamResponse {
                                            index,
                                            token,
                                            top_tokens,
                                            generated_text: None,
                                            details: None,
                                        };
                                        yield Ok(stream_token);
                                    }
                                    // Yield event for last token and compute timings
                                    InferStreamResponse::End {
                                        token,
                                        generated_text,
                                        start,
                                        queued,
                                        top_tokens,
                                    } => {
                                        // Token details
                                        let details = match details {
                                            true => Some(StreamDetails {
                                                finish_reason: generated_text.finish_reason,
                                                generated_tokens: generated_text.generated_tokens,
                                                seed: generated_text.seed,
                                                input_length,
                                            }),
                                            false => None,
                                        };

                                        // Timings
                                        let total_time = start_time.elapsed();
                                        let validation_time = queued - start_time;
                                        let queue_time = start - queued;
                                        let inference_time = Instant::now() - start;
                                        let time_per_token = inference_time / generated_text.generated_tokens;

                                        // Tracing metadata
                                        span.record("total_time", format!("{total_time:?}"));
                                        span.record("validation_time", format!("{validation_time:?}"));
                                        span.record("queue_time", format!("{queue_time:?}"));
                                        span.record("inference_time", format!("{inference_time:?}"));
                                        span.record("time_per_token", format!("{time_per_token:?}"));
                                        span.record("seed", format!("{:?}", generated_text.seed));

                                        // Metrics
                                        metrics::counter!("tgi_request_success").increment(1);
                                        metrics::histogram!("tgi_request_duration").record(total_time.as_secs_f64());
                                        metrics::histogram!("tgi_request_validation_duration").record(validation_time.as_secs_f64());
                                        metrics::histogram!("tgi_request_queue_duration").record(queue_time.as_secs_f64());
                                        metrics::histogram!("tgi_request_inference_duration").record(inference_time.as_secs_f64());
                                        metrics::histogram!("tgi_request_mean_time_per_token_duration").record(time_per_token.as_secs_f64());
                                        metrics::histogram!("tgi_request_generated_tokens").record(generated_text.generated_tokens as f64);

                                        // StreamResponse
                                        end_reached = true;

                                        let mut output_text = generated_text.text;
                                        if let Some(prompt) = add_prompt {
                                            output_text = prompt + &output_text;
                                        }

                                        tracing::debug!(parent: &span, "Output: {}", output_text);
                                        tracing::info!(parent: &span, "Success");

                                        let stream_token = StreamResponse {
                                            index,
                                            token,
                                            top_tokens,
                                            generated_text: Some(output_text),
                                            details
                                        };

                                        yield Ok(stream_token);
                                        break;
                                    }
                                }
                            }
                            // yield error
                            Err(err) => {
                                error = true;
                                yield Err(err);
                                break;
                            }
                        }
                    }
                },
                // yield error
                Err(err) => {
                    error = true;
                    yield Err(err);
                }
            }
            // Check if generation reached the end
            // Skip if we already sent an error
            if !end_reached && !error {
                let err = InferError::IncompleteGenerationStream;
                metrics::counter!("tgi_request_failure", "err" => "incomplete").increment(1);
                tracing::error!("{err}");
                yield Err(err);
            }
        }
    };

    (headers, stream)
}

/// Generate tokens
#[utoipa::path(
post,
tag = "Text Generation Inference",
path = "/v1/completions",
request_body = CompletionRequest,
responses(
(status = 200, description = "Generated Chat Completion",
content(
("application/json" = CompletionFinal),
("text/event-stream" = Chunk),
)),
(status = 424, description = "Generation Error", body = ErrorResponse,
example = json ! ({"error": "Request failed during generation"})),
(status = 429, description = "Model is overloaded", body = ErrorResponse,
example = json ! ({"error": "Model is overloaded"})),
(status = 422, description = "Input validation error", body = ErrorResponse,
example = json ! ({"error": "Input validation error"})),
(status = 500, description = "Incomplete generation", body = ErrorResponse,
example = json ! ({"error": "Incomplete generation"})),
)
)]
#[instrument(
skip_all,
fields(
// parameters = ? req.parameters,
total_time,
validation_time,
queue_time,
inference_time,
time_per_token,
seed,
)
)]
pub(crate) async fn completions(
    Extension(infer): Extension<Infer>,
    Extension(compute_type): Extension<ComputeType>,
    Extension(info): Extension<Info>,
    Json(req): Json<CompletionRequest>,
) -> Result<Response, (StatusCode, Json<ErrorResponse>)> {
    let span = tracing::Span::current();
    metrics::counter!("tgi_request_count").increment(1);

    let CompletionRequest {
        model,
        max_tokens,
        seed,
        stop,
        stream,
        temperature,
        ..
    } = req;

    let max_new_tokens = max_tokens;
    let stop = stop.unwrap_or_default();
    // enable greedy only when temperature is 0
    let (do_sample, temperature) = match temperature {
        Some(0.0) => (false, None),
        other => (true, other),
    };

    // if suffix is present throw an error
    if req.suffix.is_some() {
        metrics::counter!("tgi_request_failure", "err" => "validation").increment(1);
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(ErrorResponse {
                error: "Suffix is not supported and can be achieved by preprocessing the prompt."
                    .to_string(),
                error_type: "suffix not supported".to_string(),
            }),
        ));
    }

    if req.prompt.0.len() > info.max_client_batch_size {
        metrics::counter!("tgi_request_failure", "err" => "validation").increment(1);
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(ErrorResponse {
                error: format!(
                    "Number of prompts exceeds the maximum allowed batch size of {}",
                    info.max_client_batch_size
                ),
                error_type: "batch size exceeded".to_string(),
            }),
        ));
    }

    let generate_requests: Vec<GenerateRequest> = req
        .prompt
        .0
        .iter()
        .map(|prompt| GenerateRequest {
            inputs: prompt.to_string(),
            add_special_tokens: true,
            parameters: GenerateParameters {
                best_of: None,
                temperature,
                repetition_penalty: req.repetition_penalty,
                frequency_penalty: req.frequency_penalty,
                top_k: None,
                top_p: req.top_p,
                typical_p: None,
                do_sample,
                max_new_tokens,
                return_full_text: None,
                stop: stop.clone(),
                truncate: None,
                watermark: false,
                details: true,
                decoder_input_details: !stream,
                seed,
                top_n_tokens: None,
                grammar: None,
                adapter_id: model.as_ref().filter(|m| *m != "tgi").map(String::from),
            },
        })
        .collect();

    let mut x_compute_type = None;
    let mut x_compute_characters = 0u32;
    let mut x_accel_buffering = None;

    if stream {
        let mut response_streams = FuturesOrdered::new();
        for (index, generate_request) in generate_requests.into_iter().enumerate() {
            let model_id = info.model_id.clone();
            let system_fingerprint =
                format!("{}-{}", info.version, info.docker_label.unwrap_or("native"));
            let infer_clone = infer.clone();
            let compute_type_clone = compute_type.clone();
            let span_clone = span.clone();

            // Create a future for each generate_stream_internal call.
            let generate_future = async move {
                let (header_tx, header_rx) = oneshot::channel();
                let (sse_tx, sse_rx) = tokio::sync::mpsc::unbounded_channel();

                tokio::spawn(async move {
                    let (headers, response_stream) = generate_stream_internal(
                        infer_clone.clone(),
                        compute_type_clone.clone(),
                        Json(generate_request),
                        span_clone.clone(),
                    )
                    .await;

                    let response_stream = async_stream::stream! {
                        let mut response_stream = Box::pin(response_stream);

                        while let Some(stream_token) = response_stream.next().await {
                            match stream_token {
                                Ok(stream_token) => {
                                    let event = Event::default();

                                    let current_time = std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .unwrap_or_else(|_| std::time::Duration::from_secs(0))
                                        .as_secs();

                                    let message = match stream_token.details {
                                        Some(details) => {
                                            let completion_tokens = details.generated_tokens;
                                            let prompt_tokens = details.input_length;
                                            let total_tokens = prompt_tokens + completion_tokens;

                                            Completion::Final(CompletionFinal {
                                                id: String::new(),
                                                created: current_time,
                                                model: model_id.clone(),
                                                system_fingerprint: system_fingerprint.clone(),
                                                choices: vec![CompletionComplete {
                                                    finish_reason: details.finish_reason.to_string(),
                                                    index: index as u32,
                                                    logprobs: None,
                                                    text: stream_token.token.text,
                                                }],
                                                usage: Usage {
                                                    prompt_tokens,
                                                    completion_tokens,
                                                    total_tokens,
                                                },
                                            })
                                        }
                                        None => Completion::Chunk(Chunk {
                                            id: String::new(),
                                            created: current_time,
                                            choices: vec![CompletionComplete {
                                                finish_reason: String::new(),
                                                index: index as u32,
                                                logprobs: None,
                                                text: stream_token.token.text,
                                            }],
                                            model: model_id.clone(),
                                            system_fingerprint: system_fingerprint.clone(),
                                        }),
                                    };

                                    let event = event
                                        .json_data(message)
                                        .unwrap_or_else(|_e| Event::default());

                                    yield Ok(event);
                                }
                                Err(err) => yield Ok(err.into_openai_event()),
                            }
                        }
                    };

                    // send and dont wait for response
                    let _ = header_tx.send(headers);

                    // pin an emit messages to the sse_tx
                    let mut sse = Box::pin(response_stream);
                    while let Some(event) = sse.next().await {
                        if sse_tx.send(event).is_err() {
                            tracing::error!("Failed to send event. Receiver dropped.");
                            break;
                        }
                    }
                });

                (header_rx, sse_rx)
            };
            response_streams.push_back(generate_future);
        }

        let mut all_rxs = vec![];

        while let Some((header_rx, sse_rx)) = response_streams.next().await {
            all_rxs.push(sse_rx);

            // get the headers from the first response of each stream
            let headers = header_rx.await.map_err(|e| {
                tracing::error!("Failed to get headers: {:?}", e);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ErrorResponse {
                        error: "Failed to get headers".to_string(),
                        error_type: "headers".to_string(),
                    }),
                )
            })?;
            if x_compute_type.is_none() {
                x_compute_type = headers
                    .get("x-compute-type")
                    .and_then(|v| v.to_str().ok())
                    .map(|v| v.to_string());

                x_accel_buffering = headers
                    .get("x-accel-buffering")
                    .and_then(|v| v.to_str().ok())
                    .map(|v| v.to_string());
            }
            x_compute_characters += headers
                .get("x-compute-characters")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
        }

        let mut headers = HeaderMap::new();
        if let Some(x_compute_type) = x_compute_type {
            headers.insert("x-compute-type", x_compute_type.parse().unwrap());
        }
        headers.insert("x-compute-characters", x_compute_characters.into());
        if let Some(x_accel_buffering) = x_accel_buffering {
            headers.insert("x-accel-buffering", x_accel_buffering.parse().unwrap());
        }

        // now sink the sse streams into a single stream and remove the ones that are done
        let stream: AsyncStream<Result<Event, Infallible>, _> = async_stream::stream! {
            loop {
                let mut i = 0;
                while i < all_rxs.len() {
                    let rx = &mut all_rxs[i];
                    select! {
                        Some(event) = rx.recv() => {
                            yield event;
                        }
                        else => {
                            all_rxs.remove(i);
                            continue; // skip the increment to handle the next element at the same index
                        }
                    }
                    i += 1; // only increment when no element was removed
                }

                if all_rxs.is_empty() {
                    break;
                }
            }
        };

        let stream = stream.chain(futures::stream::once(async {
            Ok(Event::default().data("[DONE]"))
        }));

        let sse = Sse::new(stream).keep_alive(KeepAlive::default());
        Ok((headers, sse).into_response())
    } else {
        let current_time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_else(|_| std::time::Duration::from_secs(0))
            .as_secs();

        let responses = FuturesUnordered::new();
        for (index, generate_request) in generate_requests.into_iter().enumerate() {
            let infer_clone = infer.clone();
            let compute_type_clone = compute_type.clone();
            let span_clone = span.clone();
            let response_future = async move {
                let result = generate_internal(
                    Extension(infer_clone),
                    compute_type_clone,
                    Json(generate_request),
                    span_clone,
                )
                .await;
                result.map(|(headers, input_length, generation)| {
                    (index, headers, input_length, generation)
                })
            };
            responses.push(response_future);
        }
        let generate_responses = responses.try_collect::<Vec<_>>().await?;

        let mut prompt_tokens = 0u32;
        let mut completion_tokens = 0u32;
        let mut total_tokens = 0u32;

        let mut x_compute_time = 0u32;
        let mut x_total_time = 0u32;
        let mut x_validation_time = 0u32;
        let mut x_queue_time = 0u32;
        let mut x_inference_time = 0u32;
        let mut x_time_per_token = 0u32;
        let mut x_prompt_tokens = 0u32;
        let mut x_generated_tokens = 0u32;

        let choices = generate_responses
            .into_iter()
            .map(|(index, headers, input_length, Json(generation))| {
                let details = generation.details.ok_or((
                    // this should never happen but handle if details are missing unexpectedly
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(ErrorResponse {
                        error: "No details in generation".to_string(),
                        error_type: "no details".to_string(),
                    }),
                ))?;

                if x_compute_type.is_none() {
                    x_compute_type = headers
                        .get("x-compute-type")
                        .and_then(|v| v.to_str().ok())
                        .map(|v| v.to_string());
                }

                // accumulate headers and usage from each response
                x_compute_time += headers
                    .get("x-compute-time")
                    .and_then(|v| v.to_str().ok()?.parse().ok())
                    .unwrap_or(0);
                x_compute_characters += headers
                    .get("x-compute-characters")
                    .and_then(|v| v.to_str().ok()?.parse().ok())
                    .unwrap_or(0);
                x_total_time += headers
                    .get("x-total-time")
                    .and_then(|v| v.to_str().ok()?.parse().ok())
                    .unwrap_or(0);
                x_validation_time += headers
                    .get("x-validation-time")
                    .and_then(|v| v.to_str().ok()?.parse().ok())
                    .unwrap_or(0);
                x_queue_time += headers
                    .get("x-queue-time")
                    .and_then(|v| v.to_str().ok()?.parse().ok())
                    .unwrap_or(0);
                x_inference_time += headers
                    .get("x-inference-time")
                    .and_then(|v| v.to_str().ok()?.parse().ok())
                    .unwrap_or(0);
                x_time_per_token += headers
                    .get("x-time-per-token")
                    .and_then(|v| v.to_str().ok()?.parse().ok())
                    .unwrap_or(0);
                x_prompt_tokens += headers
                    .get("x-prompt-tokens")
                    .and_then(|v| v.to_str().ok()?.parse().ok())
                    .unwrap_or(0);
                x_generated_tokens += headers
                    .get("x-generated-tokens")
                    .and_then(|v| v.to_str().ok()?.parse().ok())
                    .unwrap_or(0);

                prompt_tokens += input_length;
                completion_tokens += details.generated_tokens;
                total_tokens += input_length + details.generated_tokens;

                Ok(CompletionComplete {
                    finish_reason: details.finish_reason.format(true),
                    index: index as u32,
                    logprobs: None,
                    text: generation.generated_text,
                })
            })
            .collect::<Result<Vec<_>, _>>()
            .map_err(|(status, Json(err))| (status, Json(err)))?;

        let response = Completion::Final(CompletionFinal {
            id: "".to_string(),
            created: current_time,
            model: info.model_id.clone(),
            system_fingerprint: format!(
                "{}-{}",
                info.version,
                info.docker_label.unwrap_or("native")
            ),
            choices,
            usage: Usage {
                prompt_tokens,
                completion_tokens,
                total_tokens,
            },
        });

        // headers similar to `generate` but aggregated
        let mut headers = HeaderMap::new();
        if let Some(x_compute_type) = x_compute_type {
            headers.insert("x-compute-type", x_compute_type.parse().unwrap());
        }
        headers.insert("x-compute-characters", x_compute_characters.into());
        headers.insert("x-total-time", x_total_time.into());
        headers.insert("x-validation-time", x_validation_time.into());
        headers.insert("x-queue-time", x_queue_time.into());
        headers.insert("x-inference-time", x_inference_time.into());
        headers.insert("x-time-per-token", x_time_per_token.into());
        headers.insert("x-prompt-tokens", x_prompt_tokens.into());
        headers.insert("x-generated-tokens", x_generated_tokens.into());
        if let Some(x_accel_buffering) = x_accel_buffering {
            headers.insert("x-accel-buffering", x_accel_buffering.parse().unwrap());
        }
        Ok((headers, Json(response)).into_response())
    }
}

/// Generate tokens
#[utoipa::path(
post,
tag = "Text Generation Inference",
path = "/v1/chat/completions",
request_body = ChatRequest,
responses(
(status = 200, description = "Generated Chat Completion",
content(
("application/json" = ChatCompletion),
("text/event-stream" = ChatCompletionChunk),
)),
(status = 424, description = "Generation Error", body = ErrorResponse,
example = json ! ({"error": "Request failed during generation"})),
(status = 429, description = "Model is overloaded", body = ErrorResponse,
example = json ! ({"error": "Model is overloaded"})),
(status = 422, description = "Input validation error", body = ErrorResponse,
example = json ! ({"error": "Input validation error"})),
(status = 500, description = "Incomplete generation", body = ErrorResponse,
example = json ! ({"error": "Incomplete generation"})),
)
)]
#[instrument(
    skip_all,
    fields(
        parameters,
        total_time,
        validation_time,
        queue_time,
        inference_time,
        time_per_token,
        seed,
    )
)]
pub(crate) async fn chat_completions(
    Extension(infer): Extension<Infer>,
    Extension(compute_type): Extension<ComputeType>,
    Extension(info): Extension<Info>,
    Json(mut chat): Json<ChatRequest>,
) -> Result<Response, (StatusCode, Json<ErrorResponse>)> {
    let span = tracing::Span::current();
    metrics::counter!("tgi_request_count").increment(1);
    let ChatRequest {
        model,
        stream,
        stream_options,
        logprobs,
        ..
    } = chat.clone();

    tracing::debug!("Got chat_template {:?}", infer.chat_template);
    let id = chat.next_tool_call_id();
    let (generate_request, using_tools): (GenerateRequest, bool) =
        chat.clone().try_into_generate(&infer)?;
    span.record("parameters", format!("{:?}", generate_request.parameters));
    let logprobs = logprobs.unwrap_or_default();

    // extract model id from request if specified
    let model_id = match model.as_deref() {
        Some("tgi") | None => info.model_id.clone(),
        Some(m_id) => m_id.to_string(),
    };
    let system_fingerprint = format!("{}-{}", info.version, info.docker_label.unwrap_or("native"));
    // switch on stream
    if stream {
        let (headers, response_stream) = generate_stream_internal(
            infer.clone(),
            compute_type.clone(),
            Json(generate_request),
            span.clone(),
        )
        .await;

        let response_stream = async_stream::stream! {
            let mut response_stream = Box::pin(response_stream);
            let mut state = ChatState::new(using_tools, stream_options.clone(), system_fingerprint.clone(), model_id.clone(), logprobs, id.clone());
            while let Some(result) = response_stream.next().await {
                match result{
                Ok(stream_token) => {
                    let events = state.push(stream_token);
                    match events{
                        ChatEvent::NoTool => {
                            chat.tools = None;
                            chat.response_format = None;
                            let (generate_request, using_tools): (GenerateRequest, bool) =
                                chat.clone().try_into_generate(&infer).unwrap();
                            assert!(!using_tools);
                            let (_headers, response_stream2) =
                                generate_stream_internal(infer.clone(), compute_type.clone(), Json(generate_request), span.clone()).await;
                            state = ChatState::new(using_tools, stream_options.clone(), system_fingerprint.clone(), model_id.clone(), logprobs, id.clone());
                            response_stream = Box::pin(response_stream2);
                        }
                        ChatEvent::Events(events) => {
                            for chat_complete in events{
                                yield Ok(Event::default().json_data(chat_complete).unwrap_or_else(|e| {
                                    tracing::error!("Failed to serialize ChatCompletionChunk: {:?}", e);
                                    Event::default()
                                }));
                            }
                        }
                    }
                }
                Err(err) => yield Ok(err.into_openai_event())
                }
            }
            yield Ok::<Event, Infallible>(Event::default().data("[DONE]"));
        };

        let sse = Sse::new(response_stream).keep_alive(KeepAlive::default());
        Ok((headers, sse).into_response())
    } else {
        let (mut headers, mut input_length, Json(generation)) = generate_internal(
            Extension(infer.clone()),
            compute_type.clone(),
            Json(generate_request),
            span.clone(),
        )
        .await?;

        let current_time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_else(|_| std::time::Duration::from_secs(0))
            .as_secs();

        let (tool_calls, output) = if using_tools {
            match crate::chat::parse_output(&generation.generated_text)? {
                ChatChoice::NoTool => {
                    chat.tools = None;
                    chat.response_format = None;
                    let (generate_request, using_tools): (GenerateRequest, bool) =
                        chat.clone().try_into_generate(&infer)?;
                    assert!(!using_tools);
                    let (headers_final, input_length_final, Json(generation)) = generate_internal(
                        Extension(infer),
                        compute_type,
                        Json(generate_request),
                        span,
                    )
                    .await?;
                    headers = headers_final;
                    input_length = input_length_final;
                    (None, Some(generation.generated_text))
                }
                ChatChoice::ToolCalls(tool_calls) => (Some(tool_calls), None),
            }
        } else {
            (None, Some(generation.generated_text))
        };
        // build the complete response object with the full text
        let response = CompletionType::ChatCompletion(ChatCompletion::new(
            model_id,
            system_fingerprint,
            output,
            current_time,
            generation.details.unwrap(),
            logprobs,
            tool_calls,
            input_length,
        ));

        // wrap generation inside a Vec to match api-inference
        Ok((headers, Json(response)).into_response())
    }
}

/// Tokenize inputs
#[utoipa::path(
post,
tag = "Text Generation Inference",
path = "/tokenize",
request_body = GenerateRequest,
responses(
(status = 200, description = "Tokenized ids", body = TokenizeResponse),
(status = 404, description = "No tokenizer found", body = ErrorResponse,
example = json ! ({"error": "No fast tokenizer available"})),
)
)]
#[instrument(skip_all)]
async fn tokenize(
    Extension(infer): Extension<Infer>,
    Json(req): Json<GenerateRequest>,
) -> Result<Json<TokenizeResponse>, (StatusCode, Json<ErrorResponse>)> {
    let input = req.inputs.clone();
    let encoding = infer.tokenize(req).await?;
    let tokens = encoding_to_tokens(&encoding, &input);
    Ok(Json(TokenizeResponse(tokens)))
}

/// Prometheus metrics scrape endpoint
#[utoipa::path(
    get,
    tag = "Text Generation Inference",
    path = "/metrics",
    responses((status = 200, description = "Prometheus Metrics", body = String))
)]
async fn metrics(prom_handle: Extension<PrometheusHandle>) -> String {
    prom_handle.render()
}

#[derive(Clone, Debug)]
pub(crate) struct ComputeType(String);

// OpenAPI documentation
#[derive(OpenApi)]
#[openapi(
paths(
health,
get_model_info,
compat_generate,
generate,
generate_stream,
chat_completions,
completions,
tokenize,
metrics,
openai_get_model_info,
sagemaker_compatibility,
get_chat_tokenize,
),
components(
schemas(
Info,
CompatGenerateRequest,
SagemakerRequest,
GenerateRequest,
GrammarType,
ChatRequest,
Message,
MessageContent,
MessageChunk,
Url,
FunctionName,
OutputMessage,
TextMessage,
ToolCallMessage,
ToolCallDelta,
ChatCompletionComplete,
ChatCompletionChoice,
ChatCompletionDelta,
ChatCompletionChunk,
ChatCompletionLogprob,
ChatCompletionLogprobs,
ChatCompletionTopLogprob,
ChatCompletion,
CompletionRequest,
CompletionComplete,
SagemakerResponse,
SagemakerStreamResponse,
Chunk,
Completion,
CompletionFinal,
Prompt,
GenerateParameters,
PrefillToken,
Token,
GenerateResponse,
TokenizeResponse,
SimpleToken,
BestOfSequence,
Details,
FinishReason,
StreamResponse,
StreamDetails,
ErrorResponse,
GrammarType,
Usage,
StreamOptions,
DeltaToolCall,
Tool,
ToolCall,
Function,
FunctionDefinition,
ToolChoice,
ModelInfo,
ChatTokenizeResponse,
MessageBody,
)
),
tags(
(name = "Text Generation Inference", description = "Hugging Face Text Generation Inference API")
),
info(
title = "Text Generation Inference",
license(
name = "Apache 2.0",
url = "https://www.apache.org/licenses/LICENSE-2.0"
)
)
)]
pub struct ApiDoc;

pub fn schema() -> ApiDoc {
    ApiDoc
}

pub fn py_resolve_tokenizer(
    py: pyo3::Python,
    tokenizer_name: &str,
    revision: Option<&str>,
    trust_remote_code: bool,
) -> pyo3::PyResult<()> {
    let transformers = py.import_bound("transformers")?;
    let auto = transformers.getattr("AutoTokenizer")?;
    let from_pretrained = auto.getattr("from_pretrained")?;
    let args = (tokenizer_name,);
    let kwargs = if let Some(rev) = &revision {
        [
            ("revision", rev.to_string().into_py(py)),
            ("trust_remote_code", trust_remote_code.into_py(py)),
        ]
        .into_py_dict_bound(py)
    } else {
        [("trust_remote_code", trust_remote_code.into_py(py))].into_py_dict_bound(py)
    };
    let tokenizer = from_pretrained.call(args, Some(&kwargs))?;
    let save = tokenizer.getattr("save_pretrained")?;
    let args = ("out".to_string(),);
    save.call1(args)?;
    Ok(())
}

pub fn legacy_tokenizer_handle(config_filename: Option<&PathBuf>) -> Option<()> {
    // XXX Legacy case for FasterDecoding/medusa-vicuna-7b-v1.3
    // and state-spaces/mamba-130m
    tracing::warn!("Odd tokenizer detected, falling back on legacy tokenization");

    #[derive(serde::Deserialize)]
    struct FallbackConfig {
        base_model_name_or_path: Option<String>,
        model_type: Option<String>,
        ssm_config: Option<serde_json::Value>,
    }
    config_filename.and_then(|filename| {
        std::fs::read_to_string(filename)
            .ok()
            .as_ref()
            .and_then(|c| {
                let config: Result<FallbackConfig, _> = serde_json::from_str(c);
                if let Ok(config) = config {
                    if config.model_type.is_none() {
                        if let Some(base) = config.base_model_name_or_path {
                            pyo3::Python::with_gil(|py| -> PyResult<()> {
                                py_resolve_tokenizer(py, &base, Some("main"), false)
                            })
                            .ok()?;
                        }
                    }
                    if config.ssm_config.is_some() {
                        // XXX Legacy mamba
                        pyo3::Python::with_gil(|py| -> PyResult<()> {
                            py_resolve_tokenizer(py, "EleutherAI/gpt-neox-20b", Some("main"), false)
                        })
                        .ok()?;
                    }
                }
                Some(())
            })
    })
}

/// Serving method
#[allow(clippy::too_many_arguments)]
pub async fn run(
    backend: impl Backend + Send + Sync + 'static,
    max_concurrent_requests: usize,
    max_best_of: usize,
    max_stop_sequences: usize,
    max_top_n_tokens: u32,
    max_input_tokens: usize,
    max_total_tokens: usize,
    validation_workers: usize,
    api_key: Option<String>,
    tokenizer_name: String,
    tokenizer_config_path: Option<String>,
    revision: Option<String>,
    trust_remote_code: bool,
    hostname: String,
    port: u16,
    cors_allow_origin: Option<Vec<String>>,
    ngrok: bool,
    _ngrok_authtoken: Option<String>,
    _ngrok_edge: Option<String>,
    disable_grammar_support: bool,
    max_client_batch_size: usize,
    usage_stats_level: usage_stats::UsageStatsLevel,
    payload_limit: usize,
) -> Result<(), WebServerError> {
    // CORS allowed origins
    // map to go inside the option and then map to parse from String to HeaderValue
    // Finally, convert to AllowOrigin
    let allow_origin: Option<AllowOrigin> = cors_allow_origin.map(|cors_allow_origin| {
        AllowOrigin::list(
            cors_allow_origin
                .iter()
                .map(|origin| origin.parse::<HeaderValue>().unwrap()),
        )
    });

    // Parse Huggingface hub token
    let authorization_token = std::env::var("HF_TOKEN")
        .or_else(|_| std::env::var("HUGGING_FACE_HUB_TOKEN"))
        .ok();

    // Tokenizer instance
    // This will only be used to validate payloads
    let local_path = Path::new(&tokenizer_name);

    // Shared API builder initialization
    let api_builder = || {
        let mut builder = ApiBuilder::from_env().with_progress(false);
        if let Some(token) = authorization_token {
            builder = builder.with_token(Some(token));
        }

        if let Ok(cache_dir) = std::env::var("HUGGINGFACE_HUB_CACHE") {
            builder = builder.with_cache_dir(cache_dir.into());
        }

        if let Ok(origin) = std::env::var("HF_HUB_USER_AGENT_ORIGIN") {
            builder = builder.with_user_agent("origin", origin.as_str());
        }

        builder
    };

    // Decide if we need to use the API based on the revision and local path
    let use_api = revision.is_some() || !local_path.exists() || !local_path.is_dir();

    // Initialize API if needed
    #[derive(Clone)]
    enum Type {
        Api(Api),
        Cache(Cache),
        None,
    }
    let api = if use_api {
        if std::env::var("HF_HUB_OFFLINE") == Ok("1".to_string()) {
            let cache = std::env::var("HUGGINGFACE_HUB_CACHE")
                .map_err(|_| ())
                .map(|cache_dir| Cache::new(cache_dir.into()))
                .unwrap_or_else(|_| Cache::default());
            tracing::warn!("Offline mode active using cache defaults");
            Type::Cache(cache)
        } else {
            tracing::info!("Using the Hugging Face API");
            match api_builder().build() {
                Ok(api) => Type::Api(api),
                Err(_) => {
                    tracing::warn!("Unable to build the Hugging Face API");
                    Type::None
                }
            }
        }
    } else {
        Type::None
    };

    // Load tokenizer and model info
    let (
        config_filename,
        tokenizer_config_filename,
        preprocessor_config_filename,
        processor_config_filename,
        chat_template_filename,
        model_info,
    ) = match api {
        Type::None => (
            Some(local_path.join("config.json")),
            Some(local_path.join("tokenizer_config.json")),
            Some(local_path.join("preprocessor_config.json")),
            Some(local_path.join("processor_config.json")),
            Some(local_path.join("chat_template.json")),
            None,
        ),
        Type::Api(api) => {
            let api_repo = api.repo(Repo::with_revision(
                tokenizer_name.to_string(),
                RepoType::Model,
                revision.clone().unwrap_or_else(|| "main".to_string()),
            ));

            let config_filename = api_repo.get("config.json").await.ok();
            let tokenizer_config_filename = api_repo.get("tokenizer_config.json").await.ok();
            let preprocessor_config_filename = api_repo.get("preprocessor_config.json").await.ok();
            let processor_config_filename = api_repo.get("processor_config.json").await.ok();
            let chat_template_filename = api_repo.get("chat_template.json").await.ok();

            let model_info = if let Some(model_info) = get_hub_model_info(&api_repo).await {
                Some(model_info)
            } else {
                tracing::warn!("Could not retrieve model info from the Hugging Face hub.");
                None
            };
            (
                config_filename,
                tokenizer_config_filename,
                preprocessor_config_filename,
                processor_config_filename,
                chat_template_filename,
                model_info,
            )
        }
        Type::Cache(cache) => {
            tracing::info!("Cache {cache:?}");
            let repo = cache.repo(Repo::with_revision(
                tokenizer_name.to_string(),
                RepoType::Model,
                revision.clone().unwrap_or_else(|| "main".to_string()),
            ));
            (
                repo.get("config.json"),
                repo.get("tokenizer_config.json"),
                repo.get("preprocessor_config.json"),
                repo.get("processor_config.json"),
                repo.get("chat_template.json"),
                None,
            )
        }
    };

    // if chat_template_filename is present, load the chat template
    let chat_template: Option<crate::ChatTemplateVersions> = chat_template_filename
        .and_then(|f| std::fs::read_to_string(f).ok())
        .and_then(|c| {
            let res = serde_json::from_str::<crate::ChatTemplateStandalone>(&c);
            if let Err(e) = &res {
                tracing::warn!("Could not parse chat template {e:?}");
            }
            res.ok().map(|t| t.chat_template)
        });

    // Read the JSON contents of the file as an instance of 'HubTokenizerConfig'.
    tracing::warn!("Tokenizer_config {tokenizer_config_path:?} - {tokenizer_config_filename:?}");
    let tokenizer_config: Option<HubTokenizerConfig> = if let Some(filename) = tokenizer_config_path
    {
        HubTokenizerConfig::from_file(filename)
    } else {
        tokenizer_config_filename.and_then(HubTokenizerConfig::from_file)
    };
    let mut tokenizer_config = tokenizer_config.unwrap_or_else(|| {
        tracing::warn!("Could not find tokenizer config locally and no API specified");
        HubTokenizerConfig::default()
    });

    if chat_template.is_some() {
        tracing::info!("Using chat template from chat_template.json");
        tokenizer_config.chat_template = chat_template;
    }

    let tokenizer: Result<Tokenizer, WebServerError> = {
        use pyo3::prelude::*;
        Python::with_gil(|py| -> PyResult<()> {
            py_resolve_tokenizer(py, &tokenizer_name, revision.as_deref(), trust_remote_code)?;
            Ok(())
        })
        .inspect_err(|err| {
            tracing::error!("Failed to import python tokenizer {err}");
        })
        .or_else(|err| {
            let out = legacy_tokenizer_handle(config_filename.as_ref());
            out.ok_or(err)
        })
        .map_err(|_| WebServerError::Tokenizer("Unable to load tokenizer.".to_string()))?;
        let filename = "out/tokenizer.json";
        if let Ok(tok) = tokenizers::Tokenizer::from_file(filename) {
            Ok(Tokenizer::Rust(tok))
        } else {
            Ok(Tokenizer::Python {
                tokenizer_name: tokenizer_name.clone(),
                revision: revision.clone(),
                trust_remote_code,
            })
        }
    };

    let config: Option<Config> = config_filename.and_then(|filename| {
        std::fs::read_to_string(filename)
            .ok()
            .as_ref()
            .and_then(|c| {
                let config: Result<Config, _> = serde_json::from_str(c);
                if let Err(err) = &config {
                    tracing::warn!("Could not parse config {err:?}");
                }
                config.ok()
            })
    });
    let model_info = model_info.unwrap_or_else(|| HubModelInfo {
        model_id: tokenizer_name.to_string(),
        sha: None,
        pipeline_tag: None,
    });

    let processor_config = processor_config_filename
        .and_then(HubProcessorConfig::from_file)
        .unwrap_or_default();

    let preprocessor_config: Option<HubPreprocessorConfig> =
        preprocessor_config_filename.and_then(HubPreprocessorConfig::from_file);

    tracing::info!("Using config {config:?}");

    // Only send usage stats when TGI is run in container and the function returns Some
    let is_container = matches!(usage_stats::is_container(), Ok(true));
    // retrieve the huggingface_hub user agent origin if set, and add the origin to telemetry
    let origin = std::env::var("HF_HUB_USER_AGENT_ORIGIN").ok();
    let user_agent = match (usage_stats_level, is_container) {
        (usage_stats::UsageStatsLevel::On | usage_stats::UsageStatsLevel::NoStack, true) => {
            let reduced_args = usage_stats::Args::new(
                config.clone(),
                tokenizer_config.tokenizer_class.clone(),
                max_concurrent_requests,
                max_best_of,
                max_stop_sequences,
                max_top_n_tokens,
                max_input_tokens,
                max_total_tokens,
                // waiting_served_ratio,
                // max_batch_prefill_tokens,
                // max_batch_total_tokens,
                // max_waiting_tokens,
                // max_batch_size,
                revision.clone(),
                validation_workers,
                disable_grammar_support,
                max_client_batch_size,
                usage_stats_level,
                backend.name(),
                origin,
            );
            Some(usage_stats::UserAgent::new(reduced_args))
        }
        _ => None,
    };

    let stop_usage_thread = Arc::new(AtomicBool::new(false));
    let stop_usage_thread_clone = stop_usage_thread.clone();
    if let Some(ua) = user_agent.clone() {
        let start_event =
            usage_stats::UsageStatsEvent::new(ua.clone(), usage_stats::EventType::Start, None);
        tokio::spawn(async move {
            // send start event
            start_event.send().await;
            let mut last_report = Instant::now();
            while !stop_usage_thread_clone.load(Ordering::Relaxed) {
                if last_report.elapsed() > Duration::from_secs(900) {
                    let report_event = usage_stats::UsageStatsEvent::new(
                        ua.clone(),
                        usage_stats::EventType::Ping,
                        None,
                    );
                    report_event.send().await;
                    last_report = Instant::now();
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
    };
    let compat_return_full_text = match &model_info.pipeline_tag {
        None => {
            tracing::warn!("no pipeline tag found for model {tokenizer_name}");
            true
        }
        Some(pipeline_tag) => pipeline_tag.as_str() == "text-generation",
    };
    let result = start(
        backend,
        max_concurrent_requests,
        max_best_of,
        max_stop_sequences,
        max_top_n_tokens,
        max_input_tokens,
        max_total_tokens,
        validation_workers,
        api_key,
        config,
        (tokenizer?, tokenizer_config),
        (preprocessor_config, processor_config),
        hostname,
        port,
        ngrok,
        _ngrok_authtoken,
        _ngrok_edge,
        disable_grammar_support,
        max_client_batch_size,
        model_info,
        compat_return_full_text,
        allow_origin,
        payload_limit,
    )
    .await;

    if let Some(ua) = user_agent {
        stop_usage_thread.store(true, Ordering::Relaxed);
        match result {
            Ok(_) => {
                let stop_event = usage_stats::UsageStatsEvent::new(
                    ua.clone(),
                    usage_stats::EventType::Stop,
                    None,
                );
                stop_event.send().await;
                Ok(())
            }
            Err(e) => {
                let description = match usage_stats_level {
                    usage_stats::UsageStatsLevel::On => Some(e.to_string()),
                    usage_stats::UsageStatsLevel::NoStack => Some("unknow_error".to_string()),
                    _ => None,
                };
                let event = usage_stats::UsageStatsEvent::new(
                    ua.clone(),
                    usage_stats::EventType::Error,
                    description,
                );
                event.send().await;

                Err(e)
            }
        }
    } else {
        result
    }
}

#[allow(clippy::too_many_arguments)]
async fn start(
    backend: impl Backend + Send + Sync + 'static,
    max_concurrent_requests: usize,
    max_best_of: usize,
    max_stop_sequences: usize,
    max_top_n_tokens: u32,
    max_input_tokens: usize,
    max_total_tokens: usize,
    validation_workers: usize,
    api_key: Option<String>,
    config: Option<Config>,
    (tokenizer, tokenizer_config): (Tokenizer, HubTokenizerConfig),
    (preprocessor_config, processor_config): (Option<HubPreprocessorConfig>, HubProcessorConfig),
    hostname: String,
    port: u16,
    ngrok: bool,
    _ngrok_authtoken: Option<String>,
    _ngrok_edge: Option<String>,
    disable_grammar_support: bool,
    max_client_batch_size: usize,
    model_info: HubModelInfo,
    compat_return_full_text: bool,
    allow_origin: Option<AllowOrigin>,
    payload_limit: usize,
) -> Result<(), WebServerError> {
    // Determine the server port based on the feature and environment variable.
    let port = if cfg!(feature = "google") {
        std::env::var("AIP_HTTP_PORT")
            .map(|aip_http_port| aip_http_port.parse::<u16>().unwrap_or(port))
            .unwrap_or(port)
    } else {
        port
    };

    let addr = match hostname.parse() {
        Ok(ip) => SocketAddr::new(ip, port),
        Err(_) => {
            tracing::warn!("Invalid hostname, defaulting to 0.0.0.0");
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), port)
        }
    };

    // Create state
    let validation = Validation::new(
        validation_workers,
        tokenizer,
        config,
        preprocessor_config,
        max_best_of,
        max_stop_sequences,
        max_top_n_tokens,
        max_input_tokens,
        max_total_tokens,
        disable_grammar_support,
    );

    let infer = Infer::new(
        backend,
        validation,
        max_concurrent_requests,
        tokenizer_config,
        processor_config,
    );

    // Duration buckets
    let duration_matcher = Matcher::Suffix(String::from("duration"));
    let n_duration_buckets = 35;
    let mut duration_buckets = Vec::with_capacity(n_duration_buckets);
    // Minimum duration in seconds
    let mut value = 0.0001;
    for _ in 0..n_duration_buckets {
        // geometric sequence
        value *= 1.5;
        duration_buckets.push(value);
    }
    // Input Length buckets
    let input_length_matcher = Matcher::Full(String::from("tgi_request_input_length"));
    let input_length_buckets: Vec<f64> = (0..100)
        .map(|x| (max_input_tokens as f64 / 100.0) * (x + 1) as f64)
        .collect();
    // Generated tokens buckets
    let generated_tokens_matcher = Matcher::Full(String::from("tgi_request_generated_tokens"));
    let generated_tokens_buckets: Vec<f64> = (0..100)
        .map(|x| (max_total_tokens as f64 / 100.0) * (x + 1) as f64)
        .collect();
    // Input Length buckets
    let max_new_tokens_matcher = Matcher::Full(String::from("tgi_request_max_new_tokens"));
    let max_new_tokens_buckets: Vec<f64> = (0..100)
        .map(|x| (max_total_tokens as f64 / 100.0) * (x + 1) as f64)
        .collect();
    // Batch size buckets
    let batch_size_matcher = Matcher::Full(String::from("tgi_batch_next_size"));
    let batch_size_buckets: Vec<f64> = (0..1024).map(|x| (x + 1) as f64).collect();
    // Speculated tokens buckets
    // let skipped_matcher = Matcher::Full(String::from("tgi_request_skipped_tokens"));
    // let skipped_buckets: Vec<f64> = (0..shard_info.speculate + 1).map(|x| x as f64).collect();

    // Prometheus handler
    let builder = PrometheusBuilder::new()
        .set_buckets_for_metric(duration_matcher, &duration_buckets)
        .unwrap()
        .set_buckets_for_metric(input_length_matcher, &input_length_buckets)
        .unwrap()
        .set_buckets_for_metric(generated_tokens_matcher, &generated_tokens_buckets)
        .unwrap()
        .set_buckets_for_metric(max_new_tokens_matcher, &max_new_tokens_buckets)
        .unwrap()
        .set_buckets_for_metric(batch_size_matcher, &batch_size_buckets)
        .unwrap();
    // .set_buckets_for_metric(skipped_matcher, &skipped_buckets)
    // .unwrap();
    // See: https://github.com/metrics-rs/metrics/issues/467#issuecomment-2022755151
    let (recorder, _) = builder
        .build()
        .expect("failed to build prometheus recorder");
    let prom_handle = recorder.handle();
    metrics::set_global_recorder(recorder).expect("Failed to set global recorder");

    // Metrics descriptions
    metrics::describe_counter!("tgi_request_success", "Number of successful requests");
    metrics::describe_histogram!(
        "tgi_request_duration",
        metrics::Unit::Seconds,
        "Request duration"
    );
    metrics::describe_histogram!(
        "tgi_request_validation_duration",
        metrics::Unit::Seconds,
        "Request validation duration"
    );
    metrics::describe_histogram!(
        "tgi_request_queue_duration",
        metrics::Unit::Seconds,
        "Request queue duration"
    );
    metrics::describe_histogram!(
        "tgi_request_inference_duration",
        metrics::Unit::Seconds,
        "Request inference duration"
    );
    metrics::describe_histogram!(
        "tgi_request_mean_time_per_token_duration",
        metrics::Unit::Seconds,
        "Mean time per token per request"
    );
    metrics::describe_histogram!(
        "tgi_request_generated_tokens",
        metrics::Unit::Count,
        "Generated tokens per request"
    );
    metrics::describe_counter!(
        "tgi_batch_inference_count",
        metrics::Unit::Count,
        "Inference calls per method (prefill or decode)"
    );
    metrics::describe_counter!(
        "tgi_request_count",
        metrics::Unit::Count,
        "Total number of requests"
    );
    metrics::describe_counter!(
        "tgi_batch_inference_success",
        metrics::Unit::Count,
        "Number of successful inference calls per method (prefill or decode)"
    );
    metrics::describe_gauge!(
        "tgi_batch_current_size",
        metrics::Unit::Count,
        "Current batch size"
    );
    metrics::describe_gauge!("tgi_queue_size", metrics::Unit::Count, "Current queue size");
    metrics::describe_gauge!(
        "tgi_batch_current_max_tokens",
        metrics::Unit::Count,
        "Maximum tokens for the current batch"
    );
    metrics::describe_gauge!(
        "tgi_batch_total_tokens",
        metrics::Unit::Count,
        "Maximum amount of tokens in total."
    );
    metrics::describe_histogram!(
        "tgi_request_max_new_tokens",
        metrics::Unit::Count,
        "Maximum new tokens per request"
    );
    metrics::describe_histogram!(
        "tgi_batch_inference_duration",
        metrics::Unit::Seconds,
        "Batch inference duration"
    );
    metrics::describe_histogram!(
        "tgi_batch_forward_duration",
        metrics::Unit::Seconds,
        "Batch forward duration per method (prefill or decode)"
    );
    metrics::describe_histogram!(
        "tgi_request_skipped_tokens",
        metrics::Unit::Count,
        "Speculated tokens per request"
    );
    metrics::describe_histogram!(
        "tgi_batch_filter_duration",
        metrics::Unit::Seconds,
        "Time spent filtering batches and sending generated tokens per method (prefill or decode)"
    );
    metrics::describe_histogram!(
        "tgi_request_queue_duration",
        metrics::Unit::Seconds,
        "Time spent in the queue per request"
    );
    metrics::describe_histogram!(
        "tgi_request_validation_duration",
        metrics::Unit::Seconds,
        "Time spent validating the request"
    );
    metrics::describe_histogram!(
        "tgi_request_duration",
        metrics::Unit::Seconds,
        "Total time spent processing the request"
    );
    metrics::describe_histogram!(
        "tgi_batch_decode_duration",
        metrics::Unit::Seconds,
        "Time spent decoding a batch per method (prefill or decode)"
    );
    metrics::describe_histogram!(
        "tgi_request_input_length",
        metrics::Unit::Count,
        "Input token length per request"
    );
    metrics::describe_histogram!(
        "tgi_batch_next_size",
        metrics::Unit::Count,
        "Batch size of the next batch"
    );

    // CORS layer
    let allow_origin = allow_origin.unwrap_or(AllowOrigin::any());
    let cors_layer = CorsLayer::new()
        .allow_methods([Method::GET, Method::POST])
        .allow_headers([http::header::CONTENT_TYPE])
        .allow_origin(allow_origin);

    // Endpoint info
    let info = Info {
        model_id: model_info.model_id,
        model_sha: model_info.sha,
        // model_dtype: shard_info.dtype,
        // model_device_type: shard_info.device_type,
        model_pipeline_tag: model_info.pipeline_tag,
        max_concurrent_requests,
        max_best_of,
        max_stop_sequences,
        max_input_tokens,
        max_total_tokens,
        // waiting_served_ratio,
        // max_batch_total_tokens,
        // max_waiting_tokens,
        // max_batch_size,
        validation_workers,
        max_client_batch_size,
        router: env!("CARGO_PKG_NAME"),
        version: env!("CARGO_PKG_VERSION"),
        sha: option_env!("VERGEN_GIT_SHA"),
        docker_label: option_env!("DOCKER_LABEL"),
    };

    #[allow(unused_mut)] // mut is needed for conditional compilation
    let mut doc = ApiDoc::openapi();

    #[cfg(feature = "google")]
    {
        use crate::vertex::__path_vertex_compatibility;
        use crate::vertex::{VertexInstance, VertexRequest, VertexResponse};

        #[derive(OpenApi)]
        #[openapi(
            paths(vertex_compatibility),
            components(schemas(VertexInstance, VertexRequest, VertexResponse))
        )]
        struct VertexApiDoc;

        doc.merge(VertexApiDoc::openapi());
    }

    #[cfg(feature = "kserve")]
    {
        use crate::kserve::{
            InferenceOutput, InferenceRequest, LiveResponse, MetadataServerResponse, OutputChunk,
            ReadyResponse,
        };
        use crate::kserve::{
            __path_kerve_server_metadata, __path_kserve_health_live, __path_kserve_health_ready,
            __path_kserve_model_infer, __path_kserve_model_metadata,
            __path_kserve_model_metadata_ready,
        };

        #[derive(OpenApi)]
        #[openapi(
            paths(
                kserve_health_live,
                kserve_health_ready,
                kerve_server_metadata,
                kserve_model_metadata,
                kserve_model_metadata_ready,
                kserve_model_infer,
            ),
            components(schemas(
                InferenceOutput,
                InferenceRequest,
                LiveResponse,
                MetadataServerResponse,
                OutputChunk,
                ReadyResponse,
            ))
        )]
        struct KServeApiDoc;

        doc.merge(KServeApiDoc::openapi());
    }

    // Configure Swagger UI
    let swagger_ui = SwaggerUi::new("/docs").url("/api-doc/openapi.json", doc);

    // Define base and health routes
    let mut base_routes = Router::new()
        .route("/", post(compat_generate))
        .route("/generate", post(generate))
        .route("/generate_stream", post(generate_stream))
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/completions", post(completions))
        .route("/vertex", post(vertex_compatibility))
        .route("/invocations", post(sagemaker_compatibility))
        .route("/tokenize", post(tokenize));

    if let Some(api_key) = api_key {
        let mut prefix = "Bearer ".to_string();
        prefix.push_str(&api_key);

        // Leak to allow FnMut
        let api_key: &'static str = prefix.leak();

        let auth = move |headers: HeaderMap,
                         request: axum::extract::Request,
                         next: axum::middleware::Next| async move {
            match headers.get(AUTHORIZATION) {
                Some(token) => match token.to_str() {
                    Ok(token_str) if token_str.to_lowercase() == api_key.to_lowercase() => {
                        let response = next.run(request).await;
                        Ok(response)
                    }
                    _ => Err(StatusCode::UNAUTHORIZED),
                },
                None => Err(StatusCode::UNAUTHORIZED),
            }
        };

        base_routes = base_routes.layer(axum::middleware::from_fn(auth))
    }
    let info_routes = Router::new()
        .route("/", get(health))
        .route("/chat_tokenize", post(get_chat_tokenize))
        .route("/info", get(get_model_info))
        .route("/health", get(health))
        .route("/ping", get(health))
        .route("/metrics", get(metrics))
        .route("/v1/models", get(openai_get_model_info));

    let compute_type =
        ComputeType(std::env::var("COMPUTE_TYPE").unwrap_or("gpu+optimized".to_string()));

    // Combine routes and layers
    let mut app = Router::new()
        .merge(swagger_ui)
        .merge(base_routes)
        .merge(info_routes);

    #[cfg(feature = "google")]
    {
        tracing::info!("Built with `google` feature");
        tracing::info!(
            "Environment variables `AIP_PREDICT_ROUTE` and `AIP_HEALTH_ROUTE` will be respected."
        );
        if let Ok(env_predict_route) = std::env::var("AIP_PREDICT_ROUTE") {
            app = app.route(&env_predict_route, post(vertex_compatibility));
        }
        if let Ok(env_health_route) = std::env::var("AIP_HEALTH_ROUTE") {
            app = app.route(&env_health_route, get(health));
        }
    }

    #[cfg(feature = "kserve")]
    {
        tracing::info!("Built with `kserve` feature");
        app = app
            .route(
                "/v2/models/:model_name/versions/:model_version/infer",
                post(kserve_model_infer),
            )
            .route(
                "/v2/models/:model_name/versions/:model_version",
                get(kserve_model_metadata),
            )
            .route("/v2/health/ready", get(kserve_health_ready))
            .route("/v2/health/live", get(kserve_health_live))
            .route("/v2", get(kerve_server_metadata))
            .route(
                "/v2/models/:model_name/versions/:model_version/ready",
                get(kserve_model_metadata_ready),
            );
    }

    // add layers after routes
    app = app
        .layer(Extension(info))
        .layer(Extension(compat_return_full_text))
        .layer(Extension(infer))
        .layer(Extension(compute_type))
        .layer(Extension(prom_handle.clone()))
        .layer(OtelAxumLayer::default())
        .layer(DefaultBodyLimit::max(payload_limit))
        .layer(cors_layer);

    tracing::info!("Connected");

    if ngrok {
        #[cfg(feature = "ngrok")]
        {
            panic!("ngrok feature is not functional with axum=0.7 and hyper=1, waiting on https://github.com/ngrok/ngrok-rust/pull/137/files to re-enable.");

            // Run server
        }
        #[cfg(not(feature = "ngrok"))]
        {
            let _ngrok_authtoken = ngrok_authtoken;
            let _ngrok_domain = ngrok_domain;
            let _ngrok_username = ngrok_username;
            let _ngrok_password = ngrok_password;

            panic!("`text-generation-router` was compiled without the `ngrok` feature");
        }
    } else {
        // Run server
        let listener = match tokio::net::TcpListener::bind(&addr).await {
            Ok(listener) => listener,
            Err(e) => {
                tracing::error!("Failed to bind to {addr}: {e}");
                return Err(WebServerError::Axum(Box::new(e)));
            }
        };
        axum::serve(listener, app)
            .with_graceful_shutdown(shutdown_signal())
            .await
            .map_err(|err| WebServerError::Axum(Box::new(err)))?;
    }
    Ok(())
}

/// get model info from the Huggingface Hub
pub async fn get_hub_model_info(api: &ApiRepo) -> Option<HubModelInfo> {
    let response = api.info_request().send().await.ok()?;

    if response.status().is_success() {
        let hub_model_info: HubModelInfo =
            serde_json::from_str(&response.text().await.ok()?).ok()?;
        if let Some(sha) = &hub_model_info.sha {
            tracing::info!(
                "Serving revision {sha} of model {}",
                hub_model_info.model_id
            );
        }
        Some(hub_model_info)
    } else {
        None
    }
}

/// get tokenizer_config from the Huggingface Hub
pub async fn get_tokenizer_config(api_repo: &ApiRepo) -> Option<HubTokenizerConfig> {
    let tokenizer_config_filename = api_repo.get("tokenizer_config.json").await.ok()?;

    // Open the file in read-only mode with buffer.
    let file = File::open(tokenizer_config_filename).ok()?;
    let reader = BufReader::new(file);

    // Read the JSON contents of the file as an instance of 'HubTokenizerConfig'.
    let tokenizer_config: HubTokenizerConfig = serde_json::from_reader(reader)
        .map_err(|e| {
            tracing::warn!("Unable to parse tokenizer config: {}", e);
            e
        })
        .ok()?;

    Some(tokenizer_config)
}

/// Shutdown signal handler
async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    tracing::info!("signal received, starting graceful shutdown");
    opentelemetry::global::shutdown_tracer_provider();
}

/// Convert to Axum supported formats
impl From<InferError> for (StatusCode, Json<ErrorResponse>) {
    fn from(err: InferError) -> Self {
        let status_code = match err {
            InferError::GenerationError(_) => StatusCode::FAILED_DEPENDENCY,
            InferError::Overloaded(_) => StatusCode::TOO_MANY_REQUESTS,
            InferError::ValidationError(_) => StatusCode::UNPROCESSABLE_ENTITY,
            InferError::IncompleteGeneration => StatusCode::INTERNAL_SERVER_ERROR,
            InferError::IncompleteGenerationStream => StatusCode::INTERNAL_SERVER_ERROR,
            InferError::TemplateError(_) => StatusCode::UNPROCESSABLE_ENTITY,
            InferError::MissingTemplateVariable(_) => StatusCode::UNPROCESSABLE_ENTITY,
            InferError::ToolError(_) => StatusCode::UNPROCESSABLE_ENTITY,
            InferError::StreamSerializationError(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };

        (
            status_code,
            Json(ErrorResponse {
                error: err.to_string(),
                error_type: err.error_type().to_string(),
            }),
        )
    }
}

impl From<InferError> for Event {
    fn from(err: InferError) -> Self {
        Event::default()
            .json_data(ErrorResponse {
                error: err.to_string(),
                error_type: err.error_type().to_string(),
            })
            .unwrap()
    }
}

#[derive(Debug, Error)]
pub enum WebServerError {
    #[error("Axum error: {0}")]
    Axum(#[from] axum::BoxError),
    #[error("Tokenizer error: {0}")]
    Tokenizer(String),
}
