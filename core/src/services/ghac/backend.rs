// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

use std::collections::HashMap;
use std::env;

use async_trait::async_trait;
use bytes::Bytes;
use http::header::ACCEPT;
use http::header::AUTHORIZATION;
use http::header::CONTENT_LENGTH;
use http::header::CONTENT_RANGE;
use http::header::CONTENT_TYPE;
use http::header::USER_AGENT;
use http::Request;
use http::Response;
use http::StatusCode;
use log::debug;
use serde::Deserialize;
use serde::Serialize;

use super::error::parse_error;
use super::writer::GhacWriter;
use crate::raw::*;
use crate::*;

/// The base url for cache url.
const CACHE_URL_BASE: &str = "_apis/artifactcache";
/// Cache API requires to provide an accept header.
const CACHE_HEADER_ACCEPT: &str = "application/json;api-version=6.0-preview.1";
/// The cache url env for ghac.
///
/// The url will be like `https://artifactcache.actions.githubusercontent.com/<id>/`
const ACTIONS_CACHE_URL: &str = "ACTIONS_CACHE_URL";
/// The runtime token env for ghac.
///
/// This token will be valid for 6h and github action will running for 6
/// hours at most. So we don't need to refetch it again.
const ACTIONS_RUNTIME_TOKEN: &str = "ACTIONS_RUNTIME_TOKEN";
/// The token provided by workflow;
const GITHUB_TOKEN: &str = "GITHUB_TOKEN";
/// The github api url for ghac.
const GITHUB_API_URL: &str = "GITHUB_API_URL";
/// The repository that runs this action.
const GITHUB_REPOSITORY: &str = "GITHUB_REPOSITORY";
/// The github API version that used by OpenDAL.
const GITHUB_API_VERSION: &str = "2022-11-28";

fn value_or_env(
    explicit_value: Option<String>,
    env_var_name: &str,
    operation: &'static str,
) -> Result<String> {
    if let Some(value) = explicit_value {
        return Ok(value);
    }

    env::var(env_var_name).map_err(|err| {
        let text = format!(
            "{} not found, maybe not in github action environment?",
            env_var_name
        );
        Error::new(ErrorKind::ConfigInvalid, &text)
            .with_operation(operation)
            .set_source(err)
    })
}

/// GitHub Action Cache Services support.
#[doc = include_str!("docs.md")]
#[derive(Debug, Default)]
pub struct GhacBuilder {
    root: Option<String>,
    version: Option<String>,
    endpoint: Option<String>,
    runtime_token: Option<String>,

    http_client: Option<HttpClient>,
}

impl GhacBuilder {
    /// set the working directory root of backend
    pub fn root(&mut self, root: &str) -> &mut Self {
        if !root.is_empty() {
            self.root = Some(root.to_string())
        }

        self
    }

    /// set the version that used by cache.
    ///
    /// The version is the unique value that provides namespacing.
    /// It's better to make sure this value is only used by this backend.
    ///
    /// If not set, we will use `opendal` as default.
    pub fn version(&mut self, version: &str) -> &mut Self {
        if !version.is_empty() {
            self.version = Some(version.to_string())
        }

        self
    }

    /// Set the endpoint for ghac service.
    ///
    /// For example, this is provided as the `ACTIONS_CACHE_URL` environment variable by the GHA runner.
    ///
    /// Default: the value of the `ACTIONS_CACHE_URL` environment variable.
    pub fn endpoint(&mut self, endpoint: &str) -> &mut Self {
        if !endpoint.is_empty() {
            self.endpoint = Some(endpoint.to_string())
        }
        self
    }

    /// Set the runtime token for ghac service.
    ///
    /// For example, this is provided as the `ACTIONS_RUNTIME_TOKEN` environment variable by the GHA
    /// runner.
    ///
    /// Default: the value of the `ACTIONS_RUNTIME_TOKEN` environment variable.
    pub fn runtime_token(&mut self, runtime_token: &str) -> &mut Self {
        if !runtime_token.is_empty() {
            self.runtime_token = Some(runtime_token.to_string())
        }
        self
    }

    /// Specify the http client that used by this service.
    ///
    /// # Notes
    ///
    /// This API is part of OpenDAL's Raw API. `HttpClient` could be changed
    /// during minor updates.
    pub fn http_client(&mut self, client: HttpClient) -> &mut Self {
        self.http_client = Some(client);
        self
    }
}

