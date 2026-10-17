//! Thin async wrapper over `aws-sdk-s3` for the operations RawDB needs:
//! listing, HEAD, GET (small + streamed), PUT (with optional If-Match /
//! If-None-Match for conditional writes), COPY, DELETE, and presigning.
//!
//! Works with Garage, Cloudflare R2, B2, SeaweedFS, etc. via the
//! `endpoint_url` + path-style addressing toggle. Credentials are supplied
//! explicitly from `RAWDB_S3_*` config — we never read the AWS env chain.

use std::time::Duration;

use anyhow::Result;
use aws_config::BehaviorVersion;
use aws_credential_types::Credentials;
use aws_sdk_s3::config::{Region, SharedCredentialsProvider};
use aws_sdk_s3::error::{ProvideErrorMetadata, SdkError};
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::presigning::PresigningConfig;
use aws_sdk_s3::types::{
    CompletedMultipartUpload, CompletedPart, Delete, ObjectIdentifier,
};
use aws_sdk_s3::Client;
use aws_smithy_runtime_api::client::orchestrator::HttpResponse;
use chrono::{DateTime, Utc};

use crate::config::Config;

#[derive(Clone)]
pub struct S3 {
    client: Client,
    bucket: String,
    presign_ttl: Duration,
    /// Whether the backend honors `If-Match` on `PutObject`. When false,
    /// `users.rs` skips conditional headers (last-writer-wins).
    conditional_writes: bool,
}

