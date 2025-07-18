// Copyright 2018 Parity Technologies (UK) Ltd.
//
// Permission is hereby granted, free of charge, to any person obtaining a
// copy of this software and associated documentation files (the "Software"),
// to deal in the Software without restriction, including without limitation
// the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the
// Software is furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS
// OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
// FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
// DEALINGS IN THE SOFTWARE.

//! # [DNS name resolution](https://github.com/libp2p/specs/blob/master/addressing/README.md#ip-and-name-resolution)
//! [`Transport`] for libp2p.
//!
//! This crate provides the type [`tokio::Transport`] based on [`hickory_resolver::TokioResolver`].
//!
//! A [`Transport`] is an address-rewriting [`libp2p_core::Transport`] wrapper around
//! an inner `Transport`. The composed transport behaves like the inner
//! transport, except that [`libp2p_core::Transport::dial`] resolves `/dns/...`, `/dns4/...`,
//! `/dns6/...` and `/dnsaddr/...` components of the given `Multiaddr` through
//! a DNS, replacing them with the resolved protocols (typically TCP/IP).
//!
//! The [`tokio::Transport`] is enabled by default under the `tokio` feature.
//! Tokio users can furthermore opt-in to the `tokio-dns-over-rustls` and
//! `tokio-dns-over-https-rustls` features.
//! For more information about these features, please refer to the documentation
//! of [trust-dns-resolver].
//! Alternative runtimes or resolvers can be used though a manual implementation of [`Resolver`].
//!
//! On Unix systems, if no custom configuration is given, [trust-dns-resolver]
//! will try to parse the `/etc/resolv.conf` file. This approach comes with a
//! few caveats to be aware of:
//!   1) This fails (panics even!) if `/etc/resolv.conf` does not exist. This is the case on all
//!      versions of Android.
//!   2) DNS configuration is only evaluated during startup. Runtime changes are thus ignored.
//!   3) DNS resolution is obviously done in process and consequently not using any system APIs
//!      (like libc's `gethostbyname`). Again this is problematic on platforms like Android, where
//!      there's a lot of complexity hidden behind the system APIs.
//!
//! If the implementation requires different characteristics, one should
//! consider providing their own implementation of [`Transport`] or use
//! platform specific APIs to extract the host's DNS configuration (if possible)
//! and provide a custom [`ResolverConfig`].
//!
//! [trust-dns-resolver]: https://docs.rs/trust-dns-resolver/latest/trust_dns_resolver/#dns-over-tls-and-dns-over-https

#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

#[cfg(feature = "tokio")]
pub mod tokio {
    use std::sync::Arc;

    use hickory_resolver::{name_server::TokioConnectionProvider, system_conf, TokioResolver};
    use parking_lot::Mutex;

    /// A `Transport` wrapper for performing DNS lookups when dialing `Multiaddr`esses
    /// using `tokio` for all async I/O.
    pub type Transport<T> = crate::Transport<T, TokioResolver>;

    impl<T> Transport<T> {
        /// Creates a new [`Transport`] from the OS's DNS configuration and defaults.
        pub fn system(inner: T) -> Result<Transport<T>, std::io::Error> {
            let (cfg, opts) = system_conf::read_system_conf()?;
            Ok(Self::custom(inner, cfg, opts))
        }

        /// Creates a [`Transport`] with a custom resolver configuration
        /// and options.
        pub fn custom(
            inner: T,
            cfg: hickory_resolver::config::ResolverConfig,
            opts: hickory_resolver::config::ResolverOpts,
        ) -> Transport<T> {
            Transport {
                inner: Arc::new(Mutex::new(inner)),
                resolver: TokioResolver::builder_with_config(
                    cfg,
                    TokioConnectionProvider::default(),
                )
                .with_options(opts)
                .build(),
            }
        }
    }
}

use std::{
    error, fmt, io, iter,
    net::{Ipv4Addr, Ipv6Addr},
    ops::DerefMut,
    pin::Pin,
    str,
    sync::Arc,
    task::{Context, Poll},
};

