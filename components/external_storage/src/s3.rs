// Copyright 2019 TiKV Project Authors. Licensed under Apache-2.0.

use std::io::{Error, ErrorKind, Result};

use futures::stream::StreamExt;
use futures_io::AsyncRead;
use tokio::runtime::Runtime;
use tokio_util::{
    codec::{BytesCodec, FramedRead},
    compat::{FuturesAsyncReadCompatExt, Tokio02AsyncReadCompatExt},
};

use rusoto_core::region;
use rusoto_core::request::DispatchSignedRequest;
use rusoto_core::request::{HttpClient, HttpConfig};
use rusoto_core::{ByteStream, RusotoError};
use rusoto_credential::{DefaultCredentialsProvider, StaticProvider};
use rusoto_s3::*;

use super::ExternalStorage;
use kvproto::backup::S3 as Config;

/// S3 compatible storage
#[derive(Clone)]
pub struct S3Storage {
    config: Config,
    client: S3Client,
}

impl S3Storage {
    /// Create a new S3 storage for the given config.
    pub fn new(config: &Config) -> Result<S3Storage> {
        // This can greatly improve performance dealing with payloads greater
        // than 100MB. See https://github.com/rusoto/rusoto/pull/1227
        // for more information.
        let mut http_config = HttpConfig::new();
        http_config.read_buf_size(1024 * 1024 * 2);
        let http_dispatcher = HttpClient::new_with_config(http_config).unwrap();

        S3Storage::with_request_dispatcher(config, http_dispatcher)
    }

    fn with_request_dispatcher<D>(config: &Config, dispatcher: D) -> Result<S3Storage>
    where
        D: DispatchSignedRequest + Send + Sync + 'static,
    {
        if config.bucket.is_empty() {
            return Err(Error::new(ErrorKind::InvalidInput, "missing bucket name"));
        }
        let region = if config.endpoint.is_empty() {
            config.region.parse::<region::Region>().map_err(|e| {
                Error::new(
                    ErrorKind::InvalidInput,
                    format!("invalid region format {}: {}", config.region, e),
                )
            })?
        } else {
            region::Region::Custom {
                name: config.region.clone(),
                endpoint: config.endpoint.clone(),
            }
        };
        let client = if config.access_key.is_empty() || config.secret_access_key.is_empty() {
            let cred_provider = DefaultCredentialsProvider::new().map_err(|e| {
                Error::new(
                    ErrorKind::PermissionDenied,
                    format!("unable to get credentials: {}", e),
                )
            })?;
            S3Client::new_with(dispatcher, cred_provider, region)
        } else {
            let cred_provider = StaticProvider::new(
                config.access_key.clone(),
                config.secret_access_key.clone(),
                None, /* token */
                None, /* valid_for */
            );
            S3Client::new_with(dispatcher, cred_provider, region)
        };
        Ok(S3Storage {
            config: config.clone(),
            client,
        })
    }

    fn maybe_prefix_key(&self, key: &str) -> String {
        if !self.config.prefix.is_empty() {
            return format!("{}/{}", self.config.prefix, key);
        }
        key.to_owned()
    }
}

impl ExternalStorage for S3Storage {
    fn write(
        &self,
        name: &str,
        reader: Box<dyn AsyncRead + Sync + Send + Unpin>,
        content_length: u64,
    ) -> Result<()> {
        let key = self.maybe_prefix_key(name);
        debug!("save file to s3 storage"; "key" => %key);
        let get_var = |s: &String| {
            if s.is_empty() {
                None
            } else {
                Some(s.clone())
            }
        };
        let req = PutObjectRequest {
            key,
            bucket: self.config.bucket.clone(),
            body: Some(ByteStream::new(
                FramedRead::new(reader.compat(), BytesCodec::new())
                    .map(|bytes| Ok(bytes?.freeze())),
            )),
            content_length: Some(content_length as i64),
            acl: get_var(&self.config.acl),
            server_side_encryption: get_var(&self.config.sse),
            storage_class: get_var(&self.config.storage_class),
            ..Default::default()
        };
        let mut runtime = Runtime::new().map_err(|e| {
            Error::new(
                ErrorKind::Other,
                format!("failed to create tokio runtime {}", e),
            )
        })?;
        runtime
            .block_on(self.client.put_object(req))
            .map(|_| ())
            .map_err(|e| Error::new(ErrorKind::Other, format!("failed to put object {}", e)))
    }

