use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures::{Async, Future, Poll};
use hyper::client::FutureResponse;
use hyper::header::{Headers, Location, Referer, UserAgent, Accept, Encoding,
                    AcceptEncoding, Range, qitem};
use native_tls::{TlsConnector, TlsConnectorBuilder};
use tokio_core::reactor::Handle;


use super::body;
use super::request::{self, Request, RequestBuilder};
use super::response::{self, Response};
use redirect::{self, RedirectPolicy, check_redirect, remove_sensitive_headers};
use {Certificate, IntoUrl, Method, StatusCode, Url};

static DEFAULT_USER_AGENT: &'static str =
    concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));

/// A `Client` to make Requests with.
///
/// The Client has various configuration values to tweak, but the defaults
/// are set to what is usually the most commonly desired value.
///
/// The `Client` holds a connection pool internally, so it is advised that
/// you create one and reuse it.
///
/// # Examples
///
/// ```rust
/// # use reqwest::{Error, Client};
/// #
/// # fn run() -> Result<(), Error> {
/// let client = Client::new()?;
/// let resp = client.get("http://httpbin.org/")?.send()?;
/// #   drop(resp);
/// #   Ok(())
/// # }
///
/// ```
#[derive(Clone)]
pub struct Client {
    inner: Arc<ClientRef>,
}

/// A `ClientBuilder` can be used to create a `Client` with  custom configuration:
///
/// - with hostname verification disabled
/// - with one or multiple custom certificates
///
/// # Examples
///
/// ```
/// # use std::fs::File;
/// # use std::io::Read;
/// # fn build_client() -> Result<(), Box<std::error::Error>> {
/// // read a local binary DER encoded certificate
/// let mut buf = Vec::new();
/// File::open("my-cert.der")?.read_to_end(&mut buf)?;
///
/// // create a certificate
/// let cert = reqwest::Certificate::from_der(&buf)?;
///
/// // get a client builder
/// let client = reqwest::ClientBuilder::new()?
///     .add_root_certificate(cert)?
///     .build()?;
/// # drop(client);
/// # Ok(())
/// # }
/// ```
pub struct ClientBuilder {
    config: Option<Config>,
}

struct Config {
    gzip: bool,
    hostname_verification: bool,
    redirect_policy: RedirectPolicy,
    referer: bool,
    timeout: Option<Duration>,
    tls: TlsConnectorBuilder,
}

impl ClientBuilder {
    /// Constructs a new `ClientBuilder`
    ///
    /// # Errors
    ///
    /// This method fails if native TLS backend cannot be created.
    pub fn new() -> ::Result<ClientBuilder> {
        let tls_connector_builder = try_!(TlsConnector::builder());
        Ok(ClientBuilder {
            config: Some(Config {
                gzip: true,
                hostname_verification: true,
                redirect_policy: RedirectPolicy::default(),
                referer: true,
                timeout: None,
                tls: tls_connector_builder,
            })
        })
    }

    /// Returns a `Client` that uses this `ClientBuilder` configuration.
    ///
    /// # Errors
    ///
    /// This method fails if native TLS backend cannot be initialized.
    ///
    /// # Panics
    ///
    /// This method consumes the internal state of the builder.
    /// Trying to use this builder again after calling `build` will panic.
    pub fn build(&mut self, handle: &Handle) -> ::Result<Client> {
        let config = self.take_config();

        let tls = try_!(config.tls.build());

        /*
        let mut tls_client = NativeTlsClient::from(tls_connector);
        if !config.hostname_verification {
            tls_client.danger_disable_hostname_verification(true);
        }
        */

        let hyper_client = create_hyper_client(tls, handle);
        //let mut hyper_client = create_hyper_client(tls_client);

        //hyper_client.set_read_timeout(config.timeout);
        //hyper_client.set_write_timeout(config.timeout);

        Ok(Client {
            inner: Arc::new(ClientRef {
                gzip: config.gzip,
                hyper: hyper_client,
                redirect_policy: config.redirect_policy,
                referer: config.referer,
            }),
        })
    }

    /// Add a custom root certificate.
    ///
    /// This can be used to connect to a server that has a self-signed
    /// certificate for example.
    ///
    /// # Errors
    ///
    /// This method fails if adding root certificate was unsuccessful.
    pub fn add_root_certificate(&mut self, cert: Certificate) -> ::Result<&mut ClientBuilder> {
        let cert = ::tls::cert(cert);
        try_!(self.config_mut().tls.add_root_certificate(cert));
        Ok(self)
    }

    /// Disable hostname verification.
    ///
    /// # Warning
    ///
    /// You should think very carefully before you use this method. If
    /// hostname verification is not used, any valid certificate for any
    /// site will be trusted for use from any other. This introduces a
    /// significant vulnerability to man-in-the-middle attacks.
    #[inline]
    pub fn danger_disable_hostname_verification(&mut self) -> &mut ClientBuilder {
        self.config_mut().hostname_verification = false;
        self
    }

    /// Enable hostname verification.
    #[inline]
    pub fn enable_hostname_verification(&mut self) -> &mut ClientBuilder {
        self.config_mut().hostname_verification = true;
        self
    }

