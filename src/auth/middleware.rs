use actix_web::{
    dev::{forward_ready, Service, ServiceRequest, ServiceResponse, Transform},
    Error, HttpMessage,
};
use futures_util::future::LocalBoxFuture;
use jsonwebtoken::{decode, DecodingKey, Validation};
use std::future::{ready, Ready};
use crate::config::CONFIG;
use crate::auth::claims::Claims;
use crate::auth::permissions::AuthContext;

/// Validation rules, derived from what FalconGo actually emits.
///
///   * Algorithm is PINNED to HS256 rather than read from the token header.
///     Reading it from the header is how `alg: none` and HMAC/RSA confusion
///     attacks work. `Validation::default()` happens to be HS256, but relying on
///     a default for a security property is how it silently changes later.
///   * `exp` IS validated: FalconGo sets it on every token (31d admin, 365d
///     driver).
///   * `iss` is NOT validated. FalconGo sets it to the user's own id as a
///     string, so it differs per user -- pinning it would reject every token.
///   * `aud` is NOT validated: FalconGo never emits it, and requiring it would
///     reject every token.
///   * 60s leeway absorbs clock skew between the two services.
fn validation() -> Validation {
    let mut v = Validation::new(jsonwebtoken::Algorithm::HS256);
    v.validate_exp = true;
    v.leeway = 60;
    v.validate_aud = false;
    v
}



pub struct JwtAuth {
    pub required_permission: Option<i32>,
}

impl<S, B> Transform<S, ServiceRequest> for JwtAuth
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error>,
    S::Future: 'static,
    B: 'static,
{
    type Response = ServiceResponse<B>;
    type Error = Error;
    type InitError = ();
    type Transform = JwtAuthMiddleware<S>;
    type Future = Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        ready(Ok(JwtAuthMiddleware {
            service,
            required_permission: self.required_permission,
        }))
    }
}

pub struct JwtAuthMiddleware<S> {
    service: S,
    required_permission: Option<i32>,
}