use async_trait::async_trait;
use futures::{future::BoxFuture, prelude::*};
pub use hickory_resolver::{
    config::{ResolverConfig, ResolverOpts},
    ResolveError, ResolveErrorKind,
};
use hickory_resolver::{
    lookup::{Ipv4Lookup, Ipv6Lookup, TxtLookup},
    lookup_ip::LookupIp,
    name_server::ConnectionProvider,
};
use libp2p_core::{
    multiaddr::{Multiaddr, Protocol},
    transport::{DialOpts, ListenerId, TransportError, TransportEvent},
};
use parking_lot::Mutex;
use smallvec::SmallVec;

/// The prefix for `dnsaddr` protocol TXT record lookups.
const DNSADDR_PREFIX: &str = "_dnsaddr.";

/// The maximum number of dialing attempts to resolved addresses.
const MAX_DIAL_ATTEMPTS: usize = 16;

/// The maximum number of DNS lookups when dialing.
///
/// This limit is primarily a safeguard against too many, possibly
/// even cyclic, indirections in the addresses obtained from the
/// TXT records of a `/dnsaddr`.
const MAX_DNS_LOOKUPS: usize = 32;

/// The maximum number of TXT records applicable for the address
/// being dialed that are considered for further lookups as a
/// result of a single `/dnsaddr` lookup.
const MAX_TXT_RECORDS: usize = 16;

/// A [`Transport`] for performing DNS lookups when dialing `Multiaddr`esses.
/// You shouldn't need to use this type directly. Use [`tokio::Transport`] instead.
#[derive(Debug)]
pub struct Transport<T, R> {
    /// The underlying transport.
    inner: Arc<Mutex<T>>,
    /// The DNS resolver used when dialing addresses with DNS components.
    resolver: R,
}