impl Builder for GhacBuilder {
    const SCHEME: Scheme = Scheme::Ghac;
    type Accessor = GhacBackend;

    fn from_map(map: HashMap<String, String>) -> Self {
        let mut builder = GhacBuilder::default();

        map.get("root").map(|v| builder.root(v));
        map.get("version").map(|v| builder.version(v));

        builder
    }

    fn build(&mut self) -> Result<Self::Accessor> {
        debug!("backend build started: {:?}", self);

        let root = normalize_root(&self.root.take().unwrap_or_default());
        debug!("backend use root {}", root);

        let client = if let Some(client) = self.http_client.take() {
            client
        } else {
            HttpClient::new().map_err(|err| {
                err.with_operation("Builder::build")
                    .with_context("service", Scheme::Ghac)
            })?
        };

        let backend = GhacBackend {
            root,

            cache_url: value_or_env(self.endpoint.take(), ACTIONS_CACHE_URL, "Builder::build")?,
            catch_token: value_or_env(
                self.runtime_token.take(),
                ACTIONS_RUNTIME_TOKEN,
                "Builder::build",
            )?,
            version: self
                .version
                .clone()
                .unwrap_or_else(|| "opendal".to_string()),

            api_url: env::var(GITHUB_API_URL)
                .unwrap_or_else(|_| "https://api.github.com".to_string()),
            api_token: env::var(GITHUB_TOKEN).unwrap_or_default(),
            repo: env::var(GITHUB_REPOSITORY).unwrap_or_default(),

            client,
        };

        Ok(backend)
    }
}

/// Backend for github action cache services.
#[derive(Debug, Clone)]
pub struct GhacBackend {
    // root should end with "/"
    root: String,

    cache_url: String,
    catch_token: String,
    version: String,

    api_url: String,
    api_token: String,
    repo: String,

    pub client: HttpClient,
}

#[async_trait]
impl Accessor for GhacBackend {
    type Reader = IncomingAsyncBody;
    type BlockingReader = ();
    type Writer = GhacWriter;
    type BlockingWriter = ();
    type Pager = ();
    type BlockingPager = ();

    fn info(&self) -> AccessorInfo {
        let mut am = AccessorInfo::default();
        am.set_scheme(Scheme::Ghac)
            .set_root(&self.root)
            .set_name(&self.version)
            .set_native_capability(Capability {
                stat: true,

                read: true,
                read_can_next: true,
                read_with_range: true,

                write: true,
                write_can_multi: true,
                create_dir: true,
                delete: true,

                ..Default::default()
            });
        am
    }

    async fn create_dir(&self, path: &str, _: OpCreateDir) -> Result<RpCreateDir> {
        // ignore creation of dir.
        if path.ends_with('/') {
            return Ok(RpCreateDir::default());
        }

        let req = self.ghac_reserve(path).await?;

        let resp = self.client.send(req).await?;

        let cache_id = if resp.status().is_success() {
            let slc = resp.into_body().bytes().await?;
            let reserve_resp: GhacReserveResponse =
                serde_json::from_slice(&slc).map_err(new_json_deserialize_error)?;
            reserve_resp.cache_id
        } else if resp.status().as_u16() == StatusCode::CONFLICT {
            // If the file is already exist, just return Ok.
            return Ok(RpCreateDir::default());
        } else {
            return Err(parse_error(resp)
                .await
                .map(|err| err.with_operation("Backend::ghac_reserve"))?);
        };

        // Write only 1 byte to allow create.
        let req = self
            .ghac_upload(cache_id, 0, 1, AsyncBody::Bytes(Bytes::from_static(&[0])))
            .await?;

        let resp = self.client.send(req).await?;

        if resp.status().is_success() {
            resp.into_body().consume().await?;
        } else {
            return Err(parse_error(resp)
                .await
                .map(|err| err.with_operation("Backend::ghac_upload"))?);
        }

        let req = self.ghac_commit(cache_id, 1).await?;
        let resp = self.client.send(req).await?;

        if resp.status().is_success() {
            resp.into_body().consume().await?;
            Ok(RpCreateDir::default())
        } else {
            Err(parse_error(resp)
                .await
                .map(|err| err.with_operation("Backend::ghac_commit"))?)
        }
    }