    /// Enable auto gzip decompression by checking the ContentEncoding response header.
    ///
    /// Default is enabled.
    #[inline]
    pub fn gzip(&mut self, enable: bool) -> &mut ClientBuilder {
        self.config_mut().gzip = enable;
        self
    }

    /// Set a `RedirectPolicy` for this client.
    ///
    /// Default will follow redirects up to a maximum of 10.
    #[inline]
    pub fn redirect(&mut self, policy: RedirectPolicy) -> &mut ClientBuilder {
        self.config_mut().redirect_policy = policy;
        self
    }

    /// Enable or disable automatic setting of the `Referer` header.
    ///
    /// Default is `true`.
    #[inline]
    pub fn referer(&mut self, enable: bool) -> &mut ClientBuilder {
        self.config_mut().referer = enable;
        self
    }

    /// Set a timeout for both the read and write operations of a client.
    #[inline]
    pub fn timeout(&mut self, timeout: Duration) -> &mut ClientBuilder {
        self.config_mut().timeout = Some(timeout);
        self
    }

    // private
    fn config_mut(&mut self) -> &mut Config {
        self.config
            .as_mut()
            .expect("ClientBuilder cannot be reused after building a Client")
    }

    fn take_config(&mut self) -> Config {
        self.config
            .take()
            .expect("ClientBuilder cannot be reused after building a Client")
    }
}

type HyperClient = ::hyper::Client<::hyper_tls::HttpsConnector<::hyper::client::HttpConnector>>;

fn create_hyper_client(tls: TlsConnector, handle: &Handle) -> HyperClient {
    let mut http = ::hyper::client::HttpConnector::new(4, handle);
    http.enforce_http(false);
    let https = ::hyper_tls::HttpsConnector::from((http, tls));
    ::hyper::Client::configure()
        .connector(https)
        .build(handle)
}

impl Client {
    /// Constructs a new `Client`.
    ///
    /// # Errors
    ///
    /// This method fails if native TLS backend cannot be created or initialized.
    #[inline]
    pub fn new(handle: &Handle) -> ::Result<Client> {
        ClientBuilder::new()?.build(handle)
    }

    /// Creates a `ClientBuilder` to configure a `Client`.
    ///
    /// # Errors
    ///
    /// This method fails if native TLS backend cannot be created.
    #[inline]
    pub fn builder() -> ::Result<ClientBuilder> {
        ClientBuilder::new()
    }

    /// Convenience method to make a `GET` request to a URL.
    ///
    /// # Errors
    ///
    /// This method fails whenever supplied `Url` cannot be parsed.
    pub fn get<U: IntoUrl>(&self, url: U) -> ::Result<RequestBuilder> {
        self.request(Method::Get, url)
    }

    /// Convenience method to make a `POST` request to a URL.
    ///
    /// # Errors
    ///
    /// This method fails whenever supplied `Url` cannot be parsed.
    pub fn post<U: IntoUrl>(&self, url: U) -> ::Result<RequestBuilder> {
        self.request(Method::Post, url)
    }

    /// Convenience method to make a `PUT` request to a URL.
    ///
    /// # Errors
    ///
    /// This method fails whenever supplied `Url` cannot be parsed.
    pub fn put<U: IntoUrl>(&self, url: U) -> ::Result<RequestBuilder> {
        self.request(Method::Put, url)
    }

    /// Convenience method to make a `PATCH` request to a URL.
    ///
    /// # Errors
    ///
    /// This method fails whenever supplied `Url` cannot be parsed.
    pub fn patch<U: IntoUrl>(&self, url: U) -> ::Result<RequestBuilder> {
        self.request(Method::Patch, url)
    }

    /// Convenience method to make a `DELETE` request to a URL.
    ///
    /// # Errors
    ///
    /// This method fails whenever supplied `Url` cannot be parsed.
    pub fn delete<U: IntoUrl>(&self, url: U) -> ::Result<RequestBuilder> {
        self.request(Method::Delete, url)
    }

    /// Convenience method to make a `HEAD` request to a URL.
    ///
    /// # Errors
    ///
    /// This method fails whenever supplied `Url` cannot be parsed.
    pub fn head<U: IntoUrl>(&self, url: U) -> ::Result<RequestBuilder> {
        self.request(Method::Head, url)
    }

    /// Start building a `Request` with the `Method` and `Url`.
    ///
    /// Returns a `RequestBuilder`, which will allow setting headers and
    /// request body before sending.
    ///
    /// # Errors
    ///
    /// This method fails whenever supplied `Url` cannot be parsed.
    pub fn request<U: IntoUrl>(&self, method: Method, url: U) -> ::Result<RequestBuilder> {
        let url = try_!(url.into_url());
        Ok(request::builder(self.clone(), Request::new(method, url)))
    }

    /// Executes a `Request`.
    ///
    /// A `Request` can be built manually with `Request::new()` or obtained
    /// from a RequestBuilder with `RequestBuilder::build()`.
    ///
    /// You should prefer to use the `RequestBuilder` and
    /// `RequestBuilder::send()`.
    ///
    /// # Errors
    ///
    /// This method fails if there was an error while sending request,
    /// redirect loop was detected or redirect limit was exhausted.
    pub fn execute(&self, request: Request) -> Pending {
        self.execute_request(request)
    }


