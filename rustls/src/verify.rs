use parking_lot::RwLock;
use ring::digest::Digest;
use std::convert::TryFrom;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;
use std::time::SystemTime;

use crate::anchors::OwnedTrustAnchor;
use crate::anchors::{DistinguishedNames, RootCertStore};
use crate::error::Error;
use crate::error::WebPkiOp;
use crate::key::Certificate;
#[cfg(feature = "logging")]
use crate::log::{debug, trace, warn};
use crate::msgs::enums::SignatureScheme;
use crate::msgs::handshake::DigitallySignedStruct;

type SignatureAlgorithms = &'static [&'static webpki::SignatureAlgorithm];

/// Which signature verification mechanisms we support.  No particular
/// order.
static SUPPORTED_SIG_ALGS: SignatureAlgorithms = &[
    &webpki::ECDSA_P256_SHA256,
    &webpki::ECDSA_P256_SHA384,
    &webpki::ECDSA_P384_SHA256,
    &webpki::ECDSA_P384_SHA384,
    &webpki::ED25519,
    &webpki::RSA_PSS_2048_8192_SHA256_LEGACY_KEY,
    &webpki::RSA_PSS_2048_8192_SHA384_LEGACY_KEY,
    &webpki::RSA_PSS_2048_8192_SHA512_LEGACY_KEY,
    &webpki::RSA_PKCS1_2048_8192_SHA256,
    &webpki::RSA_PKCS1_2048_8192_SHA384,
    &webpki::RSA_PKCS1_2048_8192_SHA512,
    &webpki::RSA_PKCS1_3072_8192_SHA384,
];

/// Marker types.  These are used to bind the fact some verification
/// (certificate chain or handshake signature) has taken place into
/// protocol states.  We use this to have the compiler check that there
/// are no 'goto fail'-style elisions of important checks before we
/// reach the traffic stage.
///
/// These types are public, but cannot be directly constructed.  This
/// means their origins can be precisely determined by looking
/// for their `assertion` constructors.
pub struct HandshakeSignatureValid(());
impl HandshakeSignatureValid {
    /// Make a `HandshakeSignatureValid`
    pub fn assertion() -> Self {
        Self { 0: () }
    }
}

pub struct FinishedMessageVerified(());
impl FinishedMessageVerified {
    pub fn assertion() -> Self {
        Self { 0: () }
    }
}

/// Zero-sized marker type representing verification of a server cert chain.
pub struct ServerCertVerified(());
impl ServerCertVerified {
    /// Make a `ServerCertVerified`
    pub fn assertion() -> Self {
        Self { 0: () }
    }
}

/// Zero-sized marker type representing verification of a client cert chain.
pub struct ClientCertVerified(());
impl ClientCertVerified {
    /// Make a `ClientCertVerified`
    pub fn assertion() -> Self {
        Self { 0: () }
    }
}

/// Something that can verify a server certificate chain, and verify
/// signatures made by certificates.
pub trait ServerCertVerifier: Send + Sync {
    /// Verify the end-entity certificate `end_entity` is valid for the
    /// hostname `dns_name` and chains to at least one trust anchor.
    ///
    /// `intermediates` contains the intermediate certificates the client sent
    /// along with the end-entity certificate; it is in the same order that the
    /// peer sent them and may be empty.
    ///
    /// `scts` contains the Signed Certificate Timestamps (SCTs) the server
    /// sent with the certificate, if any.
    fn verify_server_cert(
        &self,
        end_entity: &Certificate,
        intermediates: &[Certificate],
        dns_name: webpki::DnsNameRef,
        scts: &mut dyn Iterator<Item = &[u8]>,
        ocsp_response: &[u8],
        now: SystemTime,
    ) -> Result<ServerCertVerified, Error>;