impl<T, R> libp2p_core::Transport for Transport<T, R>
where
    T: libp2p_core::Transport + Send + Unpin + 'static,
    T::Error: Send,
    T::Dial: Send,
    R: Clone + Send + Sync + Resolver + 'static,
{
    type Output = T::Output;
    type Error = Error<T::Error>;
    type ListenerUpgrade = future::MapErr<T::ListenerUpgrade, fn(T::Error) -> Self::Error>;
    type Dial = future::Either<
        future::MapErr<T::Dial, fn(T::Error) -> Self::Error>,
        BoxFuture<'static, Result<Self::Output, Self::Error>>,
    >;

    fn listen_on(
        &mut self,
        id: ListenerId,
        addr: Multiaddr,
    ) -> Result<(), TransportError<Self::Error>> {
        self.inner
            .lock()
            .listen_on(id, addr)
            .map_err(|e| e.map(Error::Transport))
    }

    fn remove_listener(&mut self, id: ListenerId) -> bool {
        self.inner.lock().remove_listener(id)
    }

    fn dial(
        &mut self,
        addr: Multiaddr,
        dial_opts: DialOpts,
    ) -> Result<Self::Dial, TransportError<Self::Error>> {
        Ok(self.do_dial(addr, dial_opts))
    }

    fn poll(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<TransportEvent<Self::ListenerUpgrade, Self::Error>> {
        let mut inner = self.inner.lock();
        libp2p_core::Transport::poll(Pin::new(inner.deref_mut()), cx).map(|event| {
            event
                .map_upgrade(|upgr| upgr.map_err::<_, fn(_) -> _>(Error::Transport))
                .map_err(Error::Transport)
        })
    }
}

impl<T, R> Transport<T, R>
where
    T: libp2p_core::Transport + Send + Unpin + 'static,
    T::Error: Send,
    T::Dial: Send,
    R: Clone + Send + Sync + Resolver + 'static,
{
    fn do_dial(
        &mut self,
        addr: Multiaddr,
        dial_opts: DialOpts,
    ) -> <Self as libp2p_core::Transport>::Dial {
        let resolver = self.resolver.clone();
        let inner = self.inner.clone();

        // Asynchronously resolve all DNS names in the address before proceeding
        // with dialing on the underlying transport.
        async move {
            let mut dial_errors: Vec<Error<T::Error>> = Vec::new();
            let mut dns_lookups = 0;
            let mut dial_attempts = 0;
            // We optimise for the common case of a single DNS component
            // in the address that is resolved with a single lookup.
            let mut unresolved = SmallVec::<[Multiaddr; 1]>::new();
            unresolved.push(addr.clone());

            // Resolve (i.e. replace) all DNS protocol components, initiating
            // dialing attempts as soon as there is another fully resolved
            // address.
            while let Some(addr) = unresolved.pop() {
                if let Some((i, name)) = addr.iter().enumerate().find(|(_, p)| {
                    matches!(
                        p,
                        Protocol::Dns(_)
                            | Protocol::Dns4(_)
                            | Protocol::Dns6(_)
                            | Protocol::Dnsaddr(_)
                    )
                }) {
                    if dns_lookups == MAX_DNS_LOOKUPS {
                        tracing::debug!(address=%addr, "Too many DNS lookups, dropping unresolved address");
                        dial_errors.push(Error::TooManyLookups);
                        // There may still be fully resolved addresses in `unresolved`,
                        // so keep going until `unresolved` is empty.
                        continue;
                    }
                    dns_lookups += 1;
                    match resolve(&name, &resolver).await {
                        Err(e) => {
                            // Record the resolution error.
                            dial_errors.push(e);
                        }
                        Ok(Resolved::One(ip)) => {
                            tracing::trace!(protocol=%name, resolved=%ip);
                            let addr = addr.replace(i, |_| Some(ip)).expect("`i` is a valid index");
                            unresolved.push(addr);
                        }
                        Ok(Resolved::Many(ips)) => {
                            for ip in ips {
                                tracing::trace!(protocol=%name, resolved=%ip);
                                let addr =
                                    addr.replace(i, |_| Some(ip)).expect("`i` is a valid index");
                                unresolved.push(addr);
                            }
                        }
                        Ok(Resolved::Addrs(addrs)) => {
                            let suffix = addr.iter().skip(i + 1).collect::<Multiaddr>();
                            let prefix = addr.iter().take(i).collect::<Multiaddr>();
                            let mut n = 0;
                            for a in addrs {
                                if a.ends_with(&suffix) {
                                    if n < MAX_TXT_RECORDS {
                                        n += 1;
                                        tracing::trace!(protocol=%name, resolved=%a);
                                        let addr =
                                            prefix.iter().chain(a.iter()).collect::<Multiaddr>();
                                        unresolved.push(addr);
                                    } else {
                                        tracing::debug!(
                                            resolved=%a,
                                            "Too many TXT records, dropping resolved"
                                        );
                                    }
                                }
                            }
                        }
                    }
                } else {
                    // We have a fully resolved address, so try to dial it.
                    tracing::debug!(address=%addr, "Dialing address");

                    let transport = inner.clone();
                    let dial = transport.lock().dial(addr, dial_opts);
                    let result = match dial {
                        Ok(out) => {
                            // We only count attempts that the inner transport
                            // actually accepted, i.e. for which it produced
                            // a dialing future.
                            dial_attempts += 1;
                            out.await.map_err(Error::Transport)
                        }
                        Err(TransportError::MultiaddrNotSupported(a)) => {
                            Err(Error::MultiaddrNotSupported(a))
                        }
                        Err(TransportError::Other(err)) => Err(Error::Transport(err)),
                    };

                    match result {
                        Ok(out) => return Ok(out),
                        Err(err) => {
                            tracing::debug!("Dial error: {:?}.", err);
                            dial_errors.push(err);

                            if unresolved.is_empty() {
                                break;
                            }

                            if dial_attempts == MAX_DIAL_ATTEMPTS {
                                tracing::debug!(
                                    "Aborting dialing after {} attempts.",
                                    MAX_DIAL_ATTEMPTS
                                );
                                break;
                            }
                        }
                    }
                }
            }

            // If we have any dial errors, aggregate them.
            // Otherwise there were no valid DNS records for the given address to begin with
            // (i.e. DNS lookups succeeded but produced no records relevant for the given `addr`).
            if !dial_errors.is_empty() {
                Err(Error::Dial(dial_errors))
            } else {
                Err(Error::ResolveError(
                    ResolveErrorKind::Message("No Matching Records Found").into(),
                ))
            }
        }
        .boxed()
        .right_future()
    }
}

/// The possible errors of a [`Transport`] wrapped transport.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum Error<TErr> {
    /// The underlying transport encountered an error.
    Transport(TErr),
    /// DNS resolution failed.
    #[allow(clippy::enum_variant_names)]
    ResolveError(ResolveError),
    /// DNS resolution was successful, but the underlying transport refused the resolved address.
    MultiaddrNotSupported(Multiaddr),
    /// DNS resolution involved too many lookups.
    ///
    /// DNS resolution on dialing performs up to 32 DNS lookups. If these
    /// are not sufficient to obtain a fully-resolved address, this error
    /// is returned and the DNS records for the domain(s) being dialed
    /// should be investigated.
    TooManyLookups,
    /// Multiple dial errors were encountered.
    Dial(Vec<Error<TErr>>),
}

