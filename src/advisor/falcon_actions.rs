//! FalconGo's write routes the advisor may propose, as catalog rows.
//!
//! Field names are the JSON keys each handler reads (from its request struct
//! or `c.FormValue`), checked against FalconGo's models. Several handlers
//! overwrite the whole row with whatever they are sent; those rows carry
//! `merge`, so the fields a proposal leaves out are copied from the current
//! record and nothing is blanked by accident.
//!
//! The approver's token reaches FalconGo with `X-Via-AI`, so FalconGo's own
//! permission floor (4 for AI writes) and its `ai_audit_logs` apply on top of
//! each route's normal checks.

use serde_json::{json, Map, Value};

use super::actions::{f, int, req, Kind, Planned, Record, Source, Ty, BASE};
use super::upstream::Target;

const GAS: Ty = Ty::Str;
const SLACK_STATUS: Ty = Ty::Str;

const FUEL_FIELDS: &[(&str, &str)] = &[
    ("car_id", "car_id"),
    ("driver_name", "driver_name"),
    ("date", "date"),
    ("time", "time"),
    ("liters", "liters"),
    ("price_per_liter", "price_per_liter"),
    ("odometer_before", "odometer_before"),
    ("odometer_after", "odometer_after"),
    ("transporter", "transporter"),
    ("method", "method"),
];

const OIL_FIELDS: &[(&str, &str)] = &[
    ("car_id", "car_id"),
    ("driver_name", "driver_name"),
    ("super_visor", "super_visor"),
    ("date", "date"),
    ("mileage", "mileage"),
    ("odometer_at_change", "odometer_at_change"),
    ("current_odometer", "current_odometer"),
    ("oil_filter_changed", "oil_filter_changed"),
    ("fuel_filter_changed", "fuel_filter_changed"),
    ("water_filter_changed", "water_filter_changed"),
];