#[derive(Debug, Clone)]
pub struct ObjectInfo {
    pub key: String,
    pub size: u64,
    pub etag: Option<String>,
    pub last_modified: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
pub struct HeadInfo {
    pub size: u64,
    pub etag: Option<String>,
    pub last_modified: Option<DateTime<Utc>>,
    pub content_type: Option<String>,
}

/// A streaming GET response. `body` is the SDK's `ByteStream` (a
/// `Stream<Item = Result<Bytes, _>>`) and is intended to be plugged
/// directly into an HTTP response body — the bytes are never collected
/// into memory.
pub struct GetStream {
    pub body: aws_sdk_s3::primitives::ByteStream,
    pub content_length: Option<i64>,
    pub content_type: Option<String>,
    pub etag: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum S3Error {
    #[error("object not found: {0}")]
    NotFound(String),

    #[error("precondition failed (If-Match / If-None-Match)")]
    PreconditionFailed,

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl S3 {
    pub async fn from_config(cfg: &Config) -> Result<Self> {
        let creds = Credentials::new(
            cfg.s3_access_key.clone(),
            cfg.s3_secret_key.clone(),
            None,
            None,
            "rawdb-static",
        );

        let mut loader = aws_config::defaults(BehaviorVersion::latest())
            .region(Region::new(cfg.s3_region.clone()))
            .credentials_provider(SharedCredentialsProvider::new(creds));
        if let Some(ep) = cfg.s3_endpoint.as_deref() {
            loader = loader.endpoint_url(ep);
        }
        let sdk_cfg = loader.load().await;

        let s3_cfg = aws_sdk_s3::config::Builder::from(&sdk_cfg)
            .force_path_style(cfg.s3_path_style)
            .build();

        Ok(Self {
            client: Client::from_conf(s3_cfg),
            bucket: cfg.s3_bucket.clone(),
            presign_ttl: Duration::from_secs(cfg.presign_ttl_secs),
            conditional_writes: cfg.s3_conditional_writes,
        })
    }

    pub fn bucket(&self) -> &str {
        &self.bucket
    }

    /// Whether `If-Match` conditional PUTs are usable on this backend.
    pub fn conditional_writes(&self) -> bool {
        self.conditional_writes
    }

    // -- listing -------------------------------------------------------------

    /// Return immediate "subdirectories" under `prefix` (S3 `CommonPrefixes`),
    /// each ending with the `/` delimiter.
    pub async fn list_common_prefixes(&self, prefix: &str) -> Result<Vec<String>> {
        let mut out = Vec::new();
        let mut req = self
            .client
            .list_objects_v2()
            .bucket(&self.bucket)
            .prefix(prefix)
            .delimiter("/")
            .into_paginator()
            .send();

        while let Some(page) = req.next().await {
            let page = page.map_err(|e| sdk_error("ListObjectsV2 (delimited)", prefix, e))?;
            for cp in page.common_prefixes() {
                if let Some(p) = cp.prefix() {
                    out.push(p.to_string());
                }
            }
        }
        Ok(out)
    }

    /// List all objects (recursive) under `prefix`.
    pub async fn list_objects(&self, prefix: &str) -> Result<Vec<ObjectInfo>> {
        let mut out = Vec::new();
        let mut req = self
            .client
            .list_objects_v2()
            .bucket(&self.bucket)
            .prefix(prefix)
            .into_paginator()
            .send();

        while let Some(page) = req.next().await {
            let page = page.map_err(|e| sdk_error("ListObjectsV2 (recursive)", prefix, e))?;
            for obj in page.contents() {
                let Some(key) = obj.key() else { continue };
                out.push(ObjectInfo {
                    key: key.to_string(),
                    size: obj.size().unwrap_or(0).max(0) as u64,
                    etag: obj.e_tag().map(strip_etag_quotes),
                    last_modified: obj
                        .last_modified()
                        .and_then(|t| DateTime::<Utc>::from_timestamp(t.secs(), t.subsec_nanos())),
                });
            }
        }
        Ok(out)
    }

    // -- single-object ops ---------------------------------------------------

    pub async fn head(&self, key: &str) -> Result<Option<HeadInfo>> {
        match self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
        {
            Ok(out) => Ok(Some(HeadInfo {
                size: out.content_length().unwrap_or(0).max(0) as u64,
                etag: out.e_tag().map(strip_etag_quotes),
                last_modified: out
                    .last_modified()
                    .and_then(|t| DateTime::<Utc>::from_timestamp(t.secs(), t.subsec_nanos())),
                content_type: out.content_type().map(String::from),
            })),
            Err(e) => {
                if is_not_found(&e) {
                    Ok(None)
                } else {
                    Err(sdk_error("HeadObject", key, e))
                }
            }
        }
    }

    /// Open an object as a streaming GET. The caller is expected to forward
    /// the bytes straight to the HTTP response without buffering, which is
    /// what the "stream" download mode does.
    pub async fn get_stream(&self, key: &str) -> Result<GetStream, S3Error> {
        self.get_stream_range(key, None).await
    }

    /// Like [`get_stream`], but optionally starts at `start` bytes into the
    /// object via `Range: bytes=<start>-`. Used to resume a streaming read
    /// after a mid-transfer error without redoing the prefix we already
    /// hashed.
    pub async fn get_stream_range(
        &self,
        key: &str,
        start: Option<u64>,
    ) -> Result<GetStream, S3Error> {
        let mut req = self.client.get_object().bucket(&self.bucket).key(key);
        if let Some(s) = start {
            req = req.range(format!("bytes={s}-"));
        }
        let resp = req.send().await.map_err(|e| {
            if is_not_found_get(&e) {
                S3Error::NotFound(key.into())
            } else {
                S3Error::Other(sdk_error("GetObject", key, e))
            }
        })?;
        Ok(GetStream {
            content_length: resp.content_length(),
            content_type: resp.content_type().map(String::from),
            etag: resp.e_tag().map(strip_etag_quotes),
            body: resp.body,
        })
    }

    /// Fetch a small object entirely into memory. Suitable for `rawdb-meta.toml`
    /// and `_system/users.toml`. Returns `(bytes, etag)`.
    pub async fn get_bytes(&self, key: &str) -> Result<(Vec<u8>, Option<String>), S3Error> {
        let resp = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| {
                if is_not_found_get(&e) {
                    S3Error::NotFound(key.into())
                } else {
                    S3Error::Other(sdk_error("GetObject", key, e))
                }
            })?;

        let etag = resp.e_tag().map(strip_etag_quotes);
        let body = resp
            .body
            .collect()
            .await
            .map_err(|e| S3Error::Other(anyhow::anyhow!("collect body {key}: {e}")))?
            .into_bytes()
            .to_vec();
        Ok((body, etag))
    }

    /// PUT a small object from memory. Optional `if_match` / `if_none_match`
    /// support conditional writes (returns `PreconditionFailed` on 412).
    pub async fn put_bytes(
        &self,
        key: &str,
        body: Vec<u8>,
        content_type: Option<&str>,
        if_match: Option<&str>,
        if_none_match: Option<&str>,
    ) -> Result<String, S3Error> {
        let mut req = self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(ByteStream::from(body));
        if let Some(ct) = content_type {
            req = req.content_type(ct);
        }
        if let Some(m) = if_match {
            req = req.if_match(m);
        }
        if let Some(m) = if_none_match {
            req = req.if_none_match(m);
        }
        let out = req.send().await.map_err(|e| {
            if is_precondition_failed(&e) {
                S3Error::PreconditionFailed
            } else {
                S3Error::Other(sdk_error("PutObject", key, e))
            }
        })?;
        Ok(out.e_tag().map(strip_etag_quotes).unwrap_or_default())
    }

    pub async fn copy(&self, src_key: &str, dst_key: &str) -> Result<()> {
        let source = format!(
            "{}/{}",
            self.bucket,
            url_encode_key(src_key)
        );
        self.client
            .copy_object()
            .bucket(&self.bucket)
            .key(dst_key)
            .copy_source(&source)
            .send()
            .await
            .map_err(|e| sdk_error("CopyObject", &format!("{src_key} -> {dst_key}"), e))?;
        Ok(())
    }

    pub async fn delete(&self, key: &str) -> Result<()> {
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| sdk_error("DeleteObject", key, e))?;
        Ok(())
    }