impl<S, B> Service<ServiceRequest> for JwtAuthMiddleware<S>
where
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error>,
    S::Future: 'static,
    B: 'static,
{
    type Response = ServiceResponse<B>;
    type Error = Error;
    type Future = LocalBoxFuture<'static, Result<Self::Response, Self::Error>>;

    forward_ready!(service);

    fn call(&self, req: ServiceRequest) -> Self::Future {
        // Extract token from cookie or Authorization header
        let token = req
            .cookie("jwt")
            .map(|c| c.value().to_string())
            .or_else(|| {
                req.headers()
                    .get("Authorization")
                    .and_then(|h| h.to_str().ok())
                    .and_then(|h| {
                        if h.starts_with("Bearer ") {
                            Some(h[7..].to_string())
                        } else {
                            None
                        }
                    })
            });

        if token.is_none() {
            return Box::pin(async move {
                Err(actix_web::error::ErrorUnauthorized("Not authenticated"))
            });
        }

        let token = token.unwrap();
        
        let token_data = decode::<Claims>(
            &token,
            &DecodingKey::from_secret(CONFIG.jwt_secret.as_bytes()),
            &validation(),
        );

        match token_data {
            Ok(data) => {
                let claims = data.claims;

                // Only check if user is an admin (don't check permission level here)
                // Handler will determine what data to show based on permission level
                if !claims.is_admin() {
                    return Box::pin(async move {
                        Err(actix_web::error::ErrorForbidden(
                            "Admin access required"
                        ))
                    });
                }

                // Resolved authorization for the bank-SMS handlers, which read
                // AuthContext and never touch Claims. Existing handlers keep
                // reading Claims, so nothing changes for them.
                if let Some(ctx) = AuthContext::from_claims(&claims) {
                    req.extensions_mut().insert(ctx);
                }

                // Insert claims into request extensions for handlers to access
                req.extensions_mut().insert(claims);

                // Call the next service
                let fut = self.service.call(req);
                Box::pin(async move {
                    let res = fut.await?;
                    Ok(res)
                })
            }
            Err(err) => Box::pin(async move {
                crate::ops::metrics::incr(&crate::ops::metrics::AUTH_FAILURES, 1);
                Err(actix_web::error::ErrorUnauthorized(
                    format!("Invalid token: {}", err)
                ))
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{encode, EncodingKey, Header};
    use serde::Serialize;

    const SECRET: &str = "test-secret-matching-falcongo";

    /// Mirrors FalconGo's `utils.CustomClaims` exactly, including the fact that
    /// there is NO `sub` claim and that `iss` is the user id as a *string*.
    #[derive(Serialize)]
    struct FalconGoClaims {
        user_type: &'static str,
        user_id: u32,
        driver_id: u32,
        permission: i32,
        iss: String,
        exp: i64,
    }

    fn falcongo_token(permission: i32, exp_offset_secs: i64) -> String {
        let now = chrono::Utc::now().timestamp();
        let claims = FalconGoClaims {
            user_type: "admin_user",
            user_id: 7,
            driver_id: 0,
            permission,
            iss: "7".to_string(),
            exp: now + exp_offset_secs,
        };
        encode(
            &Header::new(jsonwebtoken::Algorithm::HS256),
            &claims,
            &EncodingKey::from_secret(SECRET.as_bytes()),
        )
        .unwrap()
    }

    fn decode_with(token: &str, secret: &str) -> Result<Claims, jsonwebtoken::errors::Error> {
        jsonwebtoken::decode::<Claims>(
            token,
            &DecodingKey::from_secret(secret.as_bytes()),
            &validation(),
        )
        .map(|d| d.claims)
    }

    #[test]
    fn real_falcongo_token_verifies() {
        let token = falcongo_token(4, 3600);
        let claims = decode_with(&token, SECRET).expect("valid FalconGo token must verify");
        assert_eq!(claims.user_type, "admin_user");
        assert_eq!(claims.user_id, Some(7));
        assert_eq!(claims.permission, Some(4));
    }

    #[test]
    fn tampered_signature_is_rejected() {
        let token = falcongo_token(4, 3600);
        // Flip the last character of the signature.
        let mut chars: Vec<char> = token.chars().collect();
        let last = chars.len() - 1;
        chars[last] = if chars[last] == 'A' { 'B' } else { 'A' };
        let tampered: String = chars.into_iter().collect();

        assert!(
            decode_with(&tampered, SECRET).is_err(),
            "a tampered signature must not verify"
        );
    }

    #[test]
    fn tampered_payload_is_rejected() {
        // The realistic attack: re-encode the payload with permission escalated
        // to 4 while keeping the original signature.
        let token = falcongo_token(1, 3600);
        let parts: Vec<&str> = token.split('.').collect();
        let forged_payload = {
            use base64::Engine;
            let claims = serde_json::json!({
                "user_type": "admin_user", "user_id": 7, "driver_id": 0,
                "permission": 4, "iss": "7",
                "exp": chrono::Utc::now().timestamp() + 3600
            });
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims.to_string())
        };
        let forged = format!("{}.{}.{}", parts[0], forged_payload, parts[2]);

        assert!(
            decode_with(&forged, SECRET).is_err(),
            "a re-signed payload must not verify"
        );
    }

    #[test]
    fn wrong_secret_is_rejected() {
        let token = falcongo_token(4, 3600);
        assert!(decode_with(&token, "not-the-falcongo-secret").is_err());
    }

    #[test]
    fn expired_token_is_rejected() {
        // Well past the 60s leeway.
        let token = falcongo_token(4, -3600);
        assert!(decode_with(&token, SECRET).is_err());
    }

    #[test]
    fn clock_skew_within_leeway_is_accepted() {
        // Expired 30s ago: inside the 60s leeway, so still valid. This is the
        // case that would otherwise flap when the two services' clocks drift.
        let token = falcongo_token(4, -30);
        assert!(decode_with(&token, SECRET).is_ok());
    }

    #[test]
    fn algorithm_is_pinned_to_hs256() {
        // `alg: none` is the classic bypass: strip the signature and claim the
        // token is unsigned. Pinning the algorithm is what makes this fail.
        let header = r#"{"alg":"none","typ":"JWT"}"#;
        let payload = r#"{"user_type":"admin_user","user_id":7,"permission":4,"exp":9999999999}"#;
        let unsigned = {
            use base64::Engine;
            let e = base64::engine::general_purpose::URL_SAFE_NO_PAD;
            format!("{}.{}.", e.encode(header), e.encode(payload))
        };

        assert!(
            decode_with(&unsigned, SECRET).is_err(),
            "alg:none must be rejected"
        );
    }

    #[test]
    fn driver_token_yields_no_auth_context() {
        // A driver token is correctly signed and structurally valid -- it must
        // still produce no AuthContext for the bank-SMS endpoints.
        #[derive(Serialize)]
        struct DriverClaims {
            user_type: &'static str,
            user_id: u32,
            driver_id: u32,
            iss: String,
            exp: i64,
        }
        let token = encode(
            &Header::new(jsonwebtoken::Algorithm::HS256),
            &DriverClaims {
                user_type: "driver",
                user_id: 0,
                driver_id: 12,
                iss: "12".to_string(),
                exp: chrono::Utc::now().timestamp() + 3600,
            },
            &EncodingKey::from_secret(SECRET.as_bytes()),
        )
        .unwrap();

        let claims = decode_with(&token, SECRET).expect("driver token is still a valid token");
        assert!(AuthContext::from_claims(&claims).is_none());
    }

    /// FalconGo emits no `sub`. If a future change made us depend on one, this
    /// test documents the current contract.
    #[test]
    fn absence_of_sub_claim_is_fine() {
        let token = falcongo_token(3, 3600);
        let claims = decode_with(&token, SECRET).unwrap();
        // Identity comes from user_id, not sub.
        assert_eq!(claims.user_id, Some(7));
    }
}