pub const KINDS: &[Kind] = &[
    // ── Trips ────────────────────────────────────────────────────────────
    Kind {
        name: "update_trip",
        what: "Edit one trip container: drop-off point (re-priced from fee mappings), receipt number, company, terminal, date, capacity, revenue or mileage. Only the fields sent change.",
        method: "PUT",
        path: "/trips/{id}",
        source: Source::Falcon("trips"),
        fields: &[
            f("drop_off_point", Ty::Str, "must have a fee mapping for the company and terminal"),
            f("receipt_no", Ty::Str, ""),
            f("company", Ty::Str, ""),
            f("terminal", Ty::Str, ""),
            f("terminal_id", Ty::Int, ""),
            f("date", Ty::Date, ""),
            f("tank_capacity", Ty::Int, "not on compartment trips"),
            f("gas_type", GAS, "not on compartment trips"),
            f("revenue", Ty::Num, "> 0"),
            f("mileage", Ty::Num, "> 0"),
            f("car_id", Ty::Int, "send with car_no_plate"),
            f("car_no_plate", Ty::Str, ""),
            f("driver_id", Ty::Int, "send with driver_name"),
            f("driver_name", Ty::Str, ""),
        ],
        ..BASE
    },
    Kind {
        name: "delete_trip",
        what: "Delete one trip container.",
        method: "DELETE",
        path: "/trips/{id}",
        source: Source::Falcon("trips"),
        ..BASE
    },
    Kind {
        name: "delete_parent_trip",
        what: "Delete a whole trip (the parent and all its containers).",
        method: "DELETE",
        path: "/trips/parent/{id}",
        source: Source::Falcon("parent_trips"),
        ..BASE
    },
    Kind {
        name: "add_receipt_step",
        what: "Record that a trip's receipt reached the garage or the office.",
        method: "POST",
        path: "/receipts/step",
        fields: &[
            req("trip_id", Ty::Int, ""),
            req("location", Ty::Enum(&["Garage", "Office"]), ""),
            req("received_by", Ty::Str, ""),
            f("stamped", Ty::Bool, ""),
            f("notes", Ty::Str, ""),
        ],
        ..BASE
    },
    // ── Fuel ─────────────────────────────────────────────────────────────
    Kind {
        name: "add_fuel_event",
        what: "Record a fuel fill-up. Price and fuel rate are computed; the truck's odometer and mileage update.",
        method: "POST",
        path: "/protected/AddFuelEvent",
        fields: &[
            req("car_id", Ty::Int, ""),
            req("date", Ty::Date, ""),
            f("time", Ty::Str, "HH:MM"),
            req("liters", Ty::Num, "> 0"),
            req("price_per_liter", Ty::Num, ""),
            req("odometer_before", Ty::Int, ""),
            req("odometer_after", Ty::Int, ""),
            f("driver_name", Ty::Str, ""),
        ],
        ..BASE
    },
    Kind {
        name: "edit_fuel_event",
        what: "Correct a fuel event (liters, price, odometers, date, driver). Only the fields sent change.",
        method: "POST",
        path: "/protected/EditFuelEvent",
        source: Source::Falcon("fuel_events"),
        fields: &[
            f("car_id", Ty::Int, ""),
            f("date", Ty::Date, ""),
            f("time", Ty::Str, ""),
            f("liters", Ty::Num, "> 0"),
            f("price_per_liter", Ty::Num, ""),
            f("odometer_before", Ty::Int, ""),
            f("odometer_after", Ty::Int, ""),
            f("driver_name", Ty::Str, ""),
        ],
        id_in_body: Some("ID"),
        merge: FUEL_FIELDS,
        ..BASE
    },
    Kind {
        name: "delete_fuel_event",
        what: "Delete a fuel event; the truck's mileage is recalculated.",
        method: "POST",
        path: "/protected/DeleteFuelEvent",
        source: Source::Falcon("fuel_events"),
        id_in_body: Some("ID"),
        ..BASE
    },
    // ── Oil changes (FalconGo's record; maint-rust's is log_oil_change) ──
    Kind {
        name: "create_oil_change",
        what: "Record an oil change in FalconGo (with cost and filters); the truck's mileage updates.",
        method: "POST",
        path: "/CreateOilChange",
        fields: &[
            req("car_id", Ty::Int, ""),
            req("date", Ty::Date, ""),
            req("current_odometer", Ty::Num, "km now"),
            f("odometer_at_change", Ty::Num, "km at the change"),
            f("mileage", Ty::Num, "km until the next change"),
            f("driver_name", Ty::Str, ""),
            f("super_visor", Ty::Str, ""),
            f("oil_filter_changed", Ty::Bool, ""),
            f("fuel_filter_changed", Ty::Bool, ""),
            f("water_filter_changed", Ty::Bool, ""),
            f("cost", Ty::Num, ""),
            f("transporter", Ty::Str, ""),
        ],
        ..BASE
    },
    Kind {
        name: "edit_oil_change",
        what: "Correct a FalconGo oil change (odometers, date, filters, driver). Cost cannot be edited.",
        method: "POST",
        path: "/EditOilChange",
        source: Source::Falcon("oil_changes"),
        fields: &[
            f("car_id", Ty::Int, ""),
            f("date", Ty::Date, ""),
            f("current_odometer", Ty::Num, ""),
            f("odometer_at_change", Ty::Num, ""),
            f("mileage", Ty::Num, ""),
            f("driver_name", Ty::Str, ""),
            f("super_visor", Ty::Str, ""),
            f("oil_filter_changed", Ty::Bool, ""),
            f("fuel_filter_changed", Ty::Bool, ""),
            f("water_filter_changed", Ty::Bool, ""),
        ],
        id_in_body: Some("ID"),
        merge: OIL_FIELDS,
        ..BASE
    },
    Kind {
        name: "delete_oil_change",
        what: "Delete a FalconGo oil change.",
        method: "DELETE",
        path: "/DeleteOilChange/{id}",
        source: Source::Falcon("oil_changes"),
        ..BASE
    },
    // ── Service invoices ─────────────────────────────────────────────────
    Kind {
        name: "create_service_invoice",
        what: "Record a service invoice (a workshop visit) with its line items; feeds semantic search and the truck's mileage.",
        method: "POST",
        path: "/service-invoices",
        fields: &[
            req("car_id", Ty::Int, ""),
            req("driver_name", Ty::Str, ""),
            req("date", Ty::Date, ""),
            req("meter_reading", Ty::Int, "km"),
            f("plate_number", Ty::Str, "filled from the truck if left out"),
            f("supervisor", Ty::Str, ""),
            f("operating_region", Ty::Str, ""),
            req("inspection_items", Ty::Items, "what was done"),
        ],
        ..BASE
    },
    Kind {
        name: "edit_service_invoice",
        what: "Correct a service invoice. Line items, if sent, replace the existing ones; otherwise they are kept.",
        method: "PUT",
        path: "/service-invoices/{id}",
        source: Source::Falcon("service_invoices"),
        fields: &[
            f("driver_name", Ty::Str, ""),
            f("date", Ty::Date, ""),
            f("meter_reading", Ty::Int, ""),
            f("plate_number", Ty::Str, ""),
            f("supervisor", Ty::Str, ""),
            f("operating_region", Ty::Str, ""),
            f("inspection_items", Ty::Items, "the full new list"),
        ],
        merge: &[
            ("driver_name", "driver_name"),
            ("date", "date"),
            ("meter_reading", "meter_reading"),
            ("plate_number", "plate_number"),
            ("supervisor", "supervisor"),
            ("operating_region", "operating_region"),
            ("inspection_items", "inspection_items"),
        ],
        ..BASE
    },
    Kind {
        name: "delete_service_invoice",
        what: "Delete a service invoice.",
        method: "DELETE",
        path: "/service-invoices/{id}",
        source: Source::Falcon("service_invoices"),
        ..BASE
    },
    // ── Trucks ───────────────────────────────────────────────────────────
    Kind {
        name: "reassign_car",
        what: "Move a truck to another operating company (customer) and/or area.",
        method: "PATCH",
        path: "/cars/{id}/assignment",
        source: Source::Falcon("cars"),
        fields: &[
            f("operating_company", Ty::Str, "must be a known company"),
            f("operating_area", Ty::Str, ""),
        ],
        id_param: "car_id",
        ..BASE
    },
    Kind {
        name: "assign_driver",
        what: "Pair a driver with a truck.",
        method: "PATCH",
        path: "/protected/SetCarDriverPair",
        source: Source::Falcon("cars"),
        fields: &[req("driver_id", Ty::Int, "")],
        id_param: "car_id",
        id_in_body: Some("car_id"),
        ..BASE
    },
    Kind {
        name: "set_truck_status",
        what: "Set a truck's status (as on the Slack board) and where it is.",
        method: "POST",
        path: "/slack/vehicles/{id}/status",
        source: Source::Falcon("cars"),
        fields: &[req("status", SLACK_STATUS, "a status from GET /slack/statuses"), f("location", Ty::Str, "")],
        id_param: "car_id",
        ..BASE
    },
    Kind {
        name: "update_car_documents",
        what: "Edit a truck's plate, type, capacity, transporter or licence expiry dates. Only the fields sent change.",
        method: "PUT",
        path: "/cars/{id}",
        source: Source::Falcon("cars"),
        fields: &[
            f("car_no_plate", Ty::Str, ""),
            f("car_type", Ty::Str, ""),
            f("transporter", Ty::Str, ""),
            f("tank_capacity", Ty::Int, ""),
            f("license_expiration_date", Ty::Date, ""),
            f("calibration_expiration_date", Ty::Date, ""),
            f("tank_license_expiration_date", Ty::Date, ""),
        ],
        form: true,
        id_param: "car_id",
        id_in_body: Some("ID"),
        merge: &[
            ("car_no_plate", "car_no_plate"),
            ("car_type", "car_type"),
            ("transporter", "transporter"),
            ("tank_capacity", "tank_capacity"),
            ("license_expiration_date", "license_expiration_date"),
            ("calibration_expiration_date", "calibration_expiration_date"),
            ("tank_license_expiration_date", "tank_license_expiration_date"),
        ],
        ..BASE
    },
    Kind {
        name: "create_car",
        what: "Add a new truck.",
        method: "POST",
        path: "/cars",
        fields: &[
            req("car_no_plate", Ty::Str, ""),
            f("car_type", Ty::Str, ""),
            f("tank_capacity", Ty::Int, ""),
            f("license_expiration_date", Ty::Date, ""),
            f("calibration_expiration_date", Ty::Date, ""),
            f("tank_license_expiration_date", Ty::Date, ""),
            f("operating_company", Ty::Str, ""),
            f("operating_area", Ty::Str, ""),
            f("driver_id", Ty::Int, ""),
        ],
        ..BASE
    },
    Kind {
        name: "delete_car",
        what: "Remove a truck.",
        method: "DELETE",
        path: "/cars/{id}",
        source: Source::Falcon("cars"),
        id_param: "car_id",
        id_in_body: Some("ID"),
        ..BASE
    },
    // ── Drivers ──────────────────────────────────────────────────────────
    Kind {
        name: "create_driver",
        what: "Add a new driver.",
        method: "POST",
        path: "/drivers",
        fields: &[
            req("name", Ty::Str, "at least 3 characters"),
            f("mobile_number", Ty::Str, ""),
            f("transporter", Ty::Str, ""),
            f("social_security_number", Ty::Str, ""),
            f("id_license_expiration_date", Ty::Date, ""),
            f("driver_license_expiration_date", Ty::Date, ""),
            f("safety_license_expiration_date", Ty::Date, ""),
            f("drug_test_expiration_date", Ty::Date, ""),
        ],
        form: true,
        ..BASE
    },
    Kind {
        name: "update_driver",
        what: "Edit a driver's name, phone, transporter, ID number or document expiry dates. Only the fields sent change; the login PIN is kept.",
        method: "PUT",
        path: "/drivers/documents/{id}",
        source: Source::Falcon("drivers"),
        fields: &[
            f("name", Ty::Str, ""),
            f("mobile_number", Ty::Str, ""),
            f("transporter", Ty::Str, ""),
            f("social_security_number", Ty::Str, ""),
            f("id_license_expiration_date", Ty::Date, ""),
            f("driver_license_expiration_date", Ty::Date, ""),
            f("safety_license_expiration_date", Ty::Date, ""),
            f("drug_test_expiration_date", Ty::Date, ""),
        ],
        form: true,
        id_param: "driver_id",
        id_in_body: Some("ID"),
        merge: &[
            ("name", "name"),
            ("mobile_number", "mobile_number"),
            ("transporter", "transporter"),
            ("social_security_number", "social_security_no"),
            ("id_license_expiration_date", "id_license_expiration_date"),
            ("driver_license_expiration_date", "driver_license_expiration_date"),
            ("safety_license_expiration_date", "safety_license_expiration_date"),
            ("drug_test_expiration_date", "drug_test_expiration_date"),
        ],
        ..BASE
    },
    Kind {
        name: "delete_driver",
        what: "Remove a driver.",
        method: "DELETE",
        path: "/drivers/{id}",
        source: Source::Falcon("drivers"),
        id_param: "driver_id",
        id_in_body: Some("ID"),
        ..BASE
    },
    // ── Driver money ─────────────────────────────────────────────────────
    Kind {
        name: "register_driver_loan",
        what: "Give a driver an advance, loan, or salary part.",
        method: "POST",
        path: "/RegisterDriverLoan",
        fields: &[
            req("driver_id", Ty::Int, ""),
            req("amount", Ty::Num, "> 0"),
            req("date", Ty::Date, ""),
            req("kind", Ty::Enum(&["advance", "loan", "salary"]), ""),
            f("method", Ty::Str, "cash, transfer…"),
            f("description", Ty::Str, ""),
            f("is_paid", Ty::Bool, "already recovered"),
        ],
        nest: Some("loan"),
        top: &["driver_id"],
        ..BASE
    },
    Kind {
        name: "delete_loan",
        what: "Delete a driver loan/advance.",
        method: "POST",
        path: "/DeleteLoan",
        source: Source::Falcon("loans"),
        id_in_body: Some("id"),
        ..BASE
    },
    Kind {
        name: "register_driver_expense",
        what: "Record an expense a driver paid or owes (tolls, repairs on the road…).",
        method: "POST",
        path: "/RegisterDriverExpense",
        fields: &[
            req("driver_id", Ty::Int, ""),
            req("cost", Ty::Num, "> 0"),
            req("date", Ty::Date, ""),
            req("category", Ty::Str, ""),
            f("description", Ty::Str, ""),
            f("payment_method", Ty::Str, ""),
            f("is_paid", Ty::Bool, ""),
        ],
        nest: Some("expense"),
        ..BASE
    },
    Kind {
        name: "delete_expense",
        what: "Delete a driver expense.",
        method: "POST",
        path: "/DeleteExpense",
        source: Source::Falcon("expenses"),
        id_in_body: Some("id"),
        ..BASE
    },
    Kind {
        name: "register_driver_salary",
        what: "Close a driver's pay period: records the salary AND marks every unpaid expense and loan of that driver dated in the period as paid.",
        method: "POST",
        path: "/RegisterDriverSalary",
        fields: &[
            req("driver_id", Ty::Int, ""),
            req("driver_cost", Ty::Num, "the salary"),
            req("start_date", Ty::Date, ""),
            req("close_date", Ty::Date, ""),
        ],
        ..BASE
    },
    Kind {
        name: "adjust_credit_balance",
        what: "Adjust a driver's or employee's credit balance (positive adds, negative deducts).",
        method: "POST",
        path: "/credit-balance/adjust",
        fields: &[
            req("entity_type", Ty::Enum(&["driver", "employee"]), ""),
            req("entity_id", Ty::Int, ""),
            req("amount", Ty::Num, "non-zero"),
            req("description", Ty::Str, ""),
        ],
        ..BASE
    },
    // ── Vendors ──────────────────────────────────────────────────────────
    Kind {
        name: "add_vendor_transaction",
        what: "Record a purchase from (debit) or payment to (credit) a vendor.",
        method: "POST",
        path: "/vendors/{id}/transactions",
        source: Source::Falcon("vendors"),
        fields: &[
            req("description", Ty::Str, ""),
            req("amount", Ty::Num, "> 0"),
            req("date", Ty::Date, ""),
            req("type", Ty::Enum(&["debit", "credit"]), "debit = we owe them more"),
        ],
        id_param: "vendor_id",
        ..BASE
    },
    Kind {
        name: "edit_vendor_transaction",
        what: "Correct a vendor transaction. Only the fields sent change.",
        method: "PUT",
        path: "/transactions/{id}",
        source: Source::Falcon("vendor_transactions"),
        fields: &[
            f("description", Ty::Str, ""),
            f("amount", Ty::Num, "> 0"),
            f("date", Ty::Date, ""),
            f("type", Ty::Enum(&["debit", "credit"]), ""),
        ],
        ..BASE
    },
    Kind {
        name: "delete_vendor_transaction",
        what: "Delete a vendor transaction.",
        method: "DELETE",
        path: "/transactions/{id}",
        source: Source::Falcon("vendor_transactions"),
        ..BASE
    },
    // ── Pricing ──────────────────────────────────────────────────────────
    Kind {
        name: "create_fee_mapping",
        what: "Add a route price: what a company pays per trip from a terminal to a drop-off point, and its distance.",
        method: "POST",
        path: "/mappings",
        fields: &[
            req("company", Ty::Str, ""),
            req("terminal", Ty::Str, ""),
            req("drop_off_point", Ty::Str, ""),
            req("distance", Ty::Num, "km"),
            req("fee", Ty::Num, "per trip"),
        ],
        ..BASE
    },
    Kind {
        name: "update_fee_mapping",
        what: "Change a route's price or distance (future trips; existing trips keep their price).",
        method: "PUT",
        path: "/mappings/{id}",
        source: Source::Falcon("fee_mappings"),
        fields: &[
            f("company", Ty::Str, ""),
            f("terminal", Ty::Str, ""),
            f("drop_off_point", Ty::Str, ""),
            f("distance", Ty::Num, ""),
            f("fee", Ty::Num, ""),
        ],
        merge: &[("distance", "distance"), ("fee", "fee")],
        ..BASE
    },
    Kind {
        name: "delete_fee_mapping",
        what: "Remove a route price.",
        method: "DELETE",
        path: "/mappings/{id}",
        source: Source::Falcon("fee_mappings"),
        ..BASE
    },
    // ── Speed violations ─────────────────────────────────────────────────
    Kind {
        name: "edit_violation",
        what: "Correct a speed violation: place, time, fine, or which driver it was.",
        method: "PUT",
        path: "/violations/{id}",
        source: Source::Falcon("speed_violations"),
        fields: &[
            f("violation_location", Ty::Str, ""),
            f("violation_date", Ty::DateTime, ""),
            f("fine_amount", Ty::Num, "> 0"),
            f("driver_id", Ty::Int, ""),
            f("driver_confidence", Ty::Num, "> 0"),
        ],
        ..BASE
    },
    Kind {
        name: "delete_violation",
        what: "Delete a speed violation.",
        method: "DELETE",
        path: "/violations/{id}",
        source: Source::Falcon("speed_violations"),
        ..BASE
    },
];

