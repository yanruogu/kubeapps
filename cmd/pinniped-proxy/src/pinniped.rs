use std::env;
use std::convert::TryFrom;

use anyhow::{Context, Result};
use k8s_openapi::api::core::v1 as corev1;
use k8s_openapi::apimachinery::pkg::apis::meta::v1 as metav1;
use k8s_openapi::Metadata;
use kube::{
    api::{Api, PostParams},
    Client, Config,
    Service,
};
use kube_derive::CustomResource;
use log::debug;
use native_tls::Identity;
use openssl::{pkcs12::Pkcs12, pkey::PKey, x509::X509};
use serde::{Deserialize, Serialize};
use serde_json;
use thiserror::Error;
use url::Url;

const DEFAULT_PINNIPED_NAMESPACE: &str = "DEFAULT_PINNIPED_NAMESPACE";
const DEFAULT_PINNIPED_AUTHENTICATOR_NAME: &str = "DEFAULT_PINNIPED_AUTHENTICATOR_NAME";
const DEFAULT_PINNIPED_AUTHENTICATOR_TYPE: &str = "DEFAULT_PINNIPED_AUTHENTICATOR_TYPE";

#[derive(Error, Debug)]
pub enum PinnipedError {
    #[error("Unauthorized by pinniped: {0}")]
    UnsuccessfulAuthentication(String),
}

/// exchange_token_for_identity accepts an authorization header and returns a client cert authentication Identity in exchange.
///
/// The token is exchanged with pinniped concierge API running on the identified kubernetes api server.
pub async fn exchange_token_for_identity(authorization: &str, k8s_api_server_url: &str, k8s_api_ca_cert_data: &[u8]) -> Result<Identity> {
    let credential_request = call_pinniped_exchange(authorization, k8s_api_server_url, k8s_api_ca_cert_data).await.context("Failed to exchange credentials")?;
    match credential_request.status {
        Some(s) => {
            match s.credential {
                Some(c) => return identity_for_exchange(&c),
                None => match s.message {
                    // A returned status without a credential is unsuccessful authentication so
                    // add context to identify this.
                    Some(m) => return Err(anyhow::anyhow!(m.clone()).context(PinnipedError::UnsuccessfulAuthentication(m))),
                    None => return Err(anyhow::anyhow!("response status neither an error msg or a credential: {:#?}", s)),
                }
            }
        },
        None => return Err(anyhow::anyhow!("pinniped credential request did not include status: {:#?}", credential_request))
    }
}

/// identity_for_exchange parses the JSON output of the credential exchange and returns the Identity.
///
/// Note: to create an identity, need to go via a pkcs12 currently.
/// https://github.com/sfackler/rust-native-tls/issues/27#issuecomment-324262673
fn identity_for_exchange(cred: &ClusterCredential) -> Result<Identity> {
    let pkey = PKey::private_key_from_pem(cred.client_key_data.as_bytes())
        .context("error creating private key from pem")?;
    let x509 = X509::from_pem(cred.client_certificate_data.as_bytes())
        .context("error creating x509 from pem")?;

    let pkcs_cert = Pkcs12::builder()
        .build("", "friendly-name", &pkey, &x509)
        .context("Error building Pkcs12 from private key and x509")?;
    let identity = Identity::from_pkcs12(
        &pkcs_cert.to_der().context("error creating der from pkcs12")?,
        "",
    ).context("error creating identity from der-formatted pkcs12")?;
    Ok(identity)
}

/// TokenCredentialRequestSpec
///
/// TokenCredentialRequestSpec and the Status including the returned cluster credential
/// are structs based on the corresponding structs in the pinniped code at:
/// https://github.com/vmware-tanzu/pinniped/blob/main/generated/1.19/apis/concierge/login/v1alpha1/types_token.go#L11
///
/// The rust derive macro together with the kube macro creates serializable and deserializable
/// resources based on the struct. See https://docs.rs/kube/0.43.0/kube/ for more details.
#[derive(CustomResource, Deserialize, Serialize, Clone, Debug)]
#[kube(group = "login.concierge.pinniped.dev", version = "v1alpha1", kind = "TokenCredentialRequest", namespaced)]
#[kube(status = "TokenCredentialRequestStatus")]
pub struct TokenCredentialRequestSpec {
    // Bearer token supplied with the credential request.
    token: Option<String>,