impl<TErr> fmt::Display for Error<TErr>
where
    TErr: fmt::Display,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Transport(err) => write!(f, "{err}"),
            Error::ResolveError(err) => write!(f, "{err}"),
            Error::MultiaddrNotSupported(a) => write!(f, "Unsupported resolved address: {a}"),
            Error::TooManyLookups => write!(f, "Too many DNS lookups"),
            Error::Dial(errs) => {
                write!(f, "Multiple dial errors occurred:")?;
                for err in errs {
                    write!(f, "\n - {err}")?;
                }
                Ok(())
            }
        }
    }
}

impl<TErr> error::Error for Error<TErr>
where
    TErr: error::Error + 'static,
{
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match self {
            Error::Transport(err) => Some(err),
            Error::ResolveError(err) => Some(err),
            Error::MultiaddrNotSupported(_) => None,
            Error::TooManyLookups => None,
            Error::Dial(errs) => errs.last().and_then(|e| e.source()),
        }
    }
}

/// The successful outcome of [`resolve`] for a given [`Protocol`].
enum Resolved<'a> {
    /// The given `Protocol` has been resolved to a single `Protocol`,
    /// which may be identical to the one given, in case it is not
    /// a DNS protocol component.
    One(Protocol<'a>),
    /// The given `Protocol` has been resolved to multiple alternative
    /// `Protocol`s as a result of a DNS lookup.
    Many(Vec<Protocol<'a>>),
    /// The given `Protocol` has been resolved to a new list of `Multiaddr`s
    /// obtained from DNS TXT records representing possible alternatives.
    /// These addresses may contain further DNS names that need resolving.
    Addrs(Vec<Multiaddr>),
}

/// Asynchronously resolves the domain name of a `Dns`, `Dns4`, `Dns6` or `Dnsaddr` protocol
/// component. If the given protocol is of a different type, it is returned unchanged as a
/// [`Resolved::One`].
fn resolve<'a, E: 'a + Send, R: Resolver>(
    proto: &Protocol<'a>,
    resolver: &'a R,
) -> BoxFuture<'a, Result<Resolved<'a>, Error<E>>> {
    match proto {
        Protocol::Dns(ref name) => resolver
            .lookup_ip(name.clone().into_owned())
            .map(move |res| match res {
                Ok(ips) => {
                    let mut ips = ips.into_iter();
                    let one = ips
                        .next()
                        .expect("If there are no results, `Err(NoRecordsFound)` is expected.");
                    if let Some(two) = ips.next() {
                        Ok(Resolved::Many(
                            iter::once(one)
                                .chain(iter::once(two))
                                .chain(ips)
                                .map(Protocol::from)
                                .collect(),
                        ))
                    } else {
                        Ok(Resolved::One(Protocol::from(one)))
                    }
                }
                Err(e) => Err(Error::ResolveError(e)),
            })
            .boxed(),
        Protocol::Dns4(ref name) => resolver
            .ipv4_lookup(name.clone().into_owned())
            .map(move |res| match res {
                Ok(ips) => {
                    let mut ips = ips.into_iter();
                    let one = ips
                        .next()
                        .expect("If there are no results, `Err(NoRecordsFound)` is expected.");
                    if let Some(two) = ips.next() {
                        Ok(Resolved::Many(
                            iter::once(one)
                                .chain(iter::once(two))
                                .chain(ips)
                                .map(Ipv4Addr::from)
                                .map(Protocol::from)
                                .collect(),
                        ))
                    } else {
                        Ok(Resolved::One(Protocol::from(Ipv4Addr::from(one))))
                    }
                }
                Err(e) => Err(Error::ResolveError(e)),
            })
            .boxed(),
        Protocol::Dns6(ref name) => resolver
            .ipv6_lookup(name.clone().into_owned())
            .map(move |res| match res {
                Ok(ips) => {
                    let mut ips = ips.into_iter();
                    let one = ips
                        .next()
                        .expect("If there are no results, `Err(NoRecordsFound)` is expected.");
                    if let Some(two) = ips.next() {
                        Ok(Resolved::Many(
                            iter::once(one)
                                .chain(iter::once(two))
                                .chain(ips)
                                .map(Ipv6Addr::from)
                                .map(Protocol::from)
                                .collect(),
                        ))
                    } else {
                        Ok(Resolved::One(Protocol::from(Ipv6Addr::from(one))))
                    }
                }
                Err(e) => Err(Error::ResolveError(e)),
            })
            .boxed(),
        Protocol::Dnsaddr(ref name) => {
            let name = [DNSADDR_PREFIX, name].concat();
            resolver
                .txt_lookup(name)
                .map(move |res| match res {
                    Ok(txts) => {
                        let mut addrs = Vec::new();
                        for txt in txts {
                            if let Some(chars) = txt.txt_data().first() {
                                match parse_dnsaddr_txt(chars) {
                                    Err(e) => {
                                        // Skip over seemingly invalid entries.
                                        tracing::debug!("Invalid TXT record: {:?}", e);
                                    }
                                    Ok(a) => {
                                        addrs.push(a);
                                    }
                                }
                            }
                        }
                        Ok(Resolved::Addrs(addrs))
                    }
                    Err(e) => Err(Error::ResolveError(e)),
                })
                .boxed()
        }
        proto => future::ready(Ok(Resolved::One(proto.clone()))).boxed(),
    }
}

/// Parses a `<character-string>` of a `dnsaddr` TXT record.
fn parse_dnsaddr_txt(txt: &[u8]) -> io::Result<Multiaddr> {
    let s = str::from_utf8(txt).map_err(invalid_data)?;
    match s.strip_prefix("dnsaddr=") {
        None => Err(invalid_data("Missing `dnsaddr=` prefix.")),
        Some(a) => Ok(Multiaddr::try_from(a).map_err(invalid_data)?),
    }
}

fn invalid_data(e: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e)
}