/// Kinds with a nested body the catalog cannot express.
pub const SPECIAL: &[(&str, &str)] = &[
    ("create_trip", "Record a trip: one truck, one driver, a company and terminal, and 1–4 containers each with a drop-off point and receipt number. Revenue and distance come from the fee mappings."),
    ("update_parent_trip", "Change a whole trip's truck, driver, company, terminal or date; every container follows."),
];

pub fn describe_special(name: &str) -> Option<Value> {
    let what = SPECIAL.iter().find(|(n, _)| *n == name)?.1;
    let params = match name {
        "create_trip" => json!({
            "type": "object",
            "properties": {
                "car_id": { "type": "integer" },
                "driver_id": { "type": "integer" },
                "company": { "type": "string" },
                "terminal": { "type": "string", "description": "or terminal_id" },
                "terminal_id": { "type": "integer" },
                "date": Ty::Date.describe(),
                "containers": { "type": "array", "minItems": 1, "maxItems": 4, "items": { "type": "object", "properties": {
                    "drop_off_point": { "type": "string" },
                    "receipt_no": { "type": "string" },
                    "tank_capacity": { "type": "integer" },
                    "gas_type": { "type": "string" }
                }, "required": ["drop_off_point", "receipt_no", "tank_capacity"] } },
                "force_create": { "type": "boolean", "description": "create even if a receipt number already exists" }
            },
            "required": ["car_id", "driver_id", "company", "date", "containers"]
        }),
        "update_parent_trip" => json!({
            "type": "object",
            "properties": {
                "id": { "type": "integer", "description": "the parent trip id" },
                "car_id": { "type": "integer" },
                "driver_id": { "type": "integer" },
                "company": { "type": "string" },
                "terminal": { "type": "string" },
                "terminal_id": { "type": "integer" },
                "date": Ty::Date.describe()
            },
            "required": ["id"]
        }),
        _ => return None,
    };
    Some(json!({ "service": "falcon", "what": what, "params": params }))
}