    fn read(&self, name: &str) -> Result<Box<dyn AsyncRead + Unpin>> {
        let key = self.maybe_prefix_key(name);
        debug!("read file from s3 storage"; "key" => %key);
        let req = GetObjectRequest {
            key,
            bucket: self.config.bucket.clone(),
            ..Default::default()
        };
        let mut runtime = Runtime::new().map_err(|e| {
            Error::new(
                ErrorKind::Other,
                format!("failed to create tokio runtime {}", e),
            )
        })?;
        runtime
            .block_on(self.client.get_object(req))
            .map(|out| Box::new(out.body.unwrap().into_async_read().compat()) as _)
            .map_err(|e| match e {
                RusotoError::Service(GetObjectError::NoSuchKey(key)) => Error::new(
                    ErrorKind::NotFound,
                    format!("no key {} at bucket {}", key, self.config.bucket),
                ),
                e => Error::new(ErrorKind::Other, format!("failed to get object {}", e)),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::io::AsyncReadExt;
    use rusoto_core::signature::SignedRequest;
    use rusoto_mock::MockRequestDispatcher;

    #[test]
    fn test_s3_config() {
        let config = Config {
            region: "ap-southeast-2".to_string(),
            bucket: "mybucket".to_string(),
            prefix: "myprefix".to_string(),
            access_key: "abc".to_string(),
            secret_access_key: "xyz".to_string(),
            ..Default::default()
        };
        let cases = vec![
            // missing both region and endpoint
            Config {
                region: "".to_string(),
                ..config.clone()
            },
        ];
        for case in cases {
            let dispatcher = MockRequestDispatcher::with_status(200);
            let r = S3Storage::with_request_dispatcher(&case, dispatcher);
            assert!(r.is_err());
        }
        let dispatcher = MockRequestDispatcher::with_status(200);
        assert!(S3Storage::with_request_dispatcher(&config, dispatcher).is_ok());
    }

    #[test]
    fn test_s3_storage() {
        let magic_contents = "5678";
        let config = Config {
            region: "ap-southeast-2".to_string(),
            bucket: "mybucket".to_string(),
            prefix: "myprefix".to_string(),
            access_key: "abc".to_string(),
            secret_access_key: "xyz".to_string(),
            ..Default::default()
        };
        let dispatcher = MockRequestDispatcher::with_status(200).with_request_checker(
            move |req: &SignedRequest| {
                assert_eq!(req.region.name(), "ap-southeast-2");
                assert_eq!(req.path(), "/mybucket/myprefix/mykey");
                // PutObject is translated to HTTP PUT.
                assert_eq!(req.payload.is_some(), req.method() == "PUT");
            },
        );
        let s = S3Storage::with_request_dispatcher(&config, dispatcher).unwrap();
        s.write(
            "mykey",
            Box::new(magic_contents.as_bytes()),
            magic_contents.len() as u64,
        )
        .unwrap();
        let mut reader = s.read("mykey").unwrap();
        let mut buf = Vec::new();
        let ret = futures::executor::block_on(reader.read_to_end(&mut buf));
        assert!(ret.unwrap() == 0);
        assert!(buf.is_empty());
    }

    #[test]
    #[cfg(FALSE)]
    // FIXME: enable this (or move this to an integration test) if we've got a
    // reliable way to test s3 (rusoto_mock requires custom logic to verify the
    // body stream which itself can have bug)
    fn test_real_s3_storage() {
        use std::f64::INFINITY;
        use tikv_util::time::Limiter;

        let mut s3 = Config::default();
        s3.set_endpoint("http://127.0.0.1:9000".to_owned());
        s3.set_bucket("bucket".to_owned());
        s3.set_prefix("prefix".to_owned());
        s3.set_access_key("93QZ01QRBYQQXC37XHZV".to_owned());
        s3.set_secret_access_key("N2VcI4Emg0Nm7fDzGBMJvguHHUxLGpjfwt2y4+vJ".to_owned());
        s3.set_force_path_style(true);

        let limiter = Limiter::new(INFINITY);

        let storage = S3Storage::new(&s3).unwrap();
        const LEN: usize = 1024 * 1024 * 4;
        static CONTENT: [u8; LEN] = [50_u8; LEN];
        storage
            .write(
                "huge_file",
                Box::new(limiter.limit(&CONTENT[..])),
                LEN as u64,
            )
            .unwrap();

        let mut reader = storage.read("huge_file").unwrap();
        let mut buf = Vec::new();
        futures::executor::block_on(reader.read_to_end(&mut buf)).unwrap();
        assert_eq!(buf.len(), LEN);
        assert_eq!(buf.iter().position(|b| *b != 50_u8), None);
    }
}
