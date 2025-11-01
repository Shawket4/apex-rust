use actix_web::{
    dev::{forward_ready, Service, ServiceRequest, ServiceResponse, Transform},
    Error, HttpMessage,
};
use futures_util::future::LocalBoxFuture;
use jsonwebtoken::{decode, DecodingKey, Validation};
use std::future::{ready, Ready};
use crate::config::CONFIG;
use crate::auth::claims::Claims;



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
        
        // Decode and validate JWT
        let validation = Validation::default();
        let token_data = decode::<Claims>(
            &token,
            &DecodingKey::from_secret(CONFIG.jwt_secret.as_bytes()),
            &validation,
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
                Err(actix_web::error::ErrorUnauthorized(
                    format!("Invalid token: {}", err)
                ))
            }),
        }
    }
}