fn copy(params: &Value, out: &mut Map<String, Value>, fields: &[(&str, Ty)]) -> Result<(), String> {
    for (key, ty) in fields {
        if let Some(v) = params.get(*key).filter(|v| !v.is_null()) {
            out.insert((*key).into(), ty.coerce(key, v)?);
        }
    }
    Ok(())
}

/// `Ok(None)`: not one of these kinds. Plates and driver names are filled in
/// from the database by the tool (`enrich`), so the model cannot misspell them.
pub fn plan_special(kind: &str, params: &Value) -> Result<Option<Planned>, String> {
    match kind {
        "create_trip" => {
            let mut parent = Map::new();
            parent.insert("car_id".into(), json!(int(params, "car_id")?));
            parent.insert("driver_id".into(), json!(int(params, "driver_id")?));
            copy(params, &mut parent, &[("company", Ty::Str), ("terminal", Ty::Str), ("terminal_id", Ty::Int)])?;
            parent.insert("date".into(), Ty::Date.coerce("date", params.get("date").unwrap_or(&Value::Null))?);
            if !parent.contains_key("company") {
                return Err("`company` is required".into());
            }
            if !parent.contains_key("terminal") && !parent.contains_key("terminal_id") {
                return Err("`terminal` or `terminal_id` is required".into());
            }
            let list = params
                .get("containers")
                .and_then(Value::as_array)
                .filter(|c| (1..=4).contains(&c.len()))
                .ok_or("`containers` must list 1 to 4 containers")?;
            let mut containers = Vec::new();
            for (n, c) in list.iter().enumerate() {
                let mut out = Map::new();
                for key in ["drop_off_point", "receipt_no"] {
                    let v = c.get(key).ok_or(format!("container {} needs {key}", n + 1))?;
                    out.insert(key.into(), Ty::Str.coerce(key, v)?);
                }
                let cap = c.get("tank_capacity").ok_or(format!("container {} needs tank_capacity", n + 1))?;
                out.insert("tank_capacity".into(), Ty::Int.coerce("tank_capacity", cap)?);
                copy(c, &mut out, &[("gas_type", Ty::Str)])?;
                containers.push(Value::Object(out));
            }
            let mut body = json!({ "parent_trip": parent, "containers": containers });
            if params.get("force_create").and_then(Value::as_bool) == Some(true) {
                body["force_create"] = json!(true);
            }
            Ok(Some(Planned {
                kind: "create_trip",
                target: Target::Falcon,
                method: "POST",
                path: "/trips/multi-container".into(),
                body,
                meta: json!({ "encoding": "json", "enrich": "trip" }),
                record: Record::None,
            }))
        }
        "update_parent_trip" => {
            let id = int(params, "id")?;
            let mut parent = Map::new();
            copy(
                params,
                &mut parent,
                &[
                    ("car_id", Ty::Int),
                    ("driver_id", Ty::Int),
                    ("company", Ty::Str),
                    ("terminal", Ty::Str),
                    ("terminal_id", Ty::Int),
                    ("date", Ty::Date),
                ],
            )?;
            if parent.is_empty() {
                return Err("update_parent_trip needs at least one field to change".into());
            }
            Ok(Some(Planned {
                kind: "update_parent_trip",
                target: Target::Falcon,
                method: "PUT",
                path: format!("/trips/parent/{id}"),
                body: json!({ "parent_trip": parent, "containers": [], "update_containers": false }),
                meta: json!({ "encoding": "json", "enrich": "trip" }),
                record: Record::Falcon { table: "parent_trips", id },
            }))
        }
        _ => Ok(None),
    }
}

