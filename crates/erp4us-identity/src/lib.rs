use axum::http::{HeaderMap, header};
use jsonwebtoken::{
    Algorithm, DecodingKey, Validation, decode, decode_header,
    jwk::{AlgorithmParameters, JwkSet, KeyAlgorithm, PublicKeyUse},
};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, fs::File, io, sync::Arc};
use uuid::Uuid;

#[derive(Clone)]
pub struct Auth(Arc<Policy>);

struct Policy {
    keys: HashMap<String, DecodingKey>,
    validation: Validation,
    principals: HashMap<String, Principal>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    issuer: String,
    audience: String,
    principals: HashMap<String, Principal>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Principal {
    actor_id: Uuid,
    grants: Vec<Grant>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    platform_permissions: Vec<String>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Grant {
    tenant_id: Uuid,
    organization_id: Uuid,
    permissions: Vec<String>,
}

/// An actor and scope established by a verified access token and an operator grant.
#[derive(Clone, Copy, Debug)]
pub struct Scope {
    pub tenant: Uuid,
    pub organization: Uuid,
    pub actor: Uuid,
}

#[derive(Deserialize)]
struct Claims {
    sub: String,
    #[serde(rename = "typ")]
    kind: Option<String>,
}

pub enum Denial {
    Unauthenticated,
    InvalidScope,
    Forbidden,
}

pub const TENANT_HEADER: &str = "x-erp4us-tenant";
pub const ORGANIZATION_HEADER: &str = "x-erp4us-organization";

impl Auth {
    pub fn load(config_path: &str, jwks_path: &str) -> io::Result<Self> {
        let config: Config = read_json(config_path, "Invalid identity policy")?;
        let jwks: JwkSet = read_json(jwks_path, "Invalid issuer JWKS")?;
        if !config.issuer.starts_with("https://")
            || config.issuer.len() <= "https://".len()
            || config.audience.trim().is_empty()
            || config.principals.keys().any(|subject| subject.is_empty())
        {
            return Err(io::Error::other(
                "Invalid issuer, audience or subject configuration",
            ));
        }
        let mut keys = HashMap::new();
        for jwk in jwks.keys {
            if !matches!(jwk.algorithm, AlgorithmParameters::RSA(_))
                || !matches!(jwk.common.key_algorithm, None | Some(KeyAlgorithm::RS256))
                || !matches!(
                    jwk.common.public_key_use,
                    None | Some(PublicKeyUse::Signature)
                )
                || jwk
                    .common
                    .key_operations
                    .as_ref()
                    .is_some_and(|ops| !ops.contains(&jsonwebtoken::jwk::KeyOperations::Verify))
            {
                continue;
            }
            let kid = jwk
                .common
                .key_id
                .as_deref()
                .filter(|id| !id.is_empty())
                .ok_or_else(|| io::Error::other("An issuer signing key has no key ID"))?;
            let key = DecodingKey::from_jwk(&jwk)
                .map_err(|_| io::Error::other("Invalid issuer signing key"))?;
            if keys.insert(kid.to_owned(), key).is_some() {
                return Err(io::Error::other("Duplicate issuer signing key ID"));
            }
        }
        if keys.is_empty() {
            return Err(io::Error::other("No RS256 issuer signing keys"));
        }
        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_issuer(&[config.issuer]);
        validation.set_audience(&[config.audience]);
        validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
        validation.validate_nbf = true;
        validation.leeway = 0;
        Ok(Self(Arc::new(Policy {
            keys,
            validation,
            principals: config.principals,
        })))
    }

    pub fn authenticate(&self, headers: &HeaderMap) -> Result<&Principal, Denial> {
        let (scheme, token) = one_header(headers, header::AUTHORIZATION.as_str())
            .and_then(|value| value.split_once(' '))
            .ok_or(Denial::Unauthenticated)?;
        if !scheme.eq_ignore_ascii_case("bearer") || token.len() > 8192 {
            return Err(Denial::Unauthenticated);
        }
        let header = decode_header(token).map_err(|_| Denial::Unauthenticated)?;
        let key = header
            .kid
            .as_ref()
            .and_then(|id| self.0.keys.get(id))
            .ok_or(Denial::Unauthenticated)?;
        let claims = decode::<Claims>(token, key, &self.0.validation)
            .map_err(|_| Denial::Unauthenticated)?
            .claims;
        let access_token = match header.typ.as_deref() {
            Some("at+jwt" | "application/at+jwt") => true,
            Some("JWT") => claims.kind.as_deref() == Some("Bearer"),
            _ => false,
        };
        if !access_token || claims.sub.is_empty() {
            return Err(Denial::Unauthenticated);
        }
        self.0.principals.get(&claims.sub).ok_or(Denial::Forbidden)
    }

    pub fn authorize(&self, headers: &HeaderMap, permission: &str) -> Result<Scope, Denial> {
        let principal = self.authenticate(headers)?;
        let tenant = scope_header(headers, TENANT_HEADER)?;
        let organization = scope_header(headers, ORGANIZATION_HEADER)?;
        if !principal.grants.iter().any(|grant| {
            grant.tenant_id == tenant
                && grant.organization_id == organization
                && grant
                    .permissions
                    .iter()
                    .any(|allowed| allowed == permission)
        }) {
            return Err(Denial::Forbidden);
        }
        Ok(Scope {
            tenant,
            organization,
            actor: principal.actor_id,
        })
    }

    pub fn authorize_platform(&self, headers: &HeaderMap, permission: &str) -> Result<(), Denial> {
        let principal = self.authenticate(headers)?;
        principal
            .platform_permissions
            .iter()
            .any(|grant| grant == permission)
            .then_some(())
            .ok_or(Denial::Forbidden)
    }
}

fn read_json<T: serde::de::DeserializeOwned>(path: &str, message: &'static str) -> io::Result<T> {
    let file = File::open(path).map_err(|_| io::Error::other(message))?;
    serde_json::from_reader(io::BufReader::new(file)).map_err(|_| io::Error::other(message))
}

fn one_header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let mut values = headers.get_all(name).iter();
    let value = values.next()?.to_str().ok()?;
    values.next().is_none().then_some(value)
}

fn scope_header(headers: &HeaderMap, name: &str) -> Result<Uuid, Denial> {
    one_header(headers, name)
        .and_then(|value| Uuid::try_parse(value).ok())
        .ok_or(Denial::InvalidScope)
}