    /// Verify a signature allegedly by the given server certificate.
    ///
    /// `message` is not hashed, and needs hashing during the verification.
    /// The signature and algorithm are within `dss`.  `cert` contains the
    /// public key to use.
    ///
    /// `cert` is the same certificate that was previously validated by a
    /// call to `verify_server_cert`.
    ///
    /// If and only if the signature is valid, return HandshakeSignatureValid.
    /// Otherwise, return an error -- rustls will send an alert and abort the
    /// connection.
    ///
    /// This method is only called for TLS1.2 handshakes.  Note that, in TLS1.2,
    /// SignatureSchemes such as `SignatureScheme::ECDSA_NISTP256_SHA256` are not
    /// in fact bound to the specific curve implied in their name.
    ///
    /// This trait method has a default implementation that uses webpki to verify
    /// the signature.
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &Certificate,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_signed_struct(message, cert, dss)
    }

    /// Verify a signature allegedly by the given server certificate.
    ///
    /// This method is only called for TLS1.3 handshakes.
    ///
    /// This method is very similar to `verify_tls12_signature`: but note the
    /// tighter ECDSA SignatureScheme semantics -- e.g. `SignatureScheme::ECDSA_NISTP256_SHA256`
    /// must only validate signatures using public keys on the right curve --
    /// rustls does not enforce this requirement for you.
    ///
    /// This trait method has a default implementation that uses webpki to verify
    /// the signature.
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &Certificate,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls13(message, cert, dss)
    }

    /// Return the list of SignatureSchemes that this verifier will handle,
    /// in `verify_tls12_signature` and `verify_tls13_signature` calls.
    ///
    /// This should be in priority order, with the most preferred first.
    ///
    /// This trait method has a default implementation that reflects the schemes
    /// supported by webpki.
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        WebPkiVerifier::verification_schemes()
    }

    /// Returns `true` if Rustls should ask the server to send SCTs.
    ///
    /// Signed Certificate Timestamps (SCTs) are used for Certificate
    /// Transparency validation.
    ///
    /// The default implementation of this function returns true.
    fn request_scts(&self) -> bool {
        true
    }
}

/// Something that can verify a client certificate chain
pub trait ClientCertVerifier: Send + Sync {
    /// Returns `true` to enable the server to request a client certificate and
    /// `false` to skip requesting a client certificate. Defaults to `true`.
    fn offer_client_auth(&self) -> bool {
        true
    }

    /// Return `Some(true)` to require a client certificate and `Some(false)` to make
    /// client authentication optional. Return `None` to abort the connection.
    /// Defaults to `Some(self.offer_client_auth())`.
    ///
    /// `sni` is the server name quoted by the client in its ClientHello; it has
    /// been validated as a proper DNS name but is otherwise untrusted.
    fn client_auth_mandatory(&self, _sni: Option<&webpki::DnsName>) -> Option<bool> {
        Some(self.offer_client_auth())
    }

    /// Returns the subject names of the client authentication trust anchors to
    /// share with the client when requesting client authentication.
    ///
    /// Return `None` to abort the connection.
    ///
    /// `sni` is the server name quoted by the client in its ClientHello; it has
    /// been validated as a proper DNS name but is otherwise untrusted.
    fn client_auth_root_subjects(
        &self,
        sni: Option<&webpki::DnsName>,
    ) -> Option<DistinguishedNames>;

    /// Verify the end-entity certificate `end_entity` is valid for the
    /// and chains to at least one of the trust anchors in `roots`.
    ///
    /// `intermediates` contains the intermediate certificates the
    /// client sent along with the end-entity certificate; it is in the same
    /// order that the peer sent them and may be empty.
    ///
    /// `sni` is the server name quoted by the client in its ClientHello; it has
    /// been validated as a proper DNS name but is otherwise untrusted.
    fn verify_client_cert(
        &self,
        end_entity: &Certificate,
        intermediates: &[Certificate],
        sni: Option<&webpki::DnsName>,
        now: SystemTime,
    ) -> Result<ClientCertVerified, Error>;