    // Reference to an authenticator which can verify this credential request.
    authenticator: corev1::TypedLocalObjectReference,
}

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct TokenCredentialRequestStatus {
    // A ClusterCredential will be returned for a successful credential request.
    credential: Option<ClusterCredential>,

    // An error message will be returned for an unsuccessful credential request.
    message: Option<String>,
}

#[derive(Deserialize, Serialize, Clone, Debug)]
/// ClusterCredential is the cluster-specific credential returned on a successful credential request. It
/// contains either a valid bearer token or a valid TLS certificate and corresponding private key for the cluster.
#[serde(rename_all = "camelCase")]
pub struct ClusterCredential {
    // ExpirationTimestamp indicates a time when the provided credentials expire.
    expiration_timestamp: metav1::Time,

    // Token is a bearer token used by the client for request authentication.
    token: Option<String>,

    // PEM-encoded client TLS certificates (including intermediates, if any).
    client_certificate_data: String,

    // PEM-encoded private key for the above certificate.
    client_key_data: String,
}

/// call_pinniped_exchange returns the resulting TokenCredentialRequest with Status after requesting a token credential exchange.
async fn call_pinniped_exchange(authorization: &str, k8s_api_server_url: &str, k8s_api_ca_cert_data: &[u8]) -> Result<TokenCredentialRequest> {
    let pinniped_namespace = env::var(DEFAULT_PINNIPED_NAMESPACE)?;

    let mut config = Config::new(Url::parse(k8s_api_server_url).context("Failed parsing url for exchange")?);
    config.default_ns = pinniped_namespace.clone();
    let x509 = X509::from_pem(k8s_api_ca_cert_data).context("error creating x509 from pem")?;
    let der = x509.to_der().context("error creating der from x509")?;
    config.root_cert = Some(vec!(der));
    let client = Client::new(Service::try_from(config)?);

    let auth_token = match authorization.to_string().strip_prefix("Bearer ") {
        Some(a) => a.to_string(),
        None => authorization.to_string(),
    };
    let token_creds: Api<TokenCredentialRequest> = Api::namespaced(client.clone(), &pinniped_namespace);
    let mut cred_request = TokenCredentialRequest::new("", TokenCredentialRequestSpec {
        token: Some(auth_token),
        authenticator: corev1::TypedLocalObjectReference {
            name: env::var(DEFAULT_PINNIPED_AUTHENTICATOR_NAME).with_context(|| format!("error retrieving {}", DEFAULT_PINNIPED_AUTHENTICATOR_NAME))?,
            kind: env::var(DEFAULT_PINNIPED_AUTHENTICATOR_TYPE).with_context(|| format!("error retrieving {}", DEFAULT_PINNIPED_AUTHENTICATOR_TYPE))?,
            api_group: Some("authentication.concierge.pinniped.dev".into()),
        },
    });
    // The pinniped authenticator cache requires the namespace of the request to be included
    // explicitly, even if the client is limited to a specific namespace.
    cred_request.metadata_mut().namespace = Some(pinniped_namespace);

    debug!("{}", serde_json::to_string(&cred_request).unwrap());
    match token_creds.create(&PostParams::default(), &cred_request).await {
        Ok(o) => Ok(o),
        Err(e) => {
            Err(anyhow::anyhow!("err creating token exchange: {:#?}\n{}", serde_json::to_string(&cred_request).unwrap(), e))
        },
    }
}

