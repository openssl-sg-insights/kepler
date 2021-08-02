use crate::config;
use crate::orbit::{create_orbit, load_orbit, verify_oid, AuthTokens, AuthTypes, Orbit};
use crate::zcap::ZCAPTokens;
use anyhow::Result;
use libipld::cid::Cid;
use rocket::{
    http::Status,
    request::{FromRequest, Outcome, Request},
};
use serde::{Deserialize, Serialize};
use ssi::did::DIDURL;
use std::str::FromStr;

pub mod cid_serde {
    use libipld::cid::{multibase::Base, Cid};
    use serde::{de::Error as SError, ser::Error as DError, Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(cid: &Cid, ser: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        ser.serialize_str(
            &cid.to_string_of_base(Base::Base58Btc)
                .map_err(S::Error::custom)?,
        )
    }

    pub fn deserialize<'de, D>(deser: D) -> Result<Cid, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s: &str = Deserialize::deserialize(deser)?;
        s.parse().map_err(D::Error::custom)
    }
}
pub mod vec_cid_serde {
    use libipld::cid::{
        multibase::{decode, Base},
        Cid,
    };
    use serde::{
        de::Error as SError, ser::Error as DError, ser::SerializeSeq, Deserialize, Deserializer,
        Serializer,
    };

    pub fn serialize<S>(vec: &Vec<Cid>, ser: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut seq = ser.serialize_seq(Some(vec.len()))?;
        for cid in vec {
            seq.serialize_element(
                &cid.to_string_of_base(Base::Base58Btc)
                    .map_err(S::Error::custom)?,
            )?;
        }
        seq.end()
    }

    pub fn deserialize<'de, D>(deser: D) -> Result<Vec<Cid>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s: Vec<&str> = Deserialize::deserialize(deser)?;
        s.iter()
            .map(|sc| {
                decode(sc).map_err(D::Error::custom).and_then(|(_, bytes)| {
                    Cid::read_bytes(bytes.as_slice()).map_err(D::Error::custom)
                })
            })
            .collect()
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum Action {
    Put {
        #[serde(with = "cid_serde")]
        orbit_id: Cid,
        #[serde(with = "vec_cid_serde")]
        content: Vec<Cid>,
    },
    Get {
        #[serde(with = "cid_serde")]
        orbit_id: Cid,
        #[serde(with = "vec_cid_serde")]
        content: Vec<Cid>,
    },
    Del {
        #[serde(with = "cid_serde")]
        orbit_id: Cid,
        #[serde(with = "vec_cid_serde")]
        content: Vec<Cid>,
    },
    Create {
        #[serde(with = "cid_serde")]
        orbit_id: Cid,
        parameters: String,
        #[serde(with = "vec_cid_serde")]
        content: Vec<Cid>,
    },
    List {
        #[serde(with = "cid_serde")]
        orbit_id: Cid,
    },
}

pub trait AuthorizationToken {
    fn action(&self) -> Action;
}

#[rocket::async_trait]
pub trait AuthorizationPolicy {
    type Token: AuthorizationToken;
    async fn authorize<'a>(&self, auth_token: &'a Self::Token) -> Result<()>;
}

pub struct PutAuthWrapper(pub Orbit);
pub struct GetAuthWrapper(pub Orbit);
pub struct DelAuthWrapper(pub Orbit);
pub struct CreateAuthWrapper(pub Orbit);
pub struct ListAuthWrapper(pub Orbit);

async fn extract_info<T>(
    req: &Request<'_>,
) -> Result<(Vec<u8>, AuthTokens, config::Config), Outcome<T, anyhow::Error>> {
    // TODO need to identify auth method from the headers
    let auth_data = match req.headers().get_one("Authorization") {
        Some(a) => a,
        None => {
            return Err(Outcome::Forward(()));
        }
    };
    info_!("Headers: {}", auth_data);
    let config = match req.rocket().state::<config::Config>() {
        Some(c) => c,
        None => {
            return Err(Outcome::Failure((
                Status::InternalServerError,
                anyhow!("Could not retrieve config"),
            )));
        }
    };
    match AuthTokens::from_request(req).await {
        Outcome::Success(token) => Ok((auth_data.as_bytes().to_vec(), token, config.clone())),
        Outcome::Failure(e) => Err(Outcome::Failure(e)),
        Outcome::Forward(_) => Err(Outcome::Failure((
            Status::Unauthorized,
            anyhow!("No valid authorization headers"),
        ))),
    }
}