    /// Verify a signature allegedly by the given server certificate.
    ///
    /// `message` is not hashed, and needs hashing during the verification.
    /// The signature and algorithm are within `dss`.  `cert` contains the
    /// public key to use.
    ///
    /// `cert` is the same certificate that was previously validated by a
    /// call to `verify_server_cert`.
    ///
    /// If and only if the signature is valid, return HandshakeSignatureValid.
    /// Otherwise, return an error -- rustls will send an alert and abort the
    /// connection.
    ///
    /// This method is only called for TLS1.2 handshakes.  Note that, in TLS1.2,
    /// SignatureSchemes such as `SignatureScheme::ECDSA_NISTP256_SHA256` are not
    /// in fact bound to the specific curve implied in their name.
    ///
    /// This trait method has a default implementation that uses webpki to verify
    /// the signature.
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &Certificate,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_signed_struct(message, cert, dss)
    }

    /// Verify a signature allegedly by the given server certificate.
    ///
    /// This method is only called for TLS1.3 handshakes.
    ///
    /// This method is very similar to `verify_tls12_signature`: but note the
    /// tighter ECDSA SignatureScheme semantics -- e.g. `SignatureScheme::ECDSA_NISTP256_SHA256`
    /// must only validate signatures using public keys on the right curve --
    /// rustls does not enforce this requirement for you.
    ///
    /// This trait method has a default implementation that uses webpki to verify
    /// the signature.
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &Certificate,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls13(message, cert, dss)
    }

    /// Return the list of SignatureSchemes that this verifier will handle,
    /// in `verify_tls12_signature` and `verify_tls13_signature` calls.
    ///
    /// This should be in priority order, with the most preferred first.
    ///
    /// This trait method has a default implementation that reflects the schemes
    /// supported by webpki.
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        WebPkiVerifier::verification_schemes()
    }
}

impl ServerCertVerifier for WebPkiVerifier {
    /// Will verify the certificate is valid in the following ways:
    /// - Signed by a  trusted `RootCertStore` CA
    /// - Not Expired
    /// - Valid for DNS entry
    /// - OCSP data is present
    fn verify_server_cert(
        &self,
        end_entity: &Certificate,
        intermediates: &[Certificate],
        dns_name: webpki::DnsNameRef,
        scts: &mut dyn Iterator<Item = &[u8]>,
        ocsp_response: &[u8],
        now: SystemTime,
    ) -> Result<ServerCertVerified, Error> {
        let (cert, chain, trustroots) = prepare(end_entity, intermediates, &self.roots)?;
        let webpki_now = webpki::Time::try_from(now).map_err(|_| Error::FailedToGetCurrentTime)?;

        let cert = cert
            .verify_is_valid_tls_server_cert(
                SUPPORTED_SIG_ALGS,
                &webpki::TlsServerTrustAnchors(&trustroots),
                &chain,
                webpki_now,
            )
            .map_err(|e| Error::WebPkiError(e, WebPkiOp::ValidateServerCert))
            .map(|_| cert)?;

        verify_scts(end_entity, now, scts, &self.ct_logs)?;

        if !ocsp_response.is_empty() {
            trace!("Unvalidated OCSP response: {:?}", ocsp_response.to_vec());
        }

        cert.verify_is_valid_for_dns_name(dns_name)
            .map_err(|e| Error::WebPkiError(e, WebPkiOp::ValidateForDnsName))
            .map(|_| ServerCertVerified::assertion())
    }
}

/// Default `ServerCertVerifier`, see the trait impl for more information.
pub struct WebPkiVerifier {
    roots: RootCertStore,
    ct_logs: &'static [&'static sct::Log<'static>],
}

impl WebPkiVerifier {
    /// Constructs a new `WebPkiVerifier`.
    ///
    /// `roots` is the set of trust anchors to trust for issuing server certs.
    ///
    /// `ct_logs` is the list of logs that are trusted for Certificate
    /// Transparency. Currently CT log enforcement is opportunistic; see
    /// https://github.com/ctz/rustls/issues/479.
    pub fn new(roots: RootCertStore, ct_logs: &'static [&'static sct::Log<'static>]) -> Self {
        Self { roots, ct_logs }
    }

