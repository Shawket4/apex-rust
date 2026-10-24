//! The changes the advisor may propose, and the API call each one becomes.
//!
//! An allowlist, not a free-form "call this endpoint": the model picks a kind
//! and fills its parameters, this module turns it into exactly one request
//! against one known route. A new kind of change is one arm here — and one
//! route in the owning service, which stays the only writer of its data.

use serde_json::{json, Value};

use super::upstream::Target;

/// A proposal turned into the request that will apply it.
#[derive(Debug, Clone, PartialEq)]
pub struct Planned {
    pub kind: &'static str,
    pub target: Target,
    pub method: &'static str,
    pub path: String,
    pub body: Value,
}

/// The kinds offered to the model, with the shape of their parameters.
pub fn kinds_schema() -> Value {
    json!({
        "reassign_car": {
            "what": "Move a truck to another operating company (customer) and/or area. FalconGo: PATCH /cars/:id/assignment.",
            "params": { "car_id": "integer", "operating_company": "string, optional", "operating_area": "string, optional" }
        },
        "assign_driver": {
            "what": "Pair a driver with a truck. FalconGo: PATCH /protected/SetCarDriverPair.",
            "params": { "car_id": "integer", "driver_id": "integer" }
        }
    })
}

fn int(params: &Value, key: &str) -> Result<i64, String> {
    params
        .get(key)
        .and_then(|v| v.as_i64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok())))
        .filter(|n| *n > 0)
        .ok_or_else(|| format!("`{key}` must be a positive integer"))
}

fn text(params: &Value, key: &str) -> Option<String> {
    params
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Validate a proposal's shape and build its request. Existence checks
/// (does car 12 exist, is that company real) happen in the tool, against the
/// database, where the current values are also captured for the reviewer.
pub fn plan(kind: &str, params: &Value) -> Result<Planned, String> {
    match kind {
        "reassign_car" => {
            let car_id = int(params, "car_id")?;
            let company = text(params, "operating_company");
            let area = text(params, "operating_area");
            if company.is_none() && area.is_none() {
                return Err("reassign_car needs operating_company and/or operating_area".into());
            }
            let mut body = json!({});
            if let Some(c) = company {
                body["operating_company"] = json!(c);
            }
            if let Some(a) = area {
                body["operating_area"] = json!(a);
            }
            Ok(Planned {
                kind: "reassign_car",
                target: Target::Falcon,
                method: "PATCH",
                path: format!("/cars/{car_id}/assignment"),
                body,
            })
        }
        "assign_driver" => {
            let car_id = int(params, "car_id")?;
            let driver_id = int(params, "driver_id")?;
            Ok(Planned {
                kind: "assign_driver",
                target: Target::Falcon,
                method: "PATCH",
                path: "/protected/SetCarDriverPair".into(),
                body: json!({ "car_id": car_id, "driver_id": driver_id }),
            })
        }
        other => Err(format!(
            "unknown change kind `{other}`; available: reassign_car, assign_driver"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reassign_sends_only_what_changes() {
        let p = plan("reassign_car", &json!({ "car_id": 12, "operating_company": " Watanya " }))
            .unwrap();
        assert_eq!(p.path, "/cars/12/assignment");
        assert_eq!(p.body, json!({ "operating_company": "Watanya" }));
        assert!(plan("reassign_car", &json!({ "car_id": 12 })).is_err());
    }

    #[test]
    fn ids_must_be_positive_integers() {
        assert!(plan("assign_driver", &json!({ "car_id": 0, "driver_id": 3 })).is_err());
        assert!(plan("assign_driver", &json!({ "car_id": "7", "driver_id": 3 })).is_ok());
    }

    #[test]
    fn unknown_kinds_are_refused() {
        assert!(plan("delete_car", &json!({ "car_id": 1 })).is_err());
    }
}