/// Fill a full-row edit's missing fields from the current record. `before`
/// is the row as `to_jsonb`; dates stored as timestamps are cut to the day.
pub fn merge_into(body: &mut Value, meta: &Value, before: &Value) {
    let Some(pairs) = meta.get("merge").and_then(Value::as_array) else {
        return;
    };
    let nest = meta.get("nest").and_then(Value::as_str);
    let target = match nest {
        Some(k) => body.get_mut(k),
        None => Some(body),
    };
    let Some(Value::Object(obj)) = target else {
        return;
    };
    for pair in pairs {
        let (Some(key), Some(col)) = (pair.get(0).and_then(Value::as_str), pair.get(1).and_then(Value::as_str)) else {
            continue;
        };
        if obj.contains_key(key) {
            continue;
        }
        if let Some(v) = before.get(col).filter(|v| !v.is_null()) {
            let v = match (key, v.as_str()) {
                (k, Some(s)) if k.ends_with("date") && s.len() > 10 && s.as_bytes()[4] == b'-' => json!(&s[..10]),
                _ => v.clone(),
            };
            obj.insert(key.into(), v);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::advisor::actions::plan;

    #[test]
    fn a_loan_nests_under_loan_with_the_driver_on_top() {
        let p = plan(
            "register_driver_loan",
            &json!({ "driver_id": 7, "amount": 500, "date": "2026-10-25", "kind": "advance" }),
        )
        .unwrap();
        assert_eq!(p.body["driver_id"], 7);
        assert_eq!(p.body["loan"]["amount"], 500.0);
        assert!(p.body["loan"].get("driver_id").is_none());
    }

    #[test]
    fn deletes_that_read_the_id_from_the_body_get_it() {
        let p = plan("delete_fuel_event", &json!({ "id": 31 })).unwrap();
        assert_eq!(p.body, json!({ "ID": 31 }));
        assert_eq!(p.record, Record::Falcon { table: "fuel_events", id: 31 });
        let p = plan("assign_driver", &json!({ "car_id": 12, "driver_id": 3 })).unwrap();
        assert_eq!(p.body, json!({ "driver_id": 3, "car_id": 12 }));
        assert_eq!(p.path, "/protected/SetCarDriverPair");
    }

    #[test]
    fn a_full_row_edit_keeps_what_it_does_not_change() {
        let mut p = plan("edit_fuel_event", &json!({ "id": 31, "liters": 410.5 })).unwrap();
        let before = json!({ "id": 31, "car_id": 12, "driver_name": "Ali", "date": "2026-10-20", "time": "08:10",
            "liters": 400.0, "price_per_liter": 15.5, "odometer_before": 1000, "odometer_after": 1900,
            "transporter": "Apex", "method": "Manual" });
        merge_into(&mut p.body, &p.meta, &before);
        assert_eq!(p.body["liters"], 410.5);
        assert_eq!(p.body["driver_name"], "Ali");
        assert_eq!(p.body["ID"], 31);
        assert_eq!(p.body["method"], "Manual");
    }

    #[test]
    fn timestamp_dates_are_cut_to_the_day_when_merged() {
        let mut p = plan("edit_service_invoice", &json!({ "id": 4, "supervisor": "Omar" })).unwrap();
        merge_into(&mut p.body, &p.meta, &json!({ "date": "2026-10-19T00:00:00+00:00", "driver_name": "Ali" }));
        assert_eq!(p.body["date"], "2026-10-19");
        assert_eq!(p.body["supervisor"], "Omar");
    }

    #[test]
    fn the_driver_ssn_maps_to_its_column() {
        let mut p = plan("update_driver", &json!({ "driver_id": 5, "mobile_number": "010" })).unwrap();
        merge_into(&mut p.body, &p.meta, &json!({ "name": "Ali", "social_security_no": "2990101" }));
        assert_eq!(p.body["social_security_number"], "2990101");
        assert_eq!(p.body["ID"], 5);
    }

    #[test]
    fn a_trip_needs_containers_and_a_terminal() {
        let base = json!({ "car_id": 1, "driver_id": 2, "company": "Watanya", "terminal": "Mostorod", "date": "2026-10-25",
            "containers": [{ "drop_off_point": "Benha", "receipt_no": "R1", "tank_capacity": 38000 }] });
        let p = plan("create_trip", &base).unwrap();
        assert_eq!(p.body["parent_trip"]["company"], "Watanya");
        assert_eq!(p.body["containers"][0]["tank_capacity"], 38000);
        let mut none = base.clone();
        none["containers"] = json!([]);
        assert!(plan("create_trip", &none).is_err());
        let mut no_terminal = base.clone();
        no_terminal.as_object_mut().unwrap().remove("terminal");
        assert!(plan("create_trip", &no_terminal).is_err());
    }
}