    /// Recursively delete everything under `prefix`. Uses batched DeleteObjects.
    pub async fn delete_prefix(&self, prefix: &str) -> Result<usize> {
        let objs = self.list_objects(prefix).await?;
        let mut deleted = 0usize;
        for chunk in objs.chunks(1000) {
            let ids: Vec<ObjectIdentifier> = chunk
                .iter()
                .map(|o| {
                    ObjectIdentifier::builder()
                        .key(&o.key)
                        .build()
                        .expect("ObjectIdentifier requires key")
                })
                .collect();
            let del = Delete::builder()
                .set_objects(Some(ids))
                .quiet(true)
                .build()
                .expect("Delete requires objects");
            self.client
                .delete_objects()
                .bucket(&self.bucket)
                .delete(del)
                .send()
                .await
                .map_err(|e| sdk_error("DeleteObjects under", prefix, e))?;
            deleted += chunk.len();
        }
        Ok(deleted)
    }

    // -- presigning ----------------------------------------------------------

    pub async fn presign_get(&self, key: &str) -> Result<String> {
        let presigning = PresigningConfig::expires_in(self.presign_ttl)?;
        let req = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .presigned(presigning)
            .await?;
        Ok(req.uri().to_string())
    }

    pub async fn presign_put(&self, key: &str) -> Result<String> {
        let presigning = PresigningConfig::expires_in(self.presign_ttl)?;
        let req = self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .presigned(presigning)
            .await?;
        Ok(req.uri().to_string())
    }

    // -- multipart upload ----------------------------------------------------
    //
    // Some S3 backends (notably Hetzner) fail large single PUTs; the
    // presigned upload path uses multipart for big files. The client PUTs
    // each part to a presigned URL, captures the per-part ETag, then calls
    // `complete_multipart_upload` with the (part_number, etag) list.

    /// Initiate a multipart upload; returns the S3 `UploadId`.
    pub async fn create_multipart_upload(&self, key: &str) -> Result<String> {
        let out = self
            .client
            .create_multipart_upload()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| sdk_error("CreateMultipartUpload", key, e))?;
        out.upload_id()
            .map(String::from)
            .ok_or_else(|| anyhow::anyhow!("CreateMultipartUpload {key}: no upload id"))
    }