    fn execute_request(&self, req: Request) -> Pending {
        let (
            method,
            url,
            mut headers,
            body
        ) = request::pieces(req);

        if !headers.has::<UserAgent>() {
            headers.set(UserAgent::new(DEFAULT_USER_AGENT));
        }

        if !headers.has::<Accept>() {
            headers.set(Accept::star());
        }
        if self.inner.gzip &&
            !headers.has::<AcceptEncoding>() &&
            !headers.has::<Range>() {
            headers.set(AcceptEncoding(vec![qitem(Encoding::Gzip)]));
        }

        let mut req = ::hyper::Request::new(method.clone(), url_to_uri(&url));
        *req.headers_mut() = headers.clone();
        let body = body.and_then(|body| {
            let (resuable, body) = body::into_hyper(body);
            req.set_body(body);
            resuable
        });

        let in_flight = self.inner.hyper.request(req);

        Pending {
            method: method,
            url: url,
            headers: headers,
            body: body,

            urls: Vec::new(),

            client: self.inner.clone(),

            in_flight: in_flight,
        }
    }
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Client")
            .field("gzip", &self.inner.gzip)
            .field("redirect_policy", &self.inner.redirect_policy)
            .field("referer", &self.inner.referer)
            .finish()
    }
}

impl fmt::Debug for ClientBuilder {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("ClientBuilder")
            .finish()
    }
}

struct ClientRef {
    gzip: bool,
    hyper: HyperClient,
    redirect_policy: RedirectPolicy,
    referer: bool,
}

pub struct Pending {
    method: Method,
    url: Url,
    headers: Headers,
    body: Option<Bytes>,

    urls: Vec<Url>,

    client: Arc<ClientRef>,

    in_flight: FutureResponse,
}

impl Future for Pending {
    type Item = Response;
    type Error = ::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            let res = match try_!(self.in_flight.poll(), &self.url) {
                Async::Ready(res) => res,
                Async::NotReady => return Ok(Async::NotReady),
            };
            let should_redirect = match res.status() {
                StatusCode::MovedPermanently |
                StatusCode::Found |
                StatusCode::SeeOther => {
                    self.body = None;
                    match self.method {
                        Method::Get | Method::Head => {},
                        _ => {
                            self.method = Method::Get;
                        }
                    }
                    true
                },
                StatusCode::TemporaryRedirect |
                StatusCode::PermanentRedirect => {
                    self.body.is_some()
                },
                _ => false,
            };
            if should_redirect {
                let loc = res.headers()
                    .get::<Location>()
                    .map(|loc| self.url.join(loc));
                if let Some(Ok(loc)) = loc {
                    if self.client.referer {
                        if let Some(referer) = make_referer(&loc, &self.url) {
                            self.headers.set(referer);
                        }
                    }
                    self.urls.push(self.url.clone());
                    let action = check_redirect(&self.client.redirect_policy, &loc, &self.urls);

                    match action {
                        redirect::Action::Follow => {
                            self.url = loc;

                            remove_sensitive_headers(&mut self.headers, &self.url, &self.urls);
                            debug!("redirecting to {:?} '{}'", self.method, self.url);
                            let mut req = ::hyper::Request::new(
                                self.method.clone(),
                                url_to_uri(&self.url)
                            );
                            *req.headers_mut() = self.headers.clone();
                            if let Some(ref body) = self.body {
                                req.set_body(body.clone());
                            }
                            self.in_flight = self.client.hyper.request(req);
                            continue;
                        },
                        redirect::Action::Stop => {
                            debug!("redirect_policy disallowed redirection to '{}'", loc);
                        },
                        redirect::Action::LoopDetected => {
                            return Err(::error::loop_detected(self.url.clone()));
                        },
                        redirect::Action::TooManyRedirects => {
                            return Err(::error::too_many_redirects(self.url.clone()));
                        }
                    }
                } else if let Some(Err(e)) = loc {
                    debug!("Location header had invalid URI: {:?}", e);
                }
            }
            let res = response::new(res, self.url.clone(), self.client.gzip);
            return Ok(Async::Ready(res));
        }
    }
}

impl fmt::Debug for Pending {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Pending")
            .field("method", &self.method)
            .field("url", &self.url)
            .finish()
    }
}

fn make_referer(next: &Url, previous: &Url) -> Option<Referer> {
    if next.scheme() == "http" && previous.scheme() == "https" {
        return None;
    }

    let mut referer = previous.clone();
    let _ = referer.set_username("");
    let _ = referer.set_password(None);
    referer.set_fragment(None);
    Some(Referer::new(referer.into_string()))
}

fn url_to_uri(url: &Url) -> ::hyper::Uri {
    url.as_str().parse().expect("a parsed Url should always be a valid Uri")
}

// pub(crate)

pub fn take_builder(builder: &mut ClientBuilder) -> ClientBuilder {
    use std::mem;
    mem::replace(builder, ClientBuilder { config: None })
}