    /// Returns the signature verification methods supported by
    /// webpki.
    pub fn verification_schemes() -> Vec<SignatureScheme> {
        vec![
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::ED25519,
            SignatureScheme::RSA_PSS_SHA512,
            SignatureScheme::RSA_PSS_SHA384,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PKCS1_SHA512,
            SignatureScheme::RSA_PKCS1_SHA384,
            SignatureScheme::RSA_PKCS1_SHA256,
        ]
    }
}

type CertChainAndRoots<'a, 'b> = (
    webpki::EndEntityCert<'a>,
    Vec<&'a [u8]>,
    Vec<webpki::TrustAnchor<'b>>,
);

fn prepare<'a, 'b>(
    end_entity: &'a Certificate,
    intermediates: &'a [Certificate],
    roots: &'b RootCertStore,
) -> Result<CertChainAndRoots<'a, 'b>, Error> {
    // EE cert must appear first.
    let cert = webpki::EndEntityCert::try_from(end_entity.0.as_ref())
        .map_err(|e| Error::WebPkiError(e, WebPkiOp::ParseEndEntity))?;

    let intermediates: Vec<&'a [u8]> = intermediates
        .iter()
        .map(|cert| cert.0.as_ref())
        .collect();

    let trustroots: Vec<webpki::TrustAnchor> = roots
        .roots
        .iter()
        .map(OwnedTrustAnchor::to_trust_anchor)
        .collect();

    Ok((cert, intermediates, trustroots))
}

/// A `ClientCertVerifier` that will ensure that every client provides a trusted
/// certificate, without any name checking.
pub struct AllowAnyAuthenticatedClient {
    roots: RootCertStore,
}

impl AllowAnyAuthenticatedClient {
    /// Construct a new `AllowAnyAuthenticatedClient`.
    ///
    /// `roots` is the list of trust anchors to use for certificate validation.
    pub fn new(roots: RootCertStore) -> Arc<dyn ClientCertVerifier> {
        Arc::new(AllowAnyAuthenticatedClient { roots })
    }
}

impl ClientCertVerifier for AllowAnyAuthenticatedClient {
    fn offer_client_auth(&self) -> bool {
        true
    }

    fn client_auth_mandatory(&self, _sni: Option<&webpki::DnsName>) -> Option<bool> {
        Some(true)
    }

    fn client_auth_root_subjects(
        &self,
        _sni: Option<&webpki::DnsName>,
    ) -> Option<DistinguishedNames> {
        Some(self.roots.subjects())
    }

    fn verify_client_cert(
        &self,
        end_entity: &Certificate,
        intermediates: &[Certificate],
        _sni: Option<&webpki::DnsName>,
        now: SystemTime,
    ) -> Result<ClientCertVerified, Error> {
        let (cert, chain, trustroots) = prepare(end_entity, intermediates, &self.roots)?;
        let now = webpki::Time::try_from(now).map_err(|_| Error::FailedToGetCurrentTime)?;
        cert.verify_is_valid_tls_client_cert(
            SUPPORTED_SIG_ALGS,
            &webpki::TlsClientTrustAnchors(&trustroots),
            &chain,
            now,
        )
        .map_err(|e| Error::WebPkiError(e, WebPkiOp::ValidateClientCert))
        .map(|_| ClientCertVerified::assertion())
    }
}

/// Turns off client authentication.
pub struct NoClientAuth;

impl NoClientAuth {
    /// Constructs a `NoClientAuth` and wraps it in an `Arc`.
    pub fn new() -> Arc<dyn ClientCertVerifier> {
        Arc::new(NoClientAuth)
    }
}

impl ClientCertVerifier for NoClientAuth {
    fn offer_client_auth(&self) -> bool {
        false
    }

    fn client_auth_root_subjects(
        &self,
        _sni: Option<&webpki::DnsName>,
    ) -> Option<DistinguishedNames> {
        unimplemented!();
    }