    /// Presigned URL for a single `UploadPart` (1-based `part_number`).
    pub async fn presign_upload_part(
        &self,
        key: &str,
        upload_id: &str,
        part_number: i32,
    ) -> Result<String> {
        let presigning = PresigningConfig::expires_in(self.presign_ttl)?;
        let req = self
            .client
            .upload_part()
            .bucket(&self.bucket)
            .key(key)
            .upload_id(upload_id)
            .part_number(part_number)
            .presigned(presigning)
            .await?;
        Ok(req.uri().to_string())
    }

    /// Finalize a multipart upload. `parts` is `(part_number, etag)` in any
    /// order; they're sorted by part number as S3 requires.
    pub async fn complete_multipart_upload(
        &self,
        key: &str,
        upload_id: &str,
        mut parts: Vec<(i32, String)>,
    ) -> Result<()> {
        parts.sort_by_key(|(n, _)| *n);
        let completed: Vec<CompletedPart> = parts
            .into_iter()
            .map(|(n, etag)| {
                CompletedPart::builder()
                    .part_number(n)
                    .e_tag(etag)
                    .build()
            })
            .collect();
        let mpu = CompletedMultipartUpload::builder()
            .set_parts(Some(completed))
            .build();
        self.client
            .complete_multipart_upload()
            .bucket(&self.bucket)
            .key(key)
            .upload_id(upload_id)
            .multipart_upload(mpu)
            .send()
            .await
            .map_err(|e| sdk_error("CompleteMultipartUpload", key, e))?;
        Ok(())
    }

    /// Abort a multipart upload, discarding any uploaded parts. Called on
    /// the failure path so incomplete uploads don't linger and accrue
    /// storage cost.
    pub async fn abort_multipart_upload(&self, key: &str, upload_id: &str) -> Result<()> {
        self.client
            .abort_multipart_upload()
            .bucket(&self.bucket)
            .key(key)
            .upload_id(upload_id)
            .send()
            .await
            .map_err(|e| sdk_error("AbortMultipartUpload", key, e))?;
        Ok(())
    }
}

// ---- helpers ---------------------------------------------------------------

/// Build an error message from an SDK error that keeps the SDK's own
/// `Display` text (`service error`, `dispatch failure`, …) and appends
/// what the server actually said, which that text omits:
///
/// `<op> <target>: service error (HTTP 403, AccessDenied: Access Denied, request id …)`
///
/// A HEAD response has no body, so for HeadObject only the status and
/// request id are available. For transport-level failures (timeout,
/// dispatch, construction) the underlying cause chain is appended
/// instead. The message is self-contained, so the SDK error isn't kept
/// as a source — `{e:#}` would otherwise repeat "service error".
fn sdk_error<E>(op: &str, target: &str, e: SdkError<E, HttpResponse>) -> anyhow::Error
where
    E: ProvideErrorMetadata + std::error::Error + Send + Sync + 'static,
{
    let mut details: Vec<String> = Vec::new();
    match &e {
        SdkError::ServiceError(svc) => {
            details.push(format!("HTTP {}", svc.raw().status().as_u16()));
            match (svc.err().code(), svc.err().message()) {
                (Some(code), Some(msg)) => details.push(format!("{code}: {msg}")),
                (Some(code), None) => details.push(code.to_string()),
                (None, Some(msg)) => details.push(msg.to_string()),
                (None, None) => {}
            }
            if let Some(id) = svc.raw().headers().get("x-amz-request-id") {
                details.push(format!("request id {id}"));
            }
        }
        SdkError::ResponseError(r) => {
            details.push(format!("HTTP {}", r.raw().status().as_u16()));
        }
        _ => {
            let mut cause = std::error::Error::source(&e);
            let mut chain = Vec::new();
            while let Some(c) = cause {
                chain.push(c.to_string());
                cause = c.source();
            }
            if !chain.is_empty() {
                details.push(chain.join(": "));
            }
        }
    }
    if details.is_empty() {
        anyhow::anyhow!("{op} {target}: {e}")
    } else {
        anyhow::anyhow!("{op} {target}: {e} ({})", details.join(", "))
    }
}

fn strip_etag_quotes(s: &str) -> String {
    s.trim_matches('"').to_string()
}