#[macro_use]
#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    const VALID_CERT_BASE64: &'static str = "LS0tLS1CRUdJTiBDRVJUSUZJQ0FURS0tLS0tCk1JSUN5RENDQWJDZ0F3SUJBZ0lCQURBTkJna3Foa2lHOXcwQkFRc0ZBREFWTVJNd0VRWURWUVFERXdwcmRXSmwKY201bGRHVnpNQjRYRFRJd01UQXlOakl6TXpBME5Wb1hEVE13TVRBeU5ESXpNekEwTlZvd0ZURVRNQkVHQTFVRQpBeE1LYTNWaVpYSnVaWFJsY3pDQ0FTSXdEUVlKS29aSWh2Y05BUUVCQlFBRGdnRVBBRENDQVFvQ2dnRUJBT1ZKCnFuOVBFZUp3UDRQYnI0cFo1ZjZKUmliOFZ5a2tOYjV2K1hzTVZER01aWGZLb293Y29IYjFwRWh5d0pzeDFiME4Kd2YvZ1JURi9maEgzT0drRnNQMlV2a0lHVytzNUlBd0sxMFRXYkN5VzAwT3lzVkdLcnl5bHNWcEhCWXBZRGJBcQpkdnQzc0FkcFJZaGlLZSs2NkVTL3dQNTdLV3g0SVdwZko0UGpyejh2NkJBWlptZ3o5ZzRCSFNMQkhpbTVFbTdYClBJTmpKL1RJTXFzVW1PR1ppUUNHR0ptRnQxZ21jQTd3eHZ0ZXg2ckkxSWdFNkh5NW10UzJ3NDZaMCtlVU1RSzgKSE9UdnI5aGFETnhJenVjbkduaFlCT2Z2U2VVaXNCR0pOUm5QbENydWx4b2NSZGI3N20rQUdzWW52QitNd2prVQpEbXNQTWZBelpSRHEwekhzcGEwQ0F3RUFBYU1qTUNFd0RnWURWUjBQQVFIL0JBUURBZ0trTUE4R0ExVWRFd0VCCi93UUZNQU1CQWY4d0RRWUpLb1pJaHZjTkFRRUxCUUFEZ2dFQkFBWndybXJLa3FVaDJUYld2VHdwSWlOd0o1NzAKaU9lTVl2WWhNakZxTmt6Tk9OUW55c3lPd1laRGJFMDRrV3AxclRLNHVZaUh3NTJUc0cyelJsZ0QzMzNKaEtvUQpIVloyV1hUT3Z5U2RJaWl5bVpKM2N3d0p2T0lhMW5zZnhYY1NJakJnYnNzYXowMndpRCtlazRPdmlRZktjcXJpCnFQbWZabDZDSkk0NU1rd3JwTExFaTZkNVhGbkhDb3d4eklxQjBrUDhwOFlOaGJYWTNYY2JaNElvY2lMemRBamUKQ1l6NXFVSlBlSDJCcHNaM0JXNXRDbjcycGZYazVQUjlYOFRUTHh6aTA4SU9yYjgvRDB4Tnk3emQyMnVjNXM1bwoveXZIeEt6cXBiczVuRXJkT0JFVXNGWnBpUEhaVGc1dExmWlZ4TG00VjNTZzQwRWUyNFd6d09zaDNIOD0KLS0tLS1FTkQgQ0VSVElGSUNBVEUtLS0tLQo=";

    // By default cargo will run rust unit tests in parallel. The serial macro ensures that a specific test
    // (or group of tests) runs serially.
    #[test]
    #[serial(envtest)]
    fn test_call_pinniped_exchange_no_env() -> Result<()> {
        env::remove_var(DEFAULT_PINNIPED_NAMESPACE);
        match tokio_test::block_on(call_pinniped_exchange("authorization", "https://example.com", VALID_CERT_BASE64.as_bytes())) {
            Ok(_) => anyhow::bail!("expected error"),
            Err(e) => {
                assert!(e.is::<env::VarError>(), "got: {:#?}, want: {}", e, env::VarError::NotPresent);
                Ok(())
            },
        }
    }

    #[test]
    #[serial(envtest)]
    fn test_call_pinniped_exchange_bad_url() -> Result<()> {
        env::set_var(DEFAULT_PINNIPED_NAMESPACE, "pinniped-concierge");
        match tokio_test::block_on(call_pinniped_exchange("authorization", "not a url", VALID_CERT_BASE64.as_bytes())) {
            Ok(_) => anyhow::bail!("expected error"),
            Err(e) => {
                assert!(e.is::<url::ParseError>(), "got: {:#?}, want: {}", e, url::ParseError::InvalidDomainCharacter);
                Ok(())
            },
        }
    }

    #[test]
    #[serial(envtest)]
    fn test_call_pinniped_exchange_bad_cert() -> Result<()> {
        env::set_var(DEFAULT_PINNIPED_NAMESPACE, "pinniped-concierge");
        match tokio_test::block_on(call_pinniped_exchange("authorization", "https://example.com", "not a cert".as_bytes())) {
            Ok(_) => anyhow::bail!("expected error"),
            Err(e) => {
                assert!(e.is::<openssl::error::ErrorStack>(), "got: {:#?}, want: openssl::error::ErrorStack", e);
                Ok(())
            },
        }
    }
}
