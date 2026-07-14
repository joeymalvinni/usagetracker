use std::path::Path;

use anyhow::{bail, Context};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::time::{sleep, timeout, Duration, Instant};
use usage_core::{
    ApiRequest, ApiResponse, RefreshJob, RefreshJobStatus, RequestEnvelope, ResponseEnvelope,
    API_VERSION, MAX_RESPONSE_BYTES,
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const WRITE_TIMEOUT: Duration = Duration::from_secs(3);
const READ_TIMEOUT: Duration = Duration::from_secs(10);
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(15);
const REFRESH_WAIT_TIMEOUT: Duration = Duration::from_secs(2 * 60);
const REFRESH_POLL_INTERVAL: Duration = Duration::from_millis(250);

pub struct Client<'a> {
    socket: &'a Path,
}

impl<'a> Client<'a> {
    pub fn new(socket: &'a Path) -> Self {
        Self { socket }
    }

    pub async fn request(&self, request: ApiRequest) -> anyhow::Result<ApiResponse> {
        self.request_batch([request])
            .await?
            .pop()
            .context("daemon closed connection without a response")
    }

    pub async fn request_batch(
        &self,
        requests: impl IntoIterator<Item = ApiRequest>,
    ) -> anyhow::Result<Vec<ApiResponse>> {
        let requests = requests.into_iter().collect::<Vec<_>>();
        timeout(EXCHANGE_TIMEOUT, self.request_batch_inner(requests))
            .await
            .with_context(|| {
                format!(
                    "daemon exchange exceeded the {}-second timeout",
                    EXCHANGE_TIMEOUT.as_secs()
                )
            })?
    }

    pub async fn wait_for_refresh(&self, mut job: RefreshJob) -> anyhow::Result<RefreshJob> {
        let deadline = Instant::now() + REFRESH_WAIT_TIMEOUT;
        while !job.status.is_terminal() {
            if Instant::now() >= deadline {
                bail!(
                    "refresh job {} did not finish within {} seconds",
                    job.id,
                    REFRESH_WAIT_TIMEOUT.as_secs()
                );
            }
            sleep(REFRESH_POLL_INTERVAL).await;
            job = match self
                .request(ApiRequest::GetRefreshJob {
                    job_id: job.id.clone(),
                })
                .await?
            {
                ApiResponse::RefreshJob { job } => job,
                other => return unexpected("refresh_job", other),
            };
        }
        if job.status == RefreshJobStatus::Failed {
            bail!(
                "refresh job {} failed: {}",
                job.id,
                job.failure_message.as_deref().unwrap_or("unknown failure")
            );
        }
        Ok(job)
    }

    async fn request_batch_inner(
        &self,
        requests: Vec<ApiRequest>,
    ) -> anyhow::Result<Vec<ApiResponse>> {
        let stream = timeout(CONNECT_TIMEOUT, UnixStream::connect(self.socket))
            .await
            .with_context(|| {
                format!(
                    "timed out connecting to daemon socket {}",
                    self.socket.display()
                )
            })?
            .with_context(|| {
                format!(
                    "failed to connect to daemon socket {}; start it with `usage-daemon`",
                    self.socket.display()
                )
            })?;
        let (reader, mut writer) = stream.into_split();
        let mut payload = Vec::with_capacity(requests.len().saturating_mul(64));
        for request in &requests {
            serde_json::to_writer(&mut payload, &RequestEnvelope::new(request.clone()))?;
            payload.push(b'\n');
        }
        timeout(WRITE_TIMEOUT, writer.write_all(&payload))
            .await
            .context("timed out writing daemon request")??;

        let mut reader = BufReader::new(reader);
        let mut line = Vec::with_capacity(32 * 1024);
        let mut responses = Vec::with_capacity(requests.len());
        for response_index in 0..requests.len() {
            let has_response = timeout(READ_TIMEOUT, read_bounded_response(&mut reader, &mut line))
                .await
                .with_context(|| {
                    format!(
                        "timed out reading daemon response {} of {}",
                        response_index + 1,
                        requests.len()
                    )
                })??;
            if !has_response {
                bail!(
                    "daemon closed connection after {response_index} of {} responses",
                    requests.len()
                );
            }
            responses.push(decode_response(&line)?);
        }
        Ok(responses)
    }
}