    fn verify_client_cert(
        &self,
        _end_entity: &Certificate,
        _intermediates: &[Certificate],
        _sni: Option<&webpki::DnsName>,
        _now: SystemTime,
    ) -> Result<ClientCertVerified, Error> {
        unimplemented!();
    }
}

enum ClientCertVerifyMode {
    AllowAnyClient,
    MustVerifyClientCert(AllowAnyAuthenticatedClient),
}

/// A `ClientVerifier` impl which can be set to allow anonymous clients
/// but will reject anonymous clients when set to verify them.
///
/// In the case of `AllowAnyAnonymousOrAuthenticatedClient` (which will
/// accept anonymous clients while verifying others who present their
/// client certificate), a client rejected for presenting a bad certificate
/// can then turn anonymous and be served.
pub struct SafeDefaultClientVerifier {
    mode: RwLock<ClientCertVerifyMode>,
}

impl SafeDefaultClientVerifier {
    /// Creates a new `SafeDefaultClientVerifier` and wraps it in an Arc.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            mode: RwLock::new(ClientCertVerifyMode::MustVerifyClientCert(
                AllowAnyAuthenticatedClient {
                    roots: RootCertStore::empty(),
                },
            )),
        })
    }

    /// Drop the list of acceptable client certificates and start serving
    /// all clients.
    ///
    /// This is a mutating operation managed by interior mutability (mutex).
    pub fn serve_anonymous_clients(&self) {
        *self.mode.write() = ClientCertVerifyMode::AllowAnyClient;
    }

    /// If currently serving anonymous clients, start serving only
    /// authenticated clients. Otherwise, no-op.
    ///
    /// This is possibly a mutating operation managed by interior mutability
    /// (mutex).
    pub fn serve_only_authenticated_clients(&self) {
        let mut mode = self.mode.write();
        match mode.deref() {
            ClientCertVerifyMode::AllowAnyClient => {
                *mode = ClientCertVerifyMode::MustVerifyClientCert(AllowAnyAuthenticatedClient {
                    roots: RootCertStore::empty(),
                });
            }
            ClientCertVerifyMode::MustVerifyClientCert(_) => {}
        };
    }

    /// Returns true if currently serving anonymous clients or if currently
    /// serving authenticated clients but no client certificate has been
    /// stored.
    pub fn is_cert_store_empty(&self) -> bool {
        match self.mode.read().deref() {
            ClientCertVerifyMode::AllowAnyClient => true,
            ClientCertVerifyMode::MustVerifyClientCert(verifier) => verifier.roots.is_empty(),
        }
    }

    /// Returns the number of client certificates stored in the underlying
    /// certificate store. Returns 0 if currently serving anonymous clients.
    pub fn root_cert_store_len(&self) -> usize {
        match self.mode.read().deref() {
            ClientCertVerifyMode::AllowAnyClient => 0,
            ClientCertVerifyMode::MustVerifyClientCert(verifier) => verifier.roots.len(),
        }
    }

    /// Returns the list of Subject Names in the underlying certificate
    /// store if currently serving authenticated clients. Otherwise,
    /// returns None.
    pub fn root_cert_store_subjects(&self) -> Option<DistinguishedNames> {
        match self.mode.read().deref() {
            ClientCertVerifyMode::AllowAnyClient => None,
            ClientCertVerifyMode::MustVerifyClientCert(verifier) => Some(verifier.roots.subjects()),
        }
    }

    /// Adds a trusted client certificate to the underlying `RootCertStore`
    /// if currently serving only authenticated clients. Otherwise, no-op
    /// and None returned.
    ///
    /// This is possibly a mutating operation managed by interior mutability
    /// (mutex).
    pub fn add_trusted_root_ca(&self, der: &Certificate) -> Option<Result<(), webpki::Error>> {
        match self.mode.write().deref_mut() {
            ClientCertVerifyMode::AllowAnyClient => None,
            ClientCertVerifyMode::MustVerifyClientCert(verifier) => Some(verifier.roots.add(der)),
        }
    }

    /// Adds all the given TrustAnchors `anchors` and returns true if
    /// currently serving only authenticated clients. Otherwise, no-op and
    /// false returned.
    ///
    /// This is possibly a mutating operation managed by interior mutability
    /// (mutex).
    pub fn add_server_trust_anchors(
        &self,
        &webpki::TlsServerTrustAnchors(anchors): &webpki::TlsServerTrustAnchors,
    ) -> bool {
        match self.mode.write().deref_mut() {
            ClientCertVerifyMode::AllowAnyClient => false,
            ClientCertVerifyMode::MustVerifyClientCert(verifier) => {
                verifier
                    .roots
                    .add_server_trust_anchors(&webpki::TlsServerTrustAnchors(anchors));
                true
            }
        }
    }

    /// Parses the given DER-encoded certificates and add all that can be parsed
    /// to the underlying authenticated client certificate store
    /// in a best-effort fashion if currently serving only authenticated clients.
    ///
    /// This is because large collections of root certificates often
    /// include ancient or syntactically invalid certificates.
    ///
    /// Returns the number of certificates added, and the number that were ignored
    /// wrapped inside `Some`. If currently serving anonymous clients, returns
    /// `None` and the function is a no-op.
    ///
    /// This is possibly a mutating operation managed by interior mutability
    /// (mutex).
    pub fn batch_add_certificates(&self, der_certs: &[Vec<u8>]) -> Option<(usize, usize)> {
        match self.mode.write().deref_mut() {
            ClientCertVerifyMode::AllowAnyClient => None,
            ClientCertVerifyMode::MustVerifyClientCert(verifier) => Some(
                verifier
                    .roots
                    .add_parsable_certificates(der_certs),
            ),
        }
    }

    /// Empties the underlying `RootCertStore` and returns true if currently
    /// serving only authenticated clients. Otherwise, returns false and
    /// does nothing.
    ///
    /// Caveat: cached handshakes using a previously trusted client may be
    /// left intact.
    ///
    /// This is possibly a mutating operation managed by interior mutability
    /// (mutex).
    pub fn reset_root_cert_store(&self) -> bool {
        match self.mode.write().deref_mut() {
            ClientCertVerifyMode::AllowAnyClient => false,
            ClientCertVerifyMode::MustVerifyClientCert(verifier) => {
                verifier.roots = RootCertStore::empty();
                true
            }
        }
    }
}

