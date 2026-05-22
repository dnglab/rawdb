//! Thin async wrapper over `aws-sdk-s3` for the operations RawDB needs:
//! listing, HEAD, GET (small + streamed), PUT (with optional If-Match /
//! If-None-Match for conditional writes), COPY, DELETE, and presigning.
//!
//! Works with Garage, Cloudflare R2, B2, SeaweedFS, etc. via the
//! `endpoint_url` + path-style addressing toggle. Credentials are supplied
//! explicitly from `RAWDB_S3_*` config — we never read the AWS env chain.

use std::time::Duration;

use anyhow::{Context, Result};
use aws_config::BehaviorVersion;
use aws_credential_types::Credentials;
use aws_sdk_s3::config::{Region, SharedCredentialsProvider};
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::presigning::PresigningConfig;
use aws_sdk_s3::types::{
    CompletedMultipartUpload, CompletedPart, Delete, ObjectIdentifier,
};
use aws_sdk_s3::Client;
use chrono::{DateTime, Utc};

use crate::config::Config;

#[derive(Clone)]
pub struct S3 {
    client: Client,
    bucket: String,
    presign_ttl: Duration,
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
        })
    }

    pub fn bucket(&self) -> &str {
        &self.bucket
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
            let page = page.context("ListObjectsV2 (delimited)")?;
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
            let page = page.context("ListObjectsV2 (recursive)")?;
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
                    Err(anyhow::anyhow!("HeadObject {key}: {e}"))
                }
            }
        }
    }

    /// Open an object as a streaming GET. The caller is expected to forward
    /// the bytes straight to the HTTP response without buffering, which is
    /// what the "stream" download mode does.
    pub async fn get_stream(&self, key: &str) -> Result<GetStream, S3Error> {
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
                    S3Error::Other(anyhow::anyhow!("GetObject {key}: {e}"))
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
                    S3Error::Other(anyhow::anyhow!("GetObject {key}: {e}"))
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
                S3Error::Other(anyhow::anyhow!("PutObject {key}: {e}"))
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
            .with_context(|| format!("CopyObject {src_key} -> {dst_key}"))?;
        Ok(())
    }

    pub async fn delete(&self, key: &str) -> Result<()> {
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .with_context(|| format!("DeleteObject {key}"))?;
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
                .with_context(|| format!("DeleteObjects under {prefix}"))?;
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
            .with_context(|| format!("CreateMultipartUpload {key}"))?;
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
            .with_context(|| format!("CompleteMultipartUpload {key}"))?;
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
            .with_context(|| format!("AbortMultipartUpload {key}"))?;
        Ok(())
    }
}

// ---- helpers ---------------------------------------------------------------

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