fn url_encode_key(key: &str) -> String {
    use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};
    // Slashes must be preserved as path separators in CopySource.
    const SET: &AsciiSet = &CONTROLS
        .add(b' ')
        .add(b'"')
        .add(b'#')
        .add(b'<')
        .add(b'>')
        .add(b'?')
        .add(b'`')
        .add(b'{')
        .add(b'}');
    utf8_percent_encode(key, SET).to_string()
}

fn is_not_found(
    err: &aws_sdk_s3::error::SdkError<aws_sdk_s3::operation::head_object::HeadObjectError>,
) -> bool {
    use aws_sdk_s3::error::SdkError;
    if let SdkError::ServiceError(svc) = err {
        return matches!(
            svc.err(),
            aws_sdk_s3::operation::head_object::HeadObjectError::NotFound(_)
        );
    }
    false
}

fn is_not_found_get(
    err: &aws_sdk_s3::error::SdkError<aws_sdk_s3::operation::get_object::GetObjectError>,
) -> bool {
    use aws_sdk_s3::error::SdkError;
    if let SdkError::ServiceError(svc) = err {
        return matches!(
            svc.err(),
            aws_sdk_s3::operation::get_object::GetObjectError::NoSuchKey(_)
        );
    }
    false
}

fn is_precondition_failed(
    err: &aws_sdk_s3::error::SdkError<aws_sdk_s3::operation::put_object::PutObjectError>,
) -> bool {
    use aws_sdk_s3::error::SdkError;
    if let SdkError::ServiceError(svc) = err {
        if let Some(meta) = svc.err().meta().code() {
            return meta == "PreconditionFailed";
        }
        return svc.raw().status().as_u16() == 412;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_sdk_s3::error::{ConnectorError, ErrorMetadata};
    use aws_sdk_s3::operation::get_object::GetObjectError;
    use aws_sdk_s3::operation::head_object::HeadObjectError;
    use aws_sdk_s3::primitives::SdkBody;
    use aws_smithy_runtime_api::http::StatusCode;

    fn raw(status: u16, request_id: Option<&'static str>) -> HttpResponse {
        let mut r = HttpResponse::new(StatusCode::try_from(status).unwrap(), SdkBody::empty());
        if let Some(id) = request_id {
            r.headers_mut().insert("x-amz-request-id", id);
        }
        r
    }

    #[test]
    fn head_error_keeps_display_and_adds_status_and_request_id() {
        // HEAD responses carry no body, so no S3 code/message.
        let e = SdkError::service_error(
            HeadObjectError::generic(ErrorMetadata::builder().build()),
            raw(403, Some("REQ123")),
        );
        let msg = sdk_error("HeadObject", "samples/Nikon/Coolpix P330/rawdb-meta.toml", e).to_string();
        assert_eq!(
            msg,
            "HeadObject samples/Nikon/Coolpix P330/rawdb-meta.toml: service error (HTTP 403, request id REQ123)"
        );
    }

    #[test]
    fn get_error_includes_s3_code_and_message() {
        let e = SdkError::service_error(
            GetObjectError::generic(
                ErrorMetadata::builder()
                    .code("SlowDown")
                    .message("Please reduce your request rate.")
                    .build(),
            ),
            raw(503, None),
        );
        let msg = sdk_error("GetObject", "a/b", e).to_string();
        assert_eq!(
            msg,
            "GetObject a/b: service error (HTTP 503, SlowDown: Please reduce your request rate.)"
        );
    }

    #[test]
    fn dispatch_failure_appends_cause_chain() {
        let io = std::io::Error::new(std::io::ErrorKind::ConnectionReset, "connection reset by peer");
        let e: SdkError<HeadObjectError, HttpResponse> =
            SdkError::dispatch_failure(ConnectorError::io(Box::new(io)));
        let msg = sdk_error("HeadObject", "a/b", e).to_string();
        assert!(msg.starts_with("HeadObject a/b: dispatch failure ("), "{msg}");
        assert!(msg.contains("connection reset by peer"), "{msg}");
    }
}
