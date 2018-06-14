use std::{
    fs::File,
    io::{self, Cursor, Read},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use super::{
    cert_resolver::CertResolver,

    rustls,
    untrusted,
    webpki,
};

use futures::{future, Future, Stream};
use futures_watch::Watch;

/// Not-yet-validated settings that are used for both TLS clients and TLS
/// servers.
///
/// The trust anchors are stored in PEM format because, in Kubernetes, they are
/// stored in a ConfigMap, and until very recently Kubernetes cannot store
/// binary data in ConfigMaps. Also, PEM is the most interoperable way to
/// distribute trust anchors, especially if it is desired to support multiple
/// trust anchors at once.
///
/// The end-entity certificate and private key are in DER format because they
/// are stored in the secret store where space utilization is a concern, and
/// because PEM doesn't offer any advantages.
#[derive(Clone, Debug)]
pub struct CommonSettings {
    /// The trust anchors as concatenated PEM-encoded X.509 certificates.
    pub trust_anchors: PathBuf,

    /// The end-entity certificate as a DER-encoded X.509 certificate.
    pub end_entity_cert: PathBuf,

    /// The private key in DER-encoded PKCS#8 form.
    pub private_key: PathBuf,
}

/// Validated configuration common between TLS clients and TLS servers.
pub struct CommonConfig {
    cert_resolver: Arc<CertResolver>,
}

/// Validated configuration for TLS clients.
///
/// TODO: Fill this in with the actual configuration.
#[derive(Clone, Debug)]
pub struct ClientConfig(Arc<()>);

/// Validated configuration for TLS servers.
#[derive(Clone)]
pub struct ServerConfig(pub(super) Arc<rustls::ServerConfig>);

pub type ClientConfigWatch = Watch<Option<ClientConfig>>;
pub type ServerConfigWatch = Watch<Option<ServerConfig>>;

#[derive(Debug)]
pub enum Error {
    Io(PathBuf, io::Error),
    FailedToParseTrustAnchors(Option<webpki::Error>),
    EndEntityCertIsNotValid(webpki::Error),
    InvalidPrivateKey,
    TimeConversionFailed,
}

impl CommonSettings {
    fn paths(&self) -> [&PathBuf; 3] {
        [
            &self.trust_anchors,
            &self.end_entity_cert,
            &self.private_key,
        ]
    }

    /// Stream changes to the files described by this `CommonSettings`.
    ///
    /// The returned stream consists of each subsequent successfully loaded
    /// `CommonSettings` after each change. If the settings could not be
    /// reloaded (i.e., they were malformed), nothing is sent.
    pub fn stream_changes(self, interval: Duration)
        -> impl Stream<Item = CommonConfig, Error = ()>
    {
        let paths = self.paths().iter()
            .map(|&p| p.clone())
            .collect::<Vec<_>>();
        ::fs_watch::stream_changes(paths, interval)
            .filter_map(move |_| {
                CommonConfig::load_from_disk(&self)
                    .map_err(|e| warn!("error reloading TLS config: {:?}, falling back", e))
                    .ok()
            })
    }

}

impl CommonConfig {
    /// Loads a configuration from the given files and validates it. If an
    /// error is returned then the caller should try again after the files are
    /// updated.
    ///
    /// In a valid configuration, all the files need to be in sync with each
    /// other. For example, the private key file must contain the private
    /// key for the end-entity certificate, and the end-entity certificate
    /// must be issued by the CA represented by a certificate in the
    /// trust anchors file. Since filesystem operations are not atomic, we
    /// need to check for this consistency.
    fn load_from_disk(settings: &CommonSettings) -> Result<Self, Error> {
        let trust_anchor_certs = load_file_contents(&settings.trust_anchors)
            .and_then(|file_contents|
                rustls::internal::pemfile::certs(&mut Cursor::new(file_contents))
                    .map_err(|()| Error::FailedToParseTrustAnchors(None)))?;
        let mut trust_anchors = Vec::with_capacity(trust_anchor_certs.len());
        for ta in &trust_anchor_certs {
            let ta = webpki::trust_anchor_util::cert_der_as_trust_anchor(
                untrusted::Input::from(ta.as_ref()))
                .map_err(|e| Error::FailedToParseTrustAnchors(Some(e)))?;
            trust_anchors.push(ta);
        }
        let trust_anchors = webpki::TLSServerTrustAnchors(&trust_anchors);

        let end_entity_cert = load_file_contents(&settings.end_entity_cert)?;

        // XXX: Assume there are no intermediates since there is no way to load
        // them yet.
        let cert_chain = vec![rustls::Certificate(end_entity_cert)];

        // Load the private key after we've validated the certificate.
        let private_key = load_file_contents(&settings.private_key)?;
        let private_key = untrusted::Input::from(&private_key);

        // `CertResolver::new` is responsible for the consistency check.
        let cert_resolver = CertResolver::new(&trust_anchors, cert_chain, private_key)?;

        Ok(Self {
            cert_resolver: Arc::new(cert_resolver),
        })
    }

}

pub fn watch_for_config_changes(settings: Option<&CommonSettings>)
    -> (ClientConfigWatch, ServerConfigWatch, Box<Future<Item = (), Error = ()> + Send>)
{
    let settings = if let Some(settings) = settings {
        settings.clone()
    } else {
        let (client_watch, _) = Watch::new(None);
        let (server_watch, _) = Watch::new(None);
        let no_future = future::ok(());
        return (client_watch, server_watch, Box::new(no_future));
    };

    let changes = settings.stream_changes(Duration::from_secs(1));
    let (client_watch, client_store) = Watch::new(None);
    let (server_watch, server_store) = Watch::new(None);

    // `Store::store` will return an error iff all watchers have been dropped,
    // so we'll use `fold` to cancel the forwarding future. Eventually, we can
    // also use the fold to continue tracking previous states if we need to do
    // that.
    let f = changes
        .fold(
            (client_store, server_store),
            |(mut client_store, mut server_store), ref config| {
                client_store
                    .store(Some(ClientConfig(Arc::new(()))))
                    .map_err(|_| trace!("all client config watchers dropped"))?;
                server_store
                    .store(Some(ServerConfig::from(config)))
                    .map_err(|_| trace!("all server config watchers dropped"))?;
                Ok((client_store, server_store))
            })
        .then(|_| {
            trace!("forwarding to server config watch finished.");
            Ok(())
        });

    // This function and `ServerConfig::no_tls` return `Box<Future<...>>`
    // rather than `impl Future<...>` so that they can have the _same_ return
    // types (impl Traits are not the same type unless the original
    // non-anonymized type was the same).
    (client_watch, server_watch, Box::new(f))
}

impl ServerConfig {
    fn from(common: &CommonConfig) -> Self {
        let mut config = rustls::ServerConfig::new(Arc::new(rustls::NoClientAuth));
        set_common_settings(&mut config.versions);
        config.cert_resolver = common.cert_resolver.clone();
        ServerConfig(Arc::new(config))
    }

    pub fn no_tls()
        -> (ServerConfigWatch, Box<Future<Item = (), Error = ()> + Send>)
    {
            let (watch, _) = Watch::new(None);
            let no_future = future::ok(());

            (watch, Box::new(no_future))
    }
}

fn load_file_contents(path: &PathBuf) -> Result<Vec<u8>, Error> {
    fn load_file(path: &PathBuf) -> Result<Vec<u8>, io::Error> {
        let mut result = Vec::new();
        let mut file = File::open(path)?;
        loop {
            match file.read_to_end(&mut result) {
                Ok(_) => {
                    return Ok(result);
                },
                Err(e) => {
                    if e.kind() != io::ErrorKind::Interrupted {
                        return Err(e);
                    }
                },
            }
        }
    }

    load_file(path)
        .map(|contents| {
            trace!("loaded file {:?}", path);
            contents
        })
        .map_err(|e| Error::Io(path.clone(), e))
}

fn set_common_settings(versions: &mut Vec<rustls::ProtocolVersion>) {
    // Only enable TLS 1.2 until TLS 1.3 is stable.
    *versions = vec![rustls::ProtocolVersion::TLSv1_2]
}

#[cfg(test)]
mod tests {
    use super::*;
    use ::fs_watch;
    use task::test_util::BlockOnFor;

    use tempdir::TempDir;
    use tokio::runtime::current_thread::Runtime;

    use std::{
        io::Write,
        fs::{self, File},
    };
    #[cfg(not(target_os = "windows"))]
    use std::os::unix::fs::symlink;

    use futures::{Sink, Stream};
    use futures_watch::Watch;

    struct Fixture {
        cfg: CommonSettings,
        dir: TempDir,
        rt: Runtime,
    }

    const END_ENTITY_CERT: &'static str = "test-test.crt";
    const PRIVATE_KEY: &'static str = "test-test.p8";
    const TRUST_ANCHORS: &'static str = "ca.pem";

    fn fixture() -> Fixture {
        let _ = ::env_logger::try_init();
        let dir = TempDir::new("certs").expect("temp dir");
        let cfg = CommonSettings {
            trust_anchors: dir.path().join(TRUST_ANCHORS),
            end_entity_cert: dir.path().join(END_ENTITY_CERT),
            private_key: dir.path().join(PRIVATE_KEY),
        };
        let rt = Runtime::new().expect("runtime");
        Fixture { cfg, dir, rt }
    }

    fn watch_stream(stream: impl Stream<Item = (), Error = ()> + 'static)
        -> (Watch<()>, Box<Future<Item = (), Error = ()>>)
    {
        let (watch, store) = Watch::new(());
        // Use a watch so we can start running the stream immediately but also
        // wait on stream updates.
        let f = stream
            .forward(store.sink_map_err(|_| ()))
            .map(|_| ())
            .map_err(|_| ());

        (watch, Box::new(f))
    }

    fn test_detects_create(
        fixture: Fixture,
        stream: impl Stream<Item = (), Error=()> + 'static,
    ) {
        let Fixture { cfg, dir: _dir, mut rt } = fixture;

        let (watch, bg) = watch_stream(stream);
        rt.spawn(bg);

        let f = File::create(cfg.trust_anchors)
            .expect("create trust anchors");
        f.sync_all().expect("create trust anchors");
        println!("created {:?}", f);

        let next = watch.into_future().map_err(|(e, _)| e);
        let (item, watch) = rt.block_on_for(Duration::from_secs(2), next)
            .expect("first change");
        assert!(item.is_some());

        let f = File::create(cfg.end_entity_cert)
            .expect("create end entity cert");
        f.sync_all()
            .expect("sync end entity cert");
        println!("created {:?}", f);

        let next = watch.into_future().map_err(|(e, _)| e);
        let (item, watch) = rt.block_on_for(Duration::from_secs(2), next)
            .expect("second change");
        assert!(item.is_some());

        let f = File::create(cfg.private_key)
            .expect("create private key");
        f.sync_all()
            .expect("sync private key");
        println!("created {:?}", f);

        let next = watch.into_future().map_err(|(e, _)| e);
        let (item, _) = rt.block_on_for(Duration::from_secs(2), next)
            .expect("third change");
        assert!(item.is_some());
    }

    fn test_detects_delete_and_recreate(
        fixture: Fixture,
        stream: impl Stream<Item = (), Error=()> + 'static,
    ) {
        let _ = ::env_logger::try_init();

        let Fixture { cfg, dir: _dir, mut rt } = fixture;

        let (watch, bg) = watch_stream(stream);
        rt.spawn(bg);

        let f = File::create(cfg.trust_anchors)
            .expect("create trust anchors");
        f.sync_all().expect("create trust anchors");
        println!("created {:?}", f);

        let next = watch.into_future().map_err(|(e, _)| e);
        let (item, watch) = rt.block_on_for(Duration::from_secs(2), next)
            .expect("first change");
        assert!(item.is_some());

        let f = File::create(cfg.end_entity_cert)
            .expect("create end entity cert");
        f.sync_all()
            .expect("sync end entity cert");
        println!("created {:?}", f);

        let next = watch.into_future().map_err(|(e, _)| e);
        let (item, watch) = rt.block_on_for(Duration::from_secs(2), next)
            .expect("second change");
        assert!(item.is_some());

        let mut f = File::create(&cfg.private_key)
            .expect("create private key");
        f.write_all(b"i'm the first private key")
            .expect("write private key once");
        f.sync_all()
            .expect("sync private key");
        println!("created {:?}", f);

        let next = watch.into_future().map_err(|(e, _)| e);
        let (item, watch) = rt.block_on_for(Duration::from_secs(2), next)
            .expect("third change");
        assert!(item.is_some());

        fs::remove_file(&cfg.private_key).expect("remove private key");
        println!("deleted {:?}", f);

        let mut f = File::create(&cfg.private_key)
            .expect("rereate private key");
        f.write_all(b"i'm the new private key")
            .expect("write private key once");
        f.sync_all()
            .expect("sync private key");
        println!("recreated {:?}", f);

        let next = watch.into_future().map_err(|(e, _)| e);
        let (item, _) = rt.block_on_for(Duration::from_secs(2), next)
            .expect("fourth change");
        assert!(item.is_some());
    }

    #[cfg(not(target_os = "windows"))]
    fn test_detects_create_symlink(
        fixture: Fixture,
        stream: impl Stream<Item = (), Error=()> + 'static,
    ) {
        let Fixture { cfg, dir, mut rt } = fixture;

        let data_path = dir.path().join("data");
        fs::create_dir(&data_path).expect("create data dir");

        let trust_anchors_path = data_path.clone().join(TRUST_ANCHORS);
        let end_entity_cert_path = data_path.clone().join(END_ENTITY_CERT);
        let private_key_path = data_path.clone().join(PRIVATE_KEY);

        let end_entity_cert = File::create(&end_entity_cert_path)
            .expect("create end entity cert");
        end_entity_cert.sync_all()
            .expect("sync end entity cert");
        let private_key = File::create(&private_key_path)
            .expect("create private key");
        private_key.sync_all()
            .expect("sync private key");
        let trust_anchors = File::create(&trust_anchors_path)
            .expect("create trust anchors");
        trust_anchors.sync_all()
            .expect("sync trust_anchors");

        let (watch, bg) = watch_stream(stream);
        rt.spawn(bg);

        symlink(trust_anchors_path, cfg.trust_anchors)
            .expect("symlink trust anchors");

        let next = watch.into_future().map_err(|(e, _)| e);
        let (item, watch) = rt.block_on_for(Duration::from_secs(2), next)
            .expect("first change");
        assert!(item.is_some());

        symlink(private_key_path, cfg.private_key)
            .expect("symlink private key");

        let next = watch.into_future().map_err(|(e, _)| e);
        let (item, watch) = rt.block_on_for(Duration::from_secs(2), next)
            .expect("second change");
        assert!(item.is_some());

        symlink(end_entity_cert_path, cfg.end_entity_cert)
            .expect("symlink end entity cert");

        let next = watch.into_future().map_err(|(e, _)| e);
        let (item, _) = rt.block_on_for(Duration::from_secs(2), next)
            .expect("third change");
        assert!(item.is_some());
    }

    // Test for when the watched files are symlinks to a file insdie of a
    // directory which is also a symlink (as is the case with Kubernetes
    // ConfigMap/Secret volume mounts).
    #[cfg(not(target_os = "windows"))]
    fn test_detects_create_double_symlink(
        fixture: Fixture,
        stream: impl Stream<Item = (), Error=()> + 'static,
    ) {
        let Fixture { cfg, dir, mut rt } = fixture;

        let real_data_path = dir.path().join("real_data");
        let data_path = dir.path().join("data");
        fs::create_dir(&real_data_path).expect("create data dir");
        symlink(&real_data_path, &data_path).expect("create data dir symlink");

        let end_entity_cert = File::create(real_data_path.clone().join(END_ENTITY_CERT))
            .expect("create end entity cert");
        end_entity_cert.sync_all()
            .expect("sync end entity cert");
        let private_key = File::create(real_data_path.clone().join(PRIVATE_KEY))
            .expect("create private key");
        private_key.sync_all()
            .expect("sync private key");
        let trust_anchors = File::create(real_data_path.clone().join(TRUST_ANCHORS))
            .expect("create trust anchors");
        trust_anchors.sync_all()
            .expect("sync private key");

        let (watch, bg) = watch_stream(stream);
        rt.spawn(bg);

        symlink(data_path.clone().join(TRUST_ANCHORS), cfg.trust_anchors)
            .expect("symlink trust anchors");

        let next = watch.into_future().map_err(|(e, _)| e);
        let (item, watch) = rt.block_on_for(Duration::from_secs(2), next)
            .expect("first change");
        assert!(item.is_some());

        symlink(data_path.clone().join(PRIVATE_KEY), cfg.private_key)
            .expect("symlink private key");

        let next = watch.into_future().map_err(|(e, _)| e);
        let (item, watch) = rt.block_on_for(Duration::from_secs(2), next)
            .expect("second change");
        assert!(item.is_some());

        symlink(real_data_path.clone().join(END_ENTITY_CERT), cfg.end_entity_cert)
            .expect("symlink end entity cert");

        let next = watch.into_future().map_err(|(e, _)| e);
        let (item, _) = rt.block_on_for(Duration::from_secs(2), next)
            .expect("third change");
        assert!(item.is_some());
    }

    #[cfg(not(target_os = "windows"))]
    fn test_detects_modification_symlink(
        fixture: Fixture,
        stream: impl Stream<Item = (), Error=()> + 'static,
    ) {
        let Fixture { cfg, dir, mut rt } = fixture;

        let data_path = dir.path().join("data");
        fs::create_dir(&data_path).expect("create data dir");

        let trust_anchors_path = data_path.clone().join(TRUST_ANCHORS);
        let end_entity_cert_path = data_path.clone().join(END_ENTITY_CERT);
        let private_key_path = data_path.clone().join(PRIVATE_KEY);

        let mut trust_anchors = File::create(&trust_anchors_path)
            .expect("create trust anchors");
        println!("created {:?}", trust_anchors);
        trust_anchors.write_all(b"I am not real trust anchors")
            .expect("write to trust anchors");
        trust_anchors.sync_all().expect("sync trust anchors");

        let mut private_key = File::create(&private_key_path)
            .expect("create private key");
        println!("created {:?}", private_key);
        private_key.write_all(b"I am not a realprivate key")
            .expect("write to private key");
        private_key.sync_all().expect("sync private key");

        let mut end_entity_cert = File::create(&end_entity_cert_path)
            .expect("create end entity cert");
        println!("created {:?}", end_entity_cert);
        end_entity_cert.write_all(b"I am not real end entity cert")
            .expect("write to end entity cert");
        end_entity_cert.sync_all().expect("sync end entity cert");

        symlink(private_key_path, cfg.private_key)
            .expect("symlink private key");
        symlink(end_entity_cert_path, cfg.end_entity_cert)
            .expect("symlink end entity cert");
        symlink(trust_anchors_path, cfg.trust_anchors)
            .expect("symlink trust anchors");

        let (watch, bg) = watch_stream(stream);
        rt.spawn(Box::new(bg));

        trust_anchors.write_all(b"Trust me on this :)")
            .expect("write to trust anchors again");
        trust_anchors.sync_all()
            .expect("sync trust anchors again");

        let next = watch.into_future().map_err(|(e, _)| e);
        let (item, watch) = rt.block_on_for(Duration::from_secs(2), next)
            .expect("first change");
        assert!(item.is_some());
        println!("saw first change");

        end_entity_cert.write_all(b"This is the end of the end entity cert :)")
            .expect("write to end entity cert again");
        end_entity_cert.sync_all()
            .expect("sync end entity cert again");

        let next = watch.into_future().map_err(|(e, _)| e);
        let (item, watch) = rt.block_on_for(Duration::from_secs(2), next)
            .expect("second change");
        assert!(item.is_some());
        println!("saw second change");

        private_key.write_all(b"Keep me private :)")
            .expect("write to private key");
        private_key.sync_all()
            .expect("sync private key again");

        let next = watch.into_future().map_err(|(e, _)| e);
        let (item, _) = rt.block_on_for(Duration::from_secs(2), next)
            .expect("third change");
        assert!(item.is_some());
        println!("saw third change");
    }

    fn test_detects_modification(
        fixture: Fixture,
        stream: impl Stream<Item = (), Error=()> + 'static,
    ) {
        let Fixture { cfg, dir: _dir, mut rt } = fixture;

        let mut trust_anchors = File::create(cfg.trust_anchors.clone())
            .expect("create trust anchors");
        println!("created {:?}", trust_anchors);
        trust_anchors.write_all(b"I am not real trust anchors")
            .expect("write to trust anchors");
        trust_anchors.sync_all().expect("sync trust anchors");

        let mut private_key = File::create(cfg.private_key.clone())
            .expect("create private key");
        println!("created {:?}", private_key);
        private_key.write_all(b"I am not a realprivate key")
            .expect("write to private key");
        private_key.sync_all().expect("sync private key");

        let mut end_entity_cert = File::create(cfg.end_entity_cert.clone())
            .expect("create end entity cert");
        println!("created {:?}", end_entity_cert);
        end_entity_cert.write_all(b"I am not real end entity cert")
            .expect("write to end entity cert");
        end_entity_cert.sync_all().expect("sync end entity cert");

        let (watch, bg) = watch_stream(stream);
        rt.spawn(Box::new(bg));

        trust_anchors.write_all(b"Trust me on this :)")
            .expect("write to trust anchors again");
        trust_anchors.sync_all()
            .expect("sync trust anchors again");

        let next = watch.into_future().map_err(|(e, _)| e);
        let (item, watch) = rt.block_on_for(Duration::from_secs(2), next)
            .expect("first change");
        assert!(item.is_some());
        println!("saw first change");

        end_entity_cert.write_all(b"This is the end of the end entity cert :)")
            .expect("write to end entity cert again");
        end_entity_cert.sync_all()
            .expect("sync end entity cert again");

        let next = watch.into_future().map_err(|(e, _)| e);
        let (item, watch) = rt.block_on_for(Duration::from_secs(2), next)
            .expect("second change");
        assert!(item.is_some());
        println!("saw second change");

        private_key.write_all(b"Keep me private :)")
            .expect("write to private key");
        private_key.sync_all()
            .expect("sync private key again");

        let next = watch.into_future().map_err(|(e, _)| e);
        let (item, _) = rt.block_on_for(Duration::from_secs(2), next)
            .expect("third change");
        assert!(item.is_some());
        println!("saw third change");
    }

    #[cfg(not(target_os = "windows"))]
    fn test_detects_modification_double_symlink(
        fixture: Fixture,
        stream: impl Stream<Item = (), Error=()> + 'static,
    ) {
        let Fixture { cfg, dir, mut rt } = fixture;

        let real_data_path = dir.path().join("real_data");
        let data_path = dir.path().join("data");
        fs::create_dir(&real_data_path).expect("create data dir");
        symlink(&real_data_path, &data_path).expect("create data dir symlink");

        let mut trust_anchors = File::create(real_data_path.clone().join(TRUST_ANCHORS))
            .expect("create trust anchors");
        println!("created {:?}", trust_anchors);
        trust_anchors.write_all(b"I am not real trust anchors")
            .expect("write to trust anchors");
        trust_anchors.sync_all().expect("sync trust anchors");

        let mut private_key = File::create(real_data_path.clone().join(PRIVATE_KEY))
            .expect("create private key");
        println!("created {:?}", private_key);
        private_key.write_all(b"I am not a realprivate key")
            .expect("write to private key");
        private_key.sync_all().expect("sync private key");

        let mut end_entity_cert = File::create(real_data_path.clone().join(END_ENTITY_CERT))
            .expect("create end entity cert");
        println!("created {:?}", end_entity_cert);
        end_entity_cert.write_all(b"I am not real end entity cert")
            .expect("write to end entity cert");
        end_entity_cert.sync_all().expect("sync end entity cert");

        symlink(data_path.clone().join(PRIVATE_KEY), cfg.private_key)
            .expect("symlink private key");
        symlink(data_path.clone().join(END_ENTITY_CERT), cfg.end_entity_cert)
            .expect("symlink end entity cert");
        symlink(data_path.clone().join(TRUST_ANCHORS), cfg.trust_anchors)
            .expect("symlink trust anchors");

        let (watch, bg) = watch_stream(stream);
        rt.spawn(Box::new(bg));

        trust_anchors.write_all(b"Trust me on this :)")
            .expect("write to trust anchors again");
        trust_anchors.sync_all()
            .expect("sync trust anchors again");

        let next = watch.into_future().map_err(|(e, _)| e);
        let (item, watch) = rt.block_on_for(Duration::from_secs(2), next)
            .expect("first change");
        assert!(item.is_some());
        println!("saw first change");

        end_entity_cert.write_all(b"This is the end of the end entity cert :)")
            .expect("write to end entity cert again");
        end_entity_cert.sync_all()
            .expect("sync end entity cert again");

        let next = watch.into_future().map_err(|(e, _)| e);
        let (item, watch) = rt.block_on_for(Duration::from_secs(2), next)
            .expect("second change");
        assert!(item.is_some());
        println!("saw second change");

        private_key.write_all(b"Keep me private :)")
            .expect("write to private key");
        private_key.sync_all()
            .expect("sync private key again");

        let next = watch.into_future().map_err(|(e, _)| e);
        let (item, _) = rt.block_on_for(Duration::from_secs(2), next)
            .expect("third change");
        assert!(item.is_some());
        println!("saw third change");
    }

    #[cfg(not(target_os = "windows"))]
    fn test_detects_double_symlink_retargeting(
        fixture: Fixture,
        stream: impl Stream<Item = (), Error=()> + 'static,
    ) {
        let Fixture { cfg, dir, mut rt } = fixture;

        let real_data_path = dir.path().join("real_data");
        let real_data_path_2 = dir.path().join("real_data_2");
        let data_path = dir.path().join("data");
        fs::create_dir(&real_data_path).expect("create data dir");
        fs::create_dir(&real_data_path_2).expect("create data dir 2");
        symlink(&real_data_path, &data_path).expect("create data dir symlink");

        let mut trust_anchors = File::create(real_data_path.clone().join(TRUST_ANCHORS))
            .expect("create trust anchors");
        println!("created {:?}", trust_anchors);
        trust_anchors.write_all(b"I am not real trust anchors")
            .expect("write to trust anchors");
        trust_anchors.sync_all().expect("sync trust anchors");

        let mut private_key = File::create(real_data_path.clone().join(PRIVATE_KEY))
            .expect("create private key");
        println!("created {:?}", private_key);
        private_key.write_all(b"I am not a realprivate key")
            .expect("write to private key");
        private_key.sync_all().expect("sync private key");

        let mut end_entity_cert = File::create(real_data_path.clone().join(END_ENTITY_CERT))
            .expect("create end entity cert");
        println!("created {:?}", end_entity_cert);
        end_entity_cert.write_all(b"I am not real end entity cert")
            .expect("write to end entity cert");
        end_entity_cert.sync_all().expect("sync end entity cert");

        let mut trust_anchors = File::create(real_data_path_2.clone().join(TRUST_ANCHORS))
            .expect("create trust anchors 2");
        println!("created {:?}", trust_anchors);
        trust_anchors.write_all(b"I am not real trust anchors either")
            .expect("write to trust anchors 2");
        trust_anchors.sync_all().expect("sync trust anchors 2");

        let mut private_key = File::create(real_data_path_2.clone().join(PRIVATE_KEY))
            .expect("create private key 2");
        println!("created {:?}", private_key);
        private_key.write_all(b"I am not a real private key either")
            .expect("write to private key 2");
        private_key.sync_all().expect("sync private key 2");

        let mut end_entity_cert = File::create(real_data_path_2.clone().join(END_ENTITY_CERT))
            .expect("create end entity cert 2");
        println!("created {:?}", end_entity_cert);
        end_entity_cert.write_all(b"I am not real end entity cert either")
            .expect("write to end entity cert 2");
        end_entity_cert.sync_all().expect("sync end entity cert 2");

        symlink(data_path.clone().join(PRIVATE_KEY), cfg.private_key)
            .expect("symlink private key");
        symlink(data_path.clone().join(END_ENTITY_CERT), cfg.end_entity_cert)
            .expect("symlink end entity cert");
        symlink(data_path.clone().join(TRUST_ANCHORS), cfg.trust_anchors)
            .expect("symlink trust anchors");

        let (watch, bg) = watch_stream(stream);
        rt.spawn(Box::new(bg));

        fs::remove_dir_all(&data_path)
            .expect("remove original data dir symlink");
        symlink(&real_data_path_2, &data_path)
            .expect("create second data dir symlink");

        let next = watch.into_future().map_err(|(e, _)| e);
        let (item, _) = rt.block_on_for(Duration::from_secs(2), next)
            .expect("first change");
        assert!(item.is_some());
        println!("saw first change");
    }


    #[test]
    fn polling_detects_create() {
        let fixture = fixture();
        let paths = fixture.cfg.paths().iter()
            .map(|&p| p.clone())
            .collect::<Vec<_>>();
        let stream = fs_watch::stream_changes_polling(
            paths,
            Duration::from_secs(1)
        );
        test_detects_create(fixture, stream)
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn inotify_detects_create() {
        let fixture = fixture();
        let paths = fixture.cfg.paths().iter().collect();
        let stream = fs_watch::inotify::WatchStream::new(paths)
            .expect("create watch")
            .map_err(|e| panic!("{}", e));
        test_detects_create(fixture, stream)
    }

    #[test]
    #[cfg(not(target_os = "windows"))]
    fn polling_detects_create_symlink() {
        let fixture = fixture();
        let paths = fixture.cfg.paths().iter()
            .map(|&p| p.clone())
            .collect::<Vec<_>>();
        let stream = fs_watch::stream_changes_polling(
            paths,
            Duration::from_secs(1)
        );
        test_detects_create_symlink(fixture, stream)
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn inotify_detects_create_symlink() {
        let fixture = fixture();
        let paths = fixture.cfg.paths().iter().collect();
        let stream = fs_watch::inotify::WatchStream::new(paths)
            .expect("create watch")
            .map_err(|e| panic!("{}", e));
        test_detects_create_symlink(fixture, stream)
    }

    #[test]
    #[cfg(not(target_os = "windows"))]
    fn polling_detects_create_double_symlink() {
        let fixture = fixture();
        let paths = fixture.cfg.paths().iter()
            .map(|&p| p.clone())
            .collect::<Vec<_>>();
        let stream = fs_watch::stream_changes_polling(
            paths,
            Duration::from_secs(1)
        );
        test_detects_create_double_symlink(fixture, stream)
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn inotify_detects_create_double_symlink() {
        let fixture = fixture();
        let paths = fixture.cfg.paths().iter().collect();
        let stream = fs_watch::inotify::WatchStream::new(paths)
            .expect("create watch")
            .map_err(|e| panic!("{}", e));
        test_detects_create_double_symlink(fixture, stream)
    }

    #[test]
    fn polling_detects_modification() {
        let fixture = fixture();
        let paths = fixture.cfg.paths().iter()
            .map(|&p| p.clone())
            .collect::<Vec<_>>();
        let stream = fs_watch::stream_changes_polling(
            paths,
            Duration::from_secs(1)
        );
        test_detects_modification(fixture, stream)
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn inotify_detects_modification() {
        let fixture = fixture();
        let paths = fixture.cfg.paths().iter().collect();
        let stream = fs_watch::inotify::WatchStream::new(paths)
            .expect("create watch")
            .map_err(|e| panic!("{}", e));
        test_detects_modification(fixture, stream)
    }

    #[test]
    #[cfg(not(target_os = "windows"))]
    fn polling_detects_modification_symlink() {
        let fixture = fixture();
        let paths = fixture.cfg.paths().iter()
            .map(|&p| p.clone())
            .collect::<Vec<_>>();
        let stream = fs_watch::stream_changes_polling(
            paths,
            Duration::from_secs(1)
        );
        test_detects_modification_symlink(fixture, stream)
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn inotify_detects_modification_symlink() {
        let fixture = fixture();
        let paths = fixture.cfg.paths().iter().collect();
        let stream = fs_watch::inotify::WatchStream::new(paths)
            .expect("create watch")
            .map_err(|e| panic!("{}", e));
        test_detects_modification_symlink(fixture, stream)
    }

    #[test]
    #[cfg(not(target_os = "windows"))]
    fn polling_detects_modification_double_symlink() {
        let fixture = fixture();
        let paths = fixture.cfg.paths().iter()
            .map(|&p| p.clone())
            .collect::<Vec<_>>();
        let stream = fs_watch::stream_changes_polling(
            paths,
            Duration::from_secs(1)
        );
        test_detects_modification_double_symlink(fixture, stream)
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn inotify_detects_modification_double_symlink() {
        let fixture = fixture();
        let paths = fixture.cfg.paths().iter().collect();
        let stream = fs_watch::inotify::WatchStream::new(paths)
            .expect("create watch")
            .map_err(|e| panic!("{}", e));
        test_detects_modification_double_symlink(fixture, stream)
    }

    #[test]
    #[cfg(not(target_os = "windows"))]
    fn polling_detects_double_symlink_retargeting() {
        let fixture = fixture();
        let paths = fixture.cfg.paths().iter()
            .map(|&p| p.clone())
            .collect::<Vec<_>>();
        let stream = fs_watch::stream_changes_polling(
            paths,
            Duration::from_secs(1)
        );
        test_detects_double_symlink_retargeting(fixture, stream)
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn inotify_detects_double_symlink_retargeting() {
        let fixture = fixture();
        let paths = fixture.cfg.paths().iter().collect();
        let stream = fs_watch::inotify::WatchStream::new(paths)
            .expect("create watch")
            .map_err(|e| panic!("{}", e));
        test_detects_double_symlink_retargeting(fixture, stream)
    }

    #[test]
    fn polling_detects_delete_and_recreate() {
        let fixture = fixture();
        let paths = fixture.cfg.paths().iter()
            .map(|&p| p.clone())
            .collect::<Vec<_>>();
        let stream = fs_watch::stream_changes_polling(
            paths,
            Duration::from_secs(1)
        );
        test_detects_delete_and_recreate(fixture, stream)
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn inotify_detects_delete_and_recreate() {
        let fixture = fixture();
        let paths = fixture.cfg.paths().iter().collect();
        let stream = fs_watch::inotify::WatchStream::new(paths)
            .expect("create watch")
            .map_err(|e| panic!("{}", e));
        test_detects_delete_and_recreate(fixture, stream)
    }

}