impl ClientCertVerifier for SafeDefaultClientVerifier {
    fn offer_client_auth(&self) -> bool {
        match self.mode.read().deref() {
            ClientCertVerifyMode::AllowAnyClient => false,
            ClientCertVerifyMode::MustVerifyClientCert(_) => true,
        }
    }

    fn client_auth_mandatory(&self, _sni: Option<&webpki::DnsName>) -> Option<bool> {
        Some(self.offer_client_auth())
    }

    fn client_auth_root_subjects(
        &self,
        _sni: Option<&webpki::DnsName>,
    ) -> Option<DistinguishedNames> {
        match self.mode.read().deref() {
            ClientCertVerifyMode::AllowAnyClient => unimplemented!(),
            ClientCertVerifyMode::MustVerifyClientCert(strict_verifier) => {
                Some(strict_verifier.roots.subjects())
            }
        }
    }

    fn verify_client_cert(
        &self,
        end_entity: &Certificate,
        intermediates: &[Certificate],
        sni: Option<&webpki::DnsName>,
        now: SystemTime,
    ) -> Result<ClientCertVerified, Error> {
        match self.mode.read().deref() {
            ClientCertVerifyMode::AllowAnyClient => unimplemented!(),
            ClientCertVerifyMode::MustVerifyClientCert(strict_verifier) => {
                strict_verifier.verify_client_cert(end_entity, intermediates, sni, now)
            }
        }
    }
}

static ECDSA_SHA256: SignatureAlgorithms =
    &[&webpki::ECDSA_P256_SHA256, &webpki::ECDSA_P384_SHA256];