#[async_trait::async_trait]
#[doc(hidden)]
pub trait Resolver {
    async fn lookup_ip(&self, name: String) -> Result<LookupIp, ResolveError>;
    async fn ipv4_lookup(&self, name: String) -> Result<Ipv4Lookup, ResolveError>;
    async fn ipv6_lookup(&self, name: String) -> Result<Ipv6Lookup, ResolveError>;
    async fn txt_lookup(&self, name: String) -> Result<TxtLookup, ResolveError>;
}

#[async_trait]
impl<C> Resolver for hickory_resolver::Resolver<C>
where
    C: ConnectionProvider,
{
    async fn lookup_ip(&self, name: String) -> Result<LookupIp, ResolveError> {
        self.lookup_ip(name).await
    }

    async fn ipv4_lookup(&self, name: String) -> Result<Ipv4Lookup, ResolveError> {
        self.ipv4_lookup(name).await
    }

    async fn ipv6_lookup(&self, name: String) -> Result<Ipv6Lookup, ResolveError> {
        self.ipv6_lookup(name).await
    }

    async fn txt_lookup(&self, name: String) -> Result<TxtLookup, ResolveError> {
        self.txt_lookup(name).await
    }
}

#[cfg(all(test, feature = "tokio"))]
mod tests {
    use futures::future::BoxFuture;
    use hickory_resolver::proto::{ProtoError, ProtoErrorKind};
    use libp2p_core::{
        multiaddr::{Multiaddr, Protocol},
        transport::{PortUse, TransportError, TransportEvent},
        Endpoint, Transport,
    };
    use libp2p_identity::PeerId;