    async fn read(&self, path: &str, args: OpRead) -> Result<(RpRead, Self::Reader)> {
        let req = self.ghac_query(path).await?;

        let resp = self.client.send(req).await?;

        let location = if resp.status() == StatusCode::OK {
            let slc = resp.into_body().bytes().await?;
            let query_resp: GhacQueryResponse =
                serde_json::from_slice(&slc).map_err(new_json_deserialize_error)?;
            query_resp.archive_location
        } else {
            return Err(parse_error(resp).await?);
        };

        let req = self.ghac_get_location(&location, args.range()).await?;
        let resp = self.client.send(req).await?;

        let status = resp.status();
        match status {
            StatusCode::OK | StatusCode::PARTIAL_CONTENT => {
                let size = parse_content_length(resp.headers())?;
                Ok((RpRead::new().with_size(size), resp.into_body()))
            }
            StatusCode::RANGE_NOT_SATISFIABLE => Ok((RpRead::new(), IncomingAsyncBody::empty())),
            _ => Err(parse_error(resp).await?),
        }
    }

    async fn write(&self, path: &str, _: OpWrite) -> Result<(RpWrite, Self::Writer)> {
        let req = self.ghac_reserve(path).await?;

        let resp = self.client.send(req).await?;

        let cache_id = if resp.status().is_success() {
            let slc = resp.into_body().bytes().await?;
            let reserve_resp: GhacReserveResponse =
                serde_json::from_slice(&slc).map_err(new_json_deserialize_error)?;
            reserve_resp.cache_id
        } else {
            return Err(parse_error(resp)
                .await
                .map(|err| err.with_operation("Backend::ghac_reserve"))?);
        };

        Ok((RpWrite::default(), GhacWriter::new(self.clone(), cache_id)))
    }

    async fn stat(&self, path: &str, _: OpStat) -> Result<RpStat> {
        // Stat root always returns a DIR.
        if path == "/" {
            return Ok(RpStat::new(Metadata::new(EntryMode::DIR)));
        }

        let req = self.ghac_query(path).await?;

        let resp = self.client.send(req).await?;

        let location = if resp.status() == StatusCode::OK {
            let slc = resp.into_body().bytes().await?;
            let query_resp: GhacQueryResponse =
                serde_json::from_slice(&slc).map_err(new_json_deserialize_error)?;
            query_resp.archive_location
        } else if resp.status() == StatusCode::NO_CONTENT && path.ends_with('/') {
            return Ok(RpStat::new(Metadata::new(EntryMode::DIR)));
        } else {
            return Err(parse_error(resp).await?);
        };

        let req = self.ghac_head_location(&location).await?;
        let resp = self.client.send(req).await?;

        let status = resp.status();
        match status {
            StatusCode::OK => {
                let meta = parse_into_metadata(path, resp.headers())?;

                Ok(RpStat::new(meta))
            }
            _ => Err(parse_error(resp).await?),
        }
    }

    async fn delete(&self, path: &str, _: OpDelete) -> Result<RpDelete> {
        if self.api_token.is_empty() {
            return Err(Error::new(
                ErrorKind::PermissionDenied,
                "github token is not configured, delete is permission denied",
            ));
        }

        let resp = self.ghac_delete(path).await?;

        // deleting not existing objects is ok
        if resp.status().is_success() || resp.status() == StatusCode::NOT_FOUND {
            Ok(RpDelete::default())
        } else {
            Err(parse_error(resp).await?)
        }
    }
}

impl GhacBackend {
    async fn ghac_query(&self, path: &str) -> Result<Request<AsyncBody>> {
        let p = build_abs_path(&self.root, path);

        let url = format!(
            "{}{CACHE_URL_BASE}/cache?keys={}&version={}",
            self.cache_url,
            percent_encode_path(&p),
            self.version
        );

        let mut req = Request::get(&url);
        req = req.header(AUTHORIZATION, format!("Bearer {}", self.catch_token));
        req = req.header(ACCEPT, CACHE_HEADER_ACCEPT);

        let req = req
            .body(AsyncBody::Empty)
            .map_err(new_request_build_error)?;

        Ok(req)
    }

