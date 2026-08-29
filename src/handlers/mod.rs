pub mod dashboard;
pub mod trip_stats;
pub mod trips;

pub use trip_stats::*;

pub mod session;
pub use session::*;

use actix_web::web;

use crate::auth::JwtAuth;

/// The non-banksms `/api/v1` surface, as ONE scope.
///
/// It has to be one scope. actix matches the first service whose prefix
/// matches and does not fall through, so a second `web::scope("/api/v1")`
/// registered alongside this one is dead: every `/api/v1/...` request is
/// answered by whichever scope was registered first, and anything only the
/// second one knows about 404s. That shipped once — `/api/v1/trips` was
/// mounted in its own scope and returned 404 in production while every test
/// passed, because the tests mounted that scope alone and never alongside this
/// one.
///
/// Which is the other reason this lives in the lib: the integration suite
/// mounts the same function the binary does, so the wiring itself is under
/// test rather than just the handlers.
pub fn configure_api_v1(cfg: &mut web::ServiceConfig) {
    let at = |level: i32| JwtAuth {
        required_permission: Some(level),
    };

    cfg.service(
        web::scope("/api/v1")
            // The entry point. The list itself is permission 1; the handler
            // withholds the money block below 4 on its own.
            .route(
                "/dashboard",
                web::get().to(dashboard::get_dashboard).wrap(at(1)),
            )
            // Drawers behind the money cards are pure money — 4 only,
            // enforced INSIDE the handlers: JwtAuth's required_permission is
            // not checked by the middleware (handlers own the ladder, so pages
            // can serve reduced data to lower levels). The trips drawer
            // carries counts, not money, and is open to any admin token.
            .route(
                "/dashboard/revenue",
                web::get().to(dashboard::get_revenue_drawer).wrap(at(4)),
            )
            .route(
                "/dashboard/cash-out",
                web::get().to(dashboard::get_cash_out_drawer).wrap(at(4)),
            )
            .route(
                "/dashboard/advances",
                web::get().to(dashboard::get_advances_drawer).wrap(at(4)),
            )
            .route(
                "/dashboard/trips",
                web::get().to(dashboard::get_trips_drawer).wrap(at(1)),
            )
            .route(
                "/dashboard/fuel",
                web::get().to(dashboard::get_fuel_drawer).wrap(at(4)),
            )
            .route(
                "/dashboard/fuel-events",
                web::get().to(dashboard::get_fuel_events).wrap(at(4)),
            )
            .service(web::scope("/sessions").route(
                "/{id}/location-pings",
                web::get().to(session::get_session_location_pings).wrap(at(1)),
            ))
            .route(
                "/trip-statistics",
                web::get().to(trip_stats::get_trip_statistics).wrap(at(3)),
            )
            .route(
                "/trip-statistics/route-days",
                web::get().to(trip_stats::get_route_days).wrap(at(3)),
            )
            // Permission 1 to SEE the list, matching the FalconGo route this
            // replaces -- gating the list higher would lock every dispatcher out
            // of the trips page. Revenue is the level-4 feature and the handler
            // withholds it on its own.
            .route(
                "/trips",
                web::get().to(trips::get_trips).wrap(at(trips::VIEW_PERMISSION)),
            ),
    );
}