// TODO some APIs prefer to return 404 when the authentication fails to avoid leaking information about content

macro_rules! impl_fromreq {
    ($type:ident, $method:tt) => {
        #[rocket::async_trait]
        impl<'r> FromRequest<'r> for $type {
            type Error = anyhow::Error;

            async fn from_request(req: &'r Request<'_>) -> Outcome<Self, Self::Error> {
                let (_, token, config) = match extract_info(req).await {
                    Ok(i) => i,
                    Err(o) => return o,
                };
                match token.action() {
                    Action::$method { orbit_id, .. } => {
                        let orbit = match load_orbit(orbit_id, config.database.path.clone()).await {
                            Ok(Some(o)) => o,
                            Ok(None) => {
                                return Outcome::Failure((
                                    Status::NotFound,
                                    anyhow!("No Orbit found"),
                                ))
                            }
                            Err(e) => return Outcome::Failure((Status::InternalServerError, e)),
                        };
                        match orbit.auth().authorize(token).await {
                            Ok(_) => Outcome::Success(Self(orbit)),
                            Err(e) => Outcome::Failure((Status::Unauthorized, e)),
                        }
                    }
                    _ => Outcome::Failure((
                        Status::BadRequest,
                        anyhow!("Token action not matching endpoint"),
                    )),
                }
            }
        }
    };
}

impl_fromreq!(PutAuthWrapper, Put);
impl_fromreq!(GetAuthWrapper, Get);
impl_fromreq!(DelAuthWrapper, Del);
impl_fromreq!(ListAuthWrapper, List);

#[rocket::async_trait]
impl<'r> FromRequest<'r> for CreateAuthWrapper {
    type Error = anyhow::Error;

    async fn from_request(req: &'r Request<'_>) -> Outcome<Self, Self::Error> {
        let (auth_data, token, config) = match extract_info(req).await {
            Ok(i) => i,
            Err(o) => return o,
        };
        // TODO remove clone, or refactor the order of validations/actions
        match &token.action() {
            // Create actions dont have an existing orbit to authorize against, it's a node policy
            // TODO have policy config, for now just be very permissive :shrug:
            Action::Create {
                orbit_id,
                parameters,
                ..
            } => {
                let (method, params) = match verify_oid(&orbit_id, &parameters) {
                    Ok(r) => r,
                    _ => {
                        return Outcome::Failure((
                            Status::BadRequest,
                            anyhow!("Incorrect Orbit ID"),
                        ))
                    }
                };
                let controllers = match &token {
                    AuthTokens::Tezos(token_tz) => {
                        match method {
                            "tz" => {}
                            _ => {
                                return Outcome::Failure((
                                    Status::BadRequest,
                                    anyhow!("Incorrect Orbit ID"),
                                ))
                            }
                        };
                        if params.get("address") != Some(&token_tz.pkh.as_str()) {
                            return Outcome::Failure((
                                Status::Unauthorized,
                                anyhow!("Incorrect PKH param"),
                            ));
                        };
                        let vm = DIDURL {
                            did: format!("did:pkh:tz:{}", &token_tz.pkh),
                            fragment: Some("TezosMethod2021".to_string()),
                            ..Default::default()
                        };
                        vec![vm]
                    }
                    AuthTokens::ZCAP(ZCAPTokens { invocation, .. }) => {
                        let vm = match invocation.proof.as_ref().and_then(|p| {
                            p.verification_method.as_ref().map(|v| DIDURL::from_str(&v))
                        }) {
                            Some(Ok(v)) => v,
                            _ => {
                                return Outcome::Failure((
                                    Status::Unauthorized,
                                    anyhow!("Invalid Delegation Verification Method"),
                                ))
                            }
                        };
                        vec![vm]
                    }
                    _ => {
                        return Outcome::Failure((
                            Status::Unauthorized,
                            anyhow!("Missing Authorization"),
                        ))
                    }
                };
                match create_orbit(
                    *orbit_id,
                    config.database.path.clone(),
                    controllers,
                    &auth_data,
                    AuthTypes::ZCAP,
                )
                .await
                {
                    Ok(Some(orbit)) => Outcome::Success(Self(orbit)),
                    Ok(None) => {
                        return Outcome::Failure((
                            Status::Conflict,
                            anyhow!("Orbit already exists"),
                        ))
                    }
                    Err(e) => Outcome::Failure((Status::InternalServerError, e)),
                }
            }
            _ => Outcome::Failure((
                Status::BadRequest,
                anyhow!("Token action not matching endpoint"),
            )),
        }
    }
}
