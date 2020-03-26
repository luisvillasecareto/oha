use anyhow::Context;
use rand::seq::SliceRandom;
use std::str::FromStr;
use tokio::prelude::*;
use tokio::stream::StreamExt;
use url::Url;

trait AsyncRW: AsyncRead + AsyncWrite {}
impl<T: AsyncRead + AsyncWrite> AsyncRW for T {}

pub struct ClientBuilder {
    pub url: Url,
    pub method: http::Method,
    pub headers: http::header::HeaderMap,
}

impl ClientBuilder {
    pub fn build(&self) -> Client {
        Client {
            url: self.url.clone(),
            method: self.method.clone(),
            rng: rand::thread_rng(),
            resolver: None,
            send_request: None,
            headers: self.headers.clone(),
        }
    }
}

pub struct Client {
    url: Url,
    method: http::Method,
    headers: http::header::HeaderMap,
    rng: rand::rngs::ThreadRng,
    resolver: Option<
        trust_dns_resolver::AsyncResolver<
            trust_dns_resolver::name_server::GenericConnection,
            trust_dns_resolver::name_server::GenericConnectionProvider<
                trust_dns_resolver::name_server::TokioRuntime,
            >,
        >,
    >,
    send_request: Option<hyper::client::conn::SendRequest<hyper::Body>>,
}

impl Client {
    async fn lookup_ip(&mut self) -> anyhow::Result<std::net::IpAddr> {
        let resolver = if let Some(resolver) = self.resolver.take() {
            resolver
        } else {
            trust_dns_resolver::AsyncResolver::tokio(Default::default(), Default::default()).await?
        };

        let addrs = resolver
            .lookup_ip(self.url.host_str().context("get host")?)
            .await?
            .iter()
            .collect::<Vec<_>>();

        let addr = *addrs.choose(&mut self.rng).context("get addr")?;

        self.resolver = Some(resolver);

        Ok(addr)
    }

    async fn send_request(
        &mut self,
        addr: (std::net::IpAddr, u16),
    ) -> anyhow::Result<hyper::client::conn::SendRequest<hyper::Body>> {
        if self.url.scheme() == "https" {
            let stream = tokio::net::TcpStream::connect(addr).await?;
            let connector = native_tls::TlsConnector::new()?;
            let connector = tokio_tls::TlsConnector::from(connector);
            let stream = connector
                .connect(self.url.domain().context("get domain")?, stream)
                .await?;
            let (send, conn) = hyper::client::conn::handshake(stream).await?;
            tokio::spawn(conn);
            Ok(send)
        } else {
            let stream = tokio::net::TcpStream::connect(addr).await?;
            let (send, conn) = hyper::client::conn::handshake(stream).await?;
            tokio::spawn(conn);
            Ok(send)
        }
    }

    fn request(&self) -> anyhow::Result<http::Request<hyper::Body>> {
        let mut builder = http::Request::builder()
            .uri(http::uri::Uri::from_str(self.url.path())?)
            .method(self.method.clone());

        builder
            .headers_mut()
            .context("get header")?
            .extend(self.headers.iter().map(|(k, v)| (k.clone(), v.clone())));

        Ok(builder.body(hyper::Body::empty())?)
    }

    pub async fn work(&mut self) -> anyhow::Result<crate::RequestResult> {
        let start = std::time::Instant::now();
        let mut send_request = if let Some(send_request) = self.send_request.take() {
            send_request
        } else {
            let addr = (
                self.lookup_ip().await?,
                self.url.port_or_known_default().context("get port")?,
            );
            self.send_request(addr).await?
        };

        let mut num_retry = 0;
        let res = loop {
            let request = self.request()?;
            match send_request.send_request(request).await {
                Ok(res) => break res,
                Err(e) => {
                    if num_retry > 1 {
                        return Err(e.into());
                    }
                    let addr = (
                        self.lookup_ip().await?,
                        self.url.port_or_known_default().context("get port")?,
                    );
                    send_request = self.send_request(addr).await?;
                    num_retry += 1;
                }
            }
        };

        let status = res.status();
        let mut len_sum = 0;

        let mut stream = res.into_body();
        while let Some(chunk) = stream.next().await {
            len_sum += chunk?.len();
        }
        let end = std::time::Instant::now();

        let result = crate::RequestResult {
            start,
            end,
            status,
            len_bytes: len_sum,
        };

        self.send_request = Some(send_request);

        Ok(result)
    }
}

/// Run n tasks by m workers
/// Currently We use Fn() -> F as "task generator".
/// Any replacement?
pub async fn work(
    client_builder: ClientBuilder,
    report_tx: flume::Sender<anyhow::Result<crate::RequestResult>>,
    n_tasks: usize,
    n_workers: usize,
) {
    let injector = crossbeam::deque::Injector::new();

    for _ in 0..n_tasks {
        injector.push(());
    }

    futures::future::join_all((0..n_workers).map(|_| async {
        let mut w = client_builder.build();
        while let crossbeam::deque::Steal::Success(()) = injector.steal() {
            report_tx.send(w.work().await).unwrap();
        }
    }))
    .await;
}

/// n tasks by m workers limit to qps works in a second
pub async fn work_with_qps(
    client_builder: ClientBuilder,
    report_tx: flume::Sender<anyhow::Result<crate::RequestResult>>,
    qps: usize,
    n_tasks: usize,
    n_workers: usize,
) {
    let (tx, rx) = crossbeam::channel::unbounded();

    tokio::spawn(async move {
        let start = std::time::Instant::now();
        for i in 0..n_tasks {
            tx.send(()).unwrap();
            tokio::time::delay_until(
                (start + i as u32 * std::time::Duration::from_secs(1) / qps as u32).into(),
            )
            .await;
        }
        // tx gone
    });

    futures::future::join_all((0..n_workers).map(|_| async {
        let mut w = client_builder.build();
        while let Ok(()) = rx.recv() {
            report_tx.send(w.work().await).unwrap();
        }
    }))
    .await;
}

/// Run until dead_line by n workers
pub async fn work_until(
    client_builder: ClientBuilder,
    report_tx: flume::Sender<anyhow::Result<crate::RequestResult>>,
    dead_line: std::time::Instant,
    n_workers: usize,
) {
    futures::future::join_all((0..n_workers).map(|_| async {
        let mut w = client_builder.build();
        while std::time::Instant::now() < dead_line {
            report_tx.send(w.work().await).unwrap();
        }
    }))
    .await;
}

/// Run until dead_line by n workers limit to qps works in a second
pub async fn work_until_with_qps(
    client_builder: ClientBuilder,
    report_tx: flume::Sender<anyhow::Result<crate::RequestResult>>,
    qps: usize,
    start: std::time::Instant,
    dead_line: std::time::Instant,
    n_workers: usize,
) {
    let (tx, rx) = crossbeam::channel::bounded(qps);

    let gen = tokio::spawn(async move {
        for i in 0.. {
            if std::time::Instant::now() > dead_line {
                break;
            }
            if tx.send(()).is_err() {
                break;
            }
            tokio::time::delay_until(
                (start + i as u32 * std::time::Duration::from_secs(1) / qps as u32).into(),
            )
            .await;
        }
        // tx gone
    });

    let ret = futures::future::join_all((0..n_workers).map(|_| async {
        let mut w = client_builder.build();
        while let Ok(()) = rx.recv() {
            if std::time::Instant::now() > dead_line {
                break;
            }
            report_tx.send(w.work().await).unwrap();
        }
    }))
    .await;

    let _ = gen.await;
}