    async fn ghac_get_location(
        &self,
        location: &str,
        range: BytesRange,
    ) -> Result<Request<AsyncBody>> {
        let mut req = Request::get(location);

        if !range.is_full() {
            // ghac is backed by azblob, and azblob doesn't support
            // read with suffix range
            //
            // ref: https://learn.microsoft.com/en-us/rest/api/storageservices/specifying-the-range-header-for-blob-service-operations
            if range.offset().is_none() && range.size().is_some() {
                return Err(Error::new(
                    ErrorKind::Unsupported,
                    "ghac doesn't support read with suffix range",
                ));
            }

            req = req.header(http::header::RANGE, range.to_header());
        }

        req.body(AsyncBody::Empty).map_err(new_request_build_error)
    }

    async fn ghac_head_location(&self, location: &str) -> Result<Request<AsyncBody>> {
        Request::head(location)
            .body(AsyncBody::Empty)
            .map_err(new_request_build_error)
    }

    async fn ghac_reserve(&self, path: &str) -> Result<Request<AsyncBody>> {
        let p = build_abs_path(&self.root, path);

        let url = format!("{}{CACHE_URL_BASE}/caches", self.cache_url);

        let bs = serde_json::to_vec(&GhacReserveRequest {
            key: p,
            version: self.version.to_string(),
        })
        .map_err(new_json_serialize_error)?;

        let mut req = Request::post(&url);
        req = req.header(AUTHORIZATION, format!("Bearer {}", self.catch_token));
        req = req.header(ACCEPT, CACHE_HEADER_ACCEPT);
        req = req.header(CONTENT_LENGTH, bs.len());
        req = req.header(CONTENT_TYPE, "application/json");

        let req = req
            .body(AsyncBody::Bytes(Bytes::from(bs)))
            .map_err(new_request_build_error)?;

        Ok(req)
    }

    pub async fn ghac_upload(
        &self,
        cache_id: i64,
        offset: u64,
        size: u64,
        body: AsyncBody,
    ) -> Result<Request<AsyncBody>> {
        let url = format!("{}{CACHE_URL_BASE}/caches/{cache_id}", self.cache_url);

        let mut req = Request::patch(&url);
        req = req.header(AUTHORIZATION, format!("Bearer {}", self.catch_token));
        req = req.header(ACCEPT, CACHE_HEADER_ACCEPT);
        req = req.header(CONTENT_LENGTH, size);
        req = req.header(CONTENT_TYPE, "application/octet-stream");
        req = req.header(
            CONTENT_RANGE,
            BytesContentRange::default()
                .with_range(offset, offset + size - 1)
                .to_header(),
        );

        let req = req.body(body).map_err(new_request_build_error)?;

        Ok(req)
    }

    pub async fn ghac_commit(&self, cache_id: i64, size: u64) -> Result<Request<AsyncBody>> {
        let url = format!("{}{CACHE_URL_BASE}/caches/{cache_id}", self.cache_url);

        let bs =
            serde_json::to_vec(&GhacCommitRequest { size }).map_err(new_json_serialize_error)?;

        let mut req = Request::post(&url);
        req = req.header(AUTHORIZATION, format!("Bearer {}", self.catch_token));
        req = req.header(ACCEPT, CACHE_HEADER_ACCEPT);
        req = req.header(CONTENT_TYPE, "application/json");
        req = req.header(CONTENT_LENGTH, bs.len());

        let req = req
            .body(AsyncBody::Bytes(Bytes::from(bs)))
            .map_err(new_request_build_error)?;

        Ok(req)
    }

    async fn ghac_delete(&self, path: &str) -> Result<Response<IncomingAsyncBody>> {
        let p = build_abs_path(&self.root, path);

        let url = format!(
            "{}/repos/{}/actions/caches?key={}",
            self.api_url,
            self.repo,
            percent_encode_path(&p)
        );

        let mut req = Request::delete(&url);
        req = req.header(AUTHORIZATION, format!("Bearer {}", self.api_token));
        req = req.header(USER_AGENT, format!("opendal/{VERSION} (service ghac)"));
        req = req.header("X-GitHub-Api-Version", GITHUB_API_VERSION);

        let req = req
            .body(AsyncBody::Empty)
            .map_err(new_request_build_error)?;

        self.client.send(req).await
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhacQueryResponse {
    // Not used fields.
    // cache_key: String,
    // scope: String,
    archive_location: String,
}

#[derive(Serialize)]
struct GhacReserveRequest {
    key: String,
    version: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhacReserveResponse {
    cache_id: i64,
}

#[derive(Serialize)]
struct GhacCommitRequest {
    size: u64,
}