    use super::*;

    fn test_tokio<T, F: Future<Output = ()>>(
        transport: T,
        test_fn: impl FnOnce(tokio::Transport<T>) -> F,
    ) {
        let config = ResolverConfig::quad9();
        let opts = ResolverOpts::default();
        let transport = tokio::Transport::custom(transport, config, opts);
        let rt = ::tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
            .unwrap();
        rt.block_on(test_fn(transport));
    }

    #[test]
    fn basic_resolve() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        #[derive(Clone)]
        struct CustomTransport;

        impl Transport for CustomTransport {
            type Output = ();
            type Error = std::io::Error;
            type ListenerUpgrade = BoxFuture<'static, Result<Self::Output, Self::Error>>;
            type Dial = BoxFuture<'static, Result<Self::Output, Self::Error>>;

            fn listen_on(
                &mut self,
                _: ListenerId,
                _: Multiaddr,
            ) -> Result<(), TransportError<Self::Error>> {
                unreachable!()
            }

            fn remove_listener(&mut self, _: ListenerId) -> bool {
                false
            }

            fn dial(
                &mut self,
                addr: Multiaddr,
                _: DialOpts,
            ) -> Result<Self::Dial, TransportError<Self::Error>> {
                // Check that all DNS components have been resolved, i.e. replaced.
                assert!(!addr.iter().any(|p| matches!(
                    p,
                    Protocol::Dns(_) | Protocol::Dns4(_) | Protocol::Dns6(_) | Protocol::Dnsaddr(_)
                )));
                Ok(Box::pin(future::ready(Ok(()))))
            }

            fn poll(
                self: Pin<&mut Self>,
                _: &mut Context<'_>,
            ) -> Poll<TransportEvent<Self::ListenerUpgrade, Self::Error>> {
                unreachable!()
            }
        }

        async fn run<T, R>(mut transport: super::Transport<T, R>)
        where
            T: Transport + Clone + Send + Unpin + 'static,
            T::Error: Send,
            T::Dial: Send,
            R: Clone + Send + Sync + Resolver + 'static,
        {
            let dial_opts = DialOpts {
                role: Endpoint::Dialer,
                port_use: PortUse::Reuse,
            };

            // Success due to existing A record for example.com.
            let _ = transport
                .dial("/dns4/example.com/tcp/20000".parse().unwrap(), dial_opts)
                .unwrap()
                .await
                .unwrap();

            // Success due to existing AAAA record for example.com.
            let _ = transport
                .dial("/dns6/example.com/tcp/20000".parse().unwrap(), dial_opts)
                .unwrap()
                .await
                .unwrap();

            // Success due to pass-through, i.e. nothing to resolve.
            let _ = transport
                .dial("/ip4/1.2.3.4/tcp/20000".parse().unwrap(), dial_opts)
                .unwrap()
                .await
                .unwrap();

            // Success due to the DNS TXT records at _dnsaddr.bootstrap.libp2p.io.
            let _ = transport
                .dial("/dnsaddr/bootstrap.libp2p.io".parse().unwrap(), dial_opts)
                .unwrap()
                .await
                .unwrap();

            // Success due to the DNS TXT records at _dnsaddr.bootstrap.libp2p.io having
            // an entry with suffix `/p2p/QmNnooDu7bfjPFoTZYxMNLWUQJyrVwtbZg5gBMjTezGAJN`,
            // i.e. a bootnode with such a peer ID.
            let _ = transport
                .dial("/dnsaddr/bootstrap.libp2p.io/p2p/QmNnooDu7bfjPFoTZYxMNLWUQJyrVwtbZg5gBMjTezGAJN".parse().unwrap(), dial_opts)
                .unwrap()
                .await
                .unwrap();

            // Failure due to the DNS TXT records at _dnsaddr.libp2p.io not having
            // an entry with a random `p2p` suffix.
            match transport
                .dial(
                    format!("/dnsaddr/bootstrap.libp2p.io/p2p/{}", PeerId::random())
                        .parse()
                        .unwrap(),
                    dial_opts,
                )
                .unwrap()
                .await
            {
                Err(Error::ResolveError(_)) => {}
                Err(e) => panic!("Unexpected error: {e:?}"),
                Ok(_) => panic!("Unexpected success."),
            }

            // Failure due to no records.
            match transport
                .dial(
                    "/dns4/example.invalid/tcp/20000".parse().unwrap(),
                    dial_opts,
                )
                .unwrap()
                .await
            {
                Err(Error::Dial(dial_errs)) => {
                    assert_eq!(
                        dial_errs.len(),
                        1,
                        "Expected exactly 1 error for 'no records' scenario, got {dial_errs:?}"
                    );

                    match &dial_errs[0] {
                        Error::ResolveError(e) => match e.kind() {
                            ResolveErrorKind::Proto(ProtoError { kind, .. })
                                if matches!(
                                    kind.as_ref(),
                                    ProtoErrorKind::NoRecordsFound { .. }
                                ) => {}
                            _ => panic!("Unexpected DNS error: {e:?}"),
                        },
                        other => {
                            panic!("Expected a single ResolveError(...) sub-error, got {other:?}")
                        }
                    }
                }

                Err(e) => panic!("Unexpected error: {e:?}"),
                Ok(_) => panic!("Unexpected success."),
            }
        }

