//! Registry credentials shared by the OCI store and lock client.
//!
//! A vk-registry server accepts no authentication, HTTP Basic, or static bearer
//! tokens for its `/v2/` and `/lock/` APIs.

use oci_client::secrets::RegistryAuth;

#[derive(Clone)]
pub(crate) enum Auth {
    /// No credentials (loopback / trusted network).
    None,
    /// HTTP Basic.
    Basic { user: String, pass: String },
    /// Static bearer token.
    Bearer { token: String },
}

impl Auth {
    /// Attach the credential to a raw `reqwest` request.
    pub(crate) fn apply(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match self {
            Auth::None => req,
            Auth::Basic { user, pass } => req.basic_auth(user, Some(pass)),
            Auth::Bearer { token } => req.bearer_auth(token),
        }
    }

    /// Convert the credential for `oci-client` manifest and blob requests.
    pub(crate) fn registry_auth(&self) -> RegistryAuth {
        match self {
            Auth::None => RegistryAuth::Anonymous,
            Auth::Basic { user, pass } => RegistryAuth::Basic(user.clone(), pass.clone()),
            Auth::Bearer { token } => RegistryAuth::Bearer(token.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_auth_maps_each_scheme() {
        assert!(matches!(
            Auth::None.registry_auth(),
            RegistryAuth::Anonymous
        ));
        let basic = Auth::Basic {
            user: "u".to_string(),
            pass: "p".to_string(),
        };
        assert!(matches!(
            basic.registry_auth(),
            RegistryAuth::Basic(u, p) if u == "u" && p == "p"
        ));
        let bearer = Auth::Bearer {
            token: "t".to_string(),
        };
        assert!(matches!(bearer.registry_auth(), RegistryAuth::Bearer(t) if t == "t"));
    }
}