static ECDSA_SHA384: SignatureAlgorithms =
    &[&webpki::ECDSA_P256_SHA384, &webpki::ECDSA_P384_SHA384];

static ED25519: SignatureAlgorithms = &[&webpki::ED25519];

static RSA_SHA256: SignatureAlgorithms = &[&webpki::RSA_PKCS1_2048_8192_SHA256];
static RSA_SHA384: SignatureAlgorithms = &[&webpki::RSA_PKCS1_2048_8192_SHA384];
static RSA_SHA512: SignatureAlgorithms = &[&webpki::RSA_PKCS1_2048_8192_SHA512];
static RSA_PSS_SHA256: SignatureAlgorithms = &[&webpki::RSA_PSS_2048_8192_SHA256_LEGACY_KEY];
static RSA_PSS_SHA384: SignatureAlgorithms = &[&webpki::RSA_PSS_2048_8192_SHA384_LEGACY_KEY];
static RSA_PSS_SHA512: SignatureAlgorithms = &[&webpki::RSA_PSS_2048_8192_SHA512_LEGACY_KEY];

fn convert_scheme(scheme: SignatureScheme) -> Result<SignatureAlgorithms, Error> {
    match scheme {
        // nb. for TLS1.2 the curve is not fixed by SignatureScheme.
        SignatureScheme::ECDSA_NISTP256_SHA256 => Ok(ECDSA_SHA256),
        SignatureScheme::ECDSA_NISTP384_SHA384 => Ok(ECDSA_SHA384),

        SignatureScheme::ED25519 => Ok(ED25519),

        SignatureScheme::RSA_PKCS1_SHA256 => Ok(RSA_SHA256),
        SignatureScheme::RSA_PKCS1_SHA384 => Ok(RSA_SHA384),
        SignatureScheme::RSA_PKCS1_SHA512 => Ok(RSA_SHA512),

        SignatureScheme::RSA_PSS_SHA256 => Ok(RSA_PSS_SHA256),
        SignatureScheme::RSA_PSS_SHA384 => Ok(RSA_PSS_SHA384),
        SignatureScheme::RSA_PSS_SHA512 => Ok(RSA_PSS_SHA512),

        _ => {
            let error_msg = format!("received unadvertised sig scheme {:?}", scheme);
            Err(Error::PeerMisbehavedError(error_msg))
        }
    }
}

fn verify_sig_using_any_alg(
    cert: &webpki::EndEntityCert,
    algs: SignatureAlgorithms,
    message: &[u8],
    sig: &[u8],
) -> Result<(), webpki::Error> {
    // TLS doesn't itself give us enough info to map to a single webpki::SignatureAlgorithm.
    // Therefore, convert_algs maps to several and we try them all.
    for alg in algs {
        match cert.verify_signature(alg, message, sig) {
            Err(webpki::Error::UnsupportedSignatureAlgorithmForPublicKey) => continue,
            res => return res,
        }
    }

    Err(webpki::Error::UnsupportedSignatureAlgorithmForPublicKey)
}

fn verify_signed_struct(
    message: &[u8],
    cert: &Certificate,
    dss: &DigitallySignedStruct,
) -> Result<HandshakeSignatureValid, Error> {
    let possible_algs = convert_scheme(dss.scheme)?;
    let cert = webpki::EndEntityCert::try_from(cert.0.as_ref())
        .map_err(|e| Error::WebPkiError(e, WebPkiOp::ParseEndEntity))?;

    verify_sig_using_any_alg(&cert, possible_algs, message, &dss.sig.0)
        .map_err(|e| Error::WebPkiError(e, WebPkiOp::VerifySignature))
        .map(|_| HandshakeSignatureValid::assertion())
}