        test_tokio(CustomTransport, run);
    }

    #[test]
    fn aggregated_dial_errors() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        #[derive(Clone)]
        struct AlwaysFailTransport;

        impl libp2p_core::Transport for AlwaysFailTransport {
            type Output = ();
            type Error = std::io::Error;
            type ListenerUpgrade = BoxFuture<'static, Result<Self::Output, Self::Error>>;
            type Dial = BoxFuture<'static, Result<Self::Output, Self::Error>>;

            fn listen_on(
                &mut self,
                _id: ListenerId,
                _addr: Multiaddr,
            ) -> Result<(), TransportError<Self::Error>> {
                unimplemented!()
            }

            fn remove_listener(&mut self, _id: ListenerId) -> bool {
                false
            }

            fn dial(
                &mut self,
                addr: Multiaddr,
                _: DialOpts,
            ) -> Result<Self::Dial, TransportError<Self::Error>> {
                // Every dial attempt fails with an error that includes the address.
                Ok(Box::pin(future::ready(Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!("No support for dialing {addr}"),
                )))))
            }

            fn poll(
                self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
            ) -> Poll<TransportEvent<Self::ListenerUpgrade, Self::Error>> {
                unimplemented!()
            }
        }

        async fn run_test<T, R>(mut transport: super::Transport<T, R>)
        where
            T: Transport<Error = std::io::Error> + Clone + Send + Unpin + 'static,
            T::Error: Send,
            T::Dial: Send,
            R: Clone + Send + Sync + Resolver + 'static,
        {
            let dial_opts = DialOpts {
                role: Endpoint::Dialer,
                port_use: PortUse::Reuse,
            };

            // This address requires DNS resolution, yielding two IP addresses,
            // forcing two dial attempts. Both fail.
            let addr: Multiaddr = "/dnsaddr/bootstrap.libp2p.io".parse().unwrap();
            let dial_future = transport.dial(addr, dial_opts).unwrap();
            let result = dial_future.await;

            match result {
                Err(Error::Dial(errs)) => {
                    // We expect at least 2 errors, one per resolved IP.
                    assert!(
                        errs.len() >= 2,
                        "Expected multiple dial errors, but got {}",
                        errs.len()
                    );
                    for e in errs {
                        match e {
                            Error::Transport(io_err) => {
                                assert_eq!(
                                    io_err.kind(),
                                    io::ErrorKind::Unsupported,
                                    "Expected Unsupported dial error, got: {io_err:?}"
                                );
                            }
                            _ => panic!("Expected Error::Transport(Unsupported), got: {e:?}"),
                        }
                    }
                }
                Err(e) => panic!("Expected aggregated dial errors, got {e:?}"),
                Ok(_) => panic!("Dial unexpectedly succeeded"),
            }
        }

        test_tokio(AlwaysFailTransport, run_test);
    }
}