async fn read_bounded_response(
    reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
    line: &mut Vec<u8>,
) -> anyhow::Result<bool> {
    line.clear();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(!line.is_empty());
        }
        let take = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        if line.len().saturating_add(take) > MAX_RESPONSE_BYTES {
            bail!("daemon response exceeded {MAX_RESPONSE_BYTES} bytes");
        }
        line.extend_from_slice(&available[..take]);
        reader.consume(take);
        if line.last() == Some(&b'\n') {
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            return Ok(true);
        }
    }
}

fn decode_response(frame: &[u8]) -> anyhow::Result<ApiResponse> {
    let envelope: ResponseEnvelope =
        serde_json::from_slice(frame).context("failed to decode daemon response")?;
    if envelope.api_version != API_VERSION {
        bail!(
            "daemon API version {} is incompatible with CLI version {}",
            envelope.api_version,
            API_VERSION
        );
    }
    Ok(envelope.response)
}

pub fn unexpected<T>(expected: &str, response: ApiResponse) -> anyhow::Result<T> {
    match response {
        ApiResponse::Error { error } => bail!("daemon returned {}: {}", error.code, error.message),
        other => bail!("daemon returned unexpected {expected} response: {other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use tokio::net::UnixListener;
    use usage_core::ApiErrorCode;

    use super::*;

    static NEXT_SOCKET_ID: AtomicU64 = AtomicU64::new(1);

    struct TestSocketPath(PathBuf);

    impl TestSocketPath {
        fn new() -> Self {
            let id = NEXT_SOCKET_ID.fetch_add(1, Ordering::Relaxed);
            Self(std::env::temp_dir().join(format!(
                "usage-cli-client-test-{}-{id}.sock",
                std::process::id()
            )))
        }
    }

    impl Drop for TestSocketPath {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn request_batch_pipelines_frames_and_preserves_order() {
        let socket = TestSocketPath::new();
        let listener = UnixListener::bind(&socket.0).unwrap();
        let server = async {
            let (stream, _) = listener.accept().await.unwrap();
            let (reader, mut writer) = stream.into_split();
            let mut reader = BufReader::new(reader);
            let mut line = Vec::new();
            for _ in 0..3 {
                line.clear();
                assert!(reader.read_until(b'\n', &mut line).await.unwrap() > 0);
            }
            for code in [
                ApiErrorCode::InvalidJson,
                ApiErrorCode::InvalidRequest,
                ApiErrorCode::Internal,
            ] {
                let mut frame =
                    serde_json::to_vec(&ResponseEnvelope::new(ApiResponse::error(code, "test")))
                        .unwrap();
                frame.push(b'\n');
                let split = frame.len() / 2;
                writer.write_all(&frame[..split]).await.unwrap();
                tokio::task::yield_now().await;
                writer.write_all(&frame[split..]).await.unwrap();
            }
        };
        let client = Client::new(&socket.0);
        let request = client.request_batch([
            ApiRequest::GetUsage,
            ApiRequest::GetAccounts,
            ApiRequest::GetConfig,
        ]);
        let (responses, ()) = tokio::join!(request, server);
        let codes = responses
            .unwrap()
            .into_iter()
            .map(|response| match response {
                ApiResponse::Error { error } => error.code,
                other => panic!("unexpected response: {other:?}"),
            })
            .collect::<Vec<_>>();

        assert_eq!(
            codes,
            [
                ApiErrorCode::InvalidJson,
                ApiErrorCode::InvalidRequest,
                ApiErrorCode::Internal
            ]
        );
    }

    #[test]
    fn rejects_response_from_future_protocol() {
        let mut envelope = ResponseEnvelope::new(ApiResponse::error(
            ApiErrorCode::Internal,
            "future response",
        ));
        envelope.api_version = API_VERSION + 1;

        let error = decode_response(&serde_json::to_vec(&envelope).unwrap()).unwrap_err();

        assert!(error.to_string().contains("incompatible with CLI version"));
    }
}