fn convert_alg_tls13(
    scheme: SignatureScheme,
) -> Result<&'static webpki::SignatureAlgorithm, Error> {
    use crate::msgs::enums::SignatureScheme::*;

    match scheme {
        ECDSA_NISTP256_SHA256 => Ok(&webpki::ECDSA_P256_SHA256),
        ECDSA_NISTP384_SHA384 => Ok(&webpki::ECDSA_P384_SHA384),
        ED25519 => Ok(&webpki::ED25519),
        RSA_PSS_SHA256 => Ok(&webpki::RSA_PSS_2048_8192_SHA256_LEGACY_KEY),
        RSA_PSS_SHA384 => Ok(&webpki::RSA_PSS_2048_8192_SHA384_LEGACY_KEY),
        RSA_PSS_SHA512 => Ok(&webpki::RSA_PSS_2048_8192_SHA512_LEGACY_KEY),
        _ => {
            let error_msg = format!("received unsupported sig scheme {:?}", scheme);
            Err(Error::PeerMisbehavedError(error_msg))
        }
    }
}

/// Constructs the signature message specified in section 4.4.3 of RFC8446.
pub fn construct_tls13_client_verify_message(handshake_hash: &Digest) -> Vec<u8> {
    construct_tls13_verify_message(handshake_hash, b"TLS 1.3, client CertificateVerify\x00")
}

/// Constructs the signature message specified in section 4.4.3 of RFC8446.
pub fn construct_tls13_server_verify_message(handshake_hash: &Digest) -> Vec<u8> {
    construct_tls13_verify_message(handshake_hash, b"TLS 1.3, server CertificateVerify\x00")
}

fn construct_tls13_verify_message(
    handshake_hash: &Digest,
    context_string_with_0: &[u8],
) -> Vec<u8> {
    let mut msg = Vec::new();
    msg.resize(64, 0x20u8);
    msg.extend_from_slice(context_string_with_0);
    msg.extend_from_slice(handshake_hash.as_ref());
    msg
}

fn verify_tls13(
    msg: &[u8],
    cert: &Certificate,
    dss: &DigitallySignedStruct,
) -> Result<HandshakeSignatureValid, Error> {
    let alg = convert_alg_tls13(dss.scheme)?;

    let cert = webpki::EndEntityCert::try_from(cert.0.as_ref())
        .map_err(|e| Error::WebPkiError(e, WebPkiOp::ParseEndEntity))?;

    cert.verify_signature(alg, &msg, &dss.sig.0)
        .map_err(|e| Error::WebPkiError(e, WebPkiOp::VerifySignature))
        .map(|_| HandshakeSignatureValid::assertion())
}

fn unix_time_millis(now: SystemTime) -> Result<u64, Error> {
    now.duration_since(std::time::UNIX_EPOCH)
        .map(|dur| dur.as_secs())
        .map_err(|_| Error::FailedToGetCurrentTime)
        .and_then(|secs| {
            secs.checked_mul(1000)
                .ok_or(Error::FailedToGetCurrentTime)
        })
}

fn verify_scts(
    cert: &Certificate,
    now: SystemTime,
    scts: &mut dyn Iterator<Item = &[u8]>,
    logs: &[&sct::Log],
) -> Result<(), Error> {
    if logs.is_empty() {
        return Ok(());
    }

    let now = unix_time_millis(now)?;
    let mut last_sct_error = None;
    for sct in scts {
        #[cfg_attr(not(feature = "logging"), allow(unused_variables))]
        match sct::verify_sct(&cert.0, sct, now, logs) {
            Ok(index) => {
                debug!(
                    "Valid SCT signed by {} on {}",
                    logs[index].operated_by, logs[index].description
                );
                return Ok(());
            }
            Err(e) => {
                if e.should_be_fatal() {
                    return Err(Error::InvalidSct(e));
                }
                debug!("SCT ignored because {:?}", e);
                last_sct_error = Some(e);
            }
        }
    }

    /* If we were supplied with some logs, and some SCTs,
     * but couldn't verify any of them, fail the handshake. */
    if let Some(last_sct_error) = last_sct_error {
        warn!("No valid SCTs provided");
        return Err(Error::InvalidSct(last_sct_error));
    }

    Ok(())
}
