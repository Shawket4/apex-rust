//! The changes the advisor may propose, and the API call each one becomes.
//!
//! An allowlist, not a free-form "call this endpoint": the model picks a kind
//! and fills its parameters, this module turns it into exactly one request
//! against one known route of the service that owns the data (FalconGo,
//! maint-rust, or apex-rust's own bank-SMS ledger). Nothing here writes a
//! database; every change is that service's own handler running with the
//! approver's token.
//!
//! Most kinds are rows in `catalog()`: a route, the fields it accepts with
//! their types, and where its current state can be read for the reviewer. The
//! maintenance kinds are built in code (`maint`), because maint-rust takes
//! every write as a sync batch keyed by entity type and version.

use serde_json::{json, Map, Value};
use uuid::Uuid;

use super::upstream::Target;

/// A proposal turned into the request that will apply it.
#[derive(Debug, Clone, PartialEq)]
pub struct Planned {
    pub kind: &'static str,
    pub target: Target,
    pub method: &'static str,
    pub path: String,
    pub body: Value,
    /// How to send and judge it: `encoding` ("json" | "form"), `if_match`
    /// (apex-rust ledger version), `check` ("sync_push" — maint-rust answers
    /// 200 with per-operation results that must all be "applied").
    pub meta: Value,
    /// Where the reviewer's "before" comes from, and what the version guard is
    /// filled from.
    pub record: Record,
}

/// The current state of the record a change touches.
#[derive(Debug, Clone, PartialEq)]
pub enum Record {
    /// A create: nothing exists yet.
    None,
    /// A row in the shared Falcon database (`apex`), read directly.
    Falcon { table: &'static str, id: i64 },
    /// A bank-SMS ledger row; its `version` becomes the If-Match.
    ApexTransaction { id: i64 },
    /// A maint-rust row, read through its records API; its `sync_version`
    /// fills the batch's operation(s) that need one.
    Maint { entity_type: String, entity_id: String },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Ty {
    Int,
    Num,
    /// A decimal amount sent as a string (apex-rust's `Decimal`).
    Money,
    Str,
    Bool,
    /// YYYY-MM-DD
    Date,
    /// RFC 3339
    DateTime,
    Enum(&'static [&'static str]),
    /// A list of `{service, notes}` lines (a service invoice's items).
    Items,
}

impl Ty {
    pub(crate) fn describe(self) -> Value {
        match self {
            Ty::Int => json!({ "type": "integer" }),
            Ty::Num => json!({ "type": "number" }),
            Ty::Money => json!({ "type": "string", "description": "decimal amount, e.g. \"1250.50\"" }),
            Ty::Str => json!({ "type": "string" }),
            Ty::Bool => json!({ "type": "boolean" }),
            Ty::Date => json!({ "type": "string", "format": "date", "description": "YYYY-MM-DD" }),
            Ty::DateTime => json!({ "type": "string", "format": "date-time", "description": "RFC 3339, e.g. 2026-10-25T08:00:00+03:00" }),
            Ty::Enum(vals) => json!({ "type": "string", "enum": vals }),
            Ty::Items => json!({ "type": "array", "items": { "type": "object", "properties": {
                "service": { "type": "string" }, "notes": { "type": "string" } } } }),
        }
    }

    /// Coerce a model-supplied value. Strings holding numbers are accepted for
    /// numeric types, since models often quote them.
    pub(crate) fn coerce(self, key: &str, v: &Value) -> Result<Value, String> {
        let bad = |what: &str| Err(format!("`{key}` must be {what}"));
        match self {
            Ty::Int => match v.as_i64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok())) {
                Some(n) => Ok(json!(n)),
                None => bad("an integer"),
            },
            Ty::Num => match v.as_f64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok())) {
                Some(n) if n.is_finite() => Ok(json!(n)),
                _ => bad("a number"),
            },
            Ty::Money => {
                let s = match v {
                    Value::String(s) => s.trim().to_string(),
                    Value::Number(n) => n.to_string(),
                    _ => return bad("a decimal amount"),
                };
                match s.parse::<f64>() {
                    Ok(n) if n.is_finite() && n > 0.0 => Ok(json!(s)),
                    _ => bad("a positive decimal amount"),
                }
            }
            Ty::Str => match v.as_str().map(str::trim) {
                Some(s) if !s.is_empty() => Ok(json!(s)),
                _ => bad("a non-empty string"),
            },
            Ty::Bool => match v {
                Value::Bool(b) => Ok(json!(b)),
                Value::String(s) if s == "true" || s == "false" => Ok(json!(s == "true")),
                _ => bad("true or false"),
            },
            Ty::Date => match v.as_str().map(str::trim) {
                Some(s) if chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").is_ok() => Ok(json!(s)),
                _ => bad("a date YYYY-MM-DD"),
            },
            Ty::DateTime => match v.as_str().map(str::trim) {
                Some(s) if chrono::DateTime::parse_from_rfc3339(s).is_ok() => Ok(json!(s)),
                _ => bad("an RFC 3339 timestamp"),
            },
            Ty::Enum(vals) => match v.as_str().map(str::trim) {
                Some(s) if vals.contains(&s) => Ok(json!(s)),
                _ => bad(&format!("one of {}", vals.join(", "))),
            },
            Ty::Items => {
                let items = v.as_array().ok_or(format!("`{key}` must be a list"))?;
                let mut out = Vec::new();
                for it in items {
                    let line = |k: &str| it.get(k).and_then(Value::as_str).map(str::trim).unwrap_or("").to_string();
                    let (service, notes) = (line("service"), line("notes"));
                    if !service.is_empty() || !notes.is_empty() {
                        out.push(json!({ "service": service, "notes": notes }));
                    }
                }
                Ok(Value::Array(out))
            }
        }
    }
}

/// One body field a route accepts.
#[derive(Debug, Clone, Copy)]
pub struct Field {
    pub name: &'static str,
    pub ty: Ty,
    pub required: bool,
    /// May be sent as JSON null to clear it (PATCH routes that distinguish
    /// "absent" from "null").
    pub nullable: bool,
    pub doc: &'static str,
}

pub(crate) const fn f(name: &'static str, ty: Ty, doc: &'static str) -> Field {
    Field { name, ty, required: false, nullable: false, doc }
}
pub(crate) const fn req(name: &'static str, ty: Ty, doc: &'static str) -> Field {
    Field { name, ty, required: true, nullable: false, doc }
}
pub(crate) const fn clearable(name: &'static str, ty: Ty, doc: &'static str) -> Field {
    Field { name, ty, required: false, nullable: true, doc }
}

/// Where a catalog kind's record lives, for the before-state.
#[derive(Debug, Clone, Copy)]
pub enum Source {
    None,
    Falcon(&'static str),
    ApexTransaction,
}

/// A route the advisor may call, as data.
#[derive(Debug, Clone, Copy)]
pub struct Kind {
    pub name: &'static str,
    pub target: Target,
    pub what: &'static str,
    pub method: &'static str,
    /// May contain `{id}`, filled from the `id` parameter.
    pub path: &'static str,
    /// The record the change touches; `id` is then a required parameter.
    pub source: Source,
    pub fields: &'static [Field],
    /// Sent as application/x-www-form-urlencoded (Fiber handlers that read
    /// `c.FormValue`) instead of JSON.
    pub form: bool,
    /// Extra fixed body keys.
    pub fixed: &'static [(&'static str, &'static str)],
    /// The parameter naming the record (`id` unless the route calls it
    /// something else, e.g. `car_id`).
    pub id_param: &'static str,
    /// Also send the id in the body under this key (FalconGo routes that
    /// read `ID` from the body rather than the path).
    pub id_in_body: Option<&'static str>,
    /// The route overwrites the whole row: fields the proposal leaves out are
    /// filled from the current record, (body key, column), so an edit never
    /// blanks what it did not mean to touch.
    pub merge: &'static [(&'static str, &'static str)],
    /// Nest the fields under this key (`{"expense": {...}}`), except those in
    /// `top`.
    pub nest: Option<&'static str>,
    pub top: &'static [&'static str],
}

/// Defaults for the layout fields, for `..BASE` in the catalogs.
pub const BASE: Kind = Kind {
    name: "",
    target: Target::Falcon,
    what: "",
    method: "POST",
    path: "",
    source: Source::None,
    fields: &[],
    form: false,
    fixed: &[],
    id_param: "id",
    id_in_body: None,
    merge: &[],
    nest: None,
    top: &[],
};

impl Kind {
    fn needs_id(&self) -> bool {
        self.path.contains("{id}") || !matches!(self.source, Source::None)
    }
}

const APEX: Target = Target::Apex;
const DIRECTION: Ty = Ty::Enum(&["in", "out"]);

/// apex-rust's bank-SMS ledger. Templates are left out on purpose: saving
/// one re-parses the whole message backlog.
const APEX_KINDS: &[Kind] = &[
    Kind {
        name: "create_transaction",
        target: APEX,
        what: "Record a money movement in the bank-SMS ledger (a manual transaction). A category that requires a party needs driver_id or employee_id; a loan category registers the loan.",
        method: "POST",
        path: "/api/v1/transactions",
        source: Source::None,
        fields: &[
            req("direction", DIRECTION, "in = received, out = paid"),
            req("amount", Ty::Money, "> 0"),
            req("occurred_at", Ty::DateTime, "when it happened"),
            f("currency", Ty::Str, "default EGP"),
            f("category", Ty::Str, "a banksms.categories name"),
            f("description", Ty::Str, ""),
            f("counterparty", Ty::Str, ""),
            f("account", Ty::Str, ""),
            f("reference", Ty::Str, ""),
            f("payment_method", Ty::Str, ""),
            f("company", Ty::Str, "operating company"),
            f("paid_by", Ty::Str, ""),
            f("car_id", Ty::Int, ""),
            f("driver_id", Ty::Int, "not with employee_id"),
            f("employee_id", Ty::Int, "not with driver_id"),
            f("raw_message_id", Ty::Int, "the bank SMS it came from, if any"),
        ],
        ..BASE
    },
    Kind {
        name: "edit_transaction",
        target: APEX,
        what: "Edit a ledger transaction: category, party, car, description, amount, date. Send only what changes; null clears a clearable field. Guarded by its version, so a row edited since the proposal is not overwritten.",
        method: "PATCH",
        path: "/api/v1/transactions/{id}",
        source: Source::ApexTransaction,
        fields: &[
            f("direction", DIRECTION, ""),
            f("amount", Ty::Money, "not on a split part"),
            f("occurred_at", Ty::DateTime, "not on a split part"),
            f("currency", Ty::Str, ""),
            clearable("category", Ty::Str, ""),
            clearable("description", Ty::Str, ""),
            clearable("counterparty", Ty::Str, ""),
            clearable("account", Ty::Str, ""),
            clearable("reference", Ty::Str, ""),
            clearable("payment_method", Ty::Str, ""),
            clearable("company", Ty::Str, ""),
            clearable("paid_by", Ty::Str, ""),
            clearable("car_id", Ty::Int, ""),
            clearable("driver_id", Ty::Int, ""),
            clearable("employee_id", Ty::Int, ""),
            clearable("linked_loan_id", Ty::Int, "a non-banksms public.loans id this payment settles"),
        ],
        ..BASE
    },
    Kind {
        name: "delete_transaction",
        target: APEX,
        what: "Soft-delete a ledger transaction (refused for split parents and paid loans).",
        method: "DELETE",
        path: "/api/v1/transactions/{id}",
        source: Source::ApexTransaction,
        fields: &[],
        ..BASE
    },
    Kind {
        name: "dismiss_classification",
        target: APEX,
        what: "Dismiss the open category suggestion on a ledger transaction.",
        method: "POST",
        path: "/api/v1/transactions/{id}/suggestion/dismiss",
        source: Source::ApexTransaction,
        fields: &[],
        ..BASE
    },
];

/// Every catalog kind, across services.
pub fn catalog() -> Vec<&'static Kind> {
    APEX_KINDS.iter().chain(super::falcon_actions::KINDS.iter()).collect()
}

fn find(kind: &str) -> Option<&'static Kind> {
    catalog().into_iter().find(|k| k.name == kind)
}

/// Names of every kind, catalog and maintenance, for the tool's enum.
pub fn kind_names() -> Vec<&'static str> {
    let mut v: Vec<&'static str> = catalog().iter().map(|k| k.name).collect();
    v.extend(maint::KINDS.iter().map(|(n, _)| *n));
    v.push("split_transaction");
    v.extend(super::falcon_actions::SPECIAL.iter().map(|(n, _)| *n));
    v
}

/// One line per kind, for the propose_change description.
pub fn summary() -> String {
    let mut lines: Vec<String> = catalog().iter().map(|k| format!("{}: {}", k.name, k.what)).collect();
    lines.extend(maint::KINDS.iter().map(|(n, w)| format!("{n}: {w}")));
    lines.push("split_transaction: Split an outgoing ledger transaction into parts that sum exactly to it.".into());
    lines.extend(super::falcon_actions::SPECIAL.iter().map(|(n, w)| format!("{n}: {w}")));
    lines.join("\n")
}

/// Full parameter schemas, for `describe_actions`.
pub fn describe(names: &[String]) -> Value {
    let mut out = Map::new();
    for name in names {
        if let Some(k) = find(name) {
            let mut props = Map::new();
            let mut required: Vec<&str> = Vec::new();
            if k.needs_id() {
                props.insert(k.id_param.into(), json!({ "type": "integer", "description": "the record's id" }));
                required.push(k.id_param);
            }
            for fl in k.fields {
                let mut d = fl.ty.describe();
                if !fl.doc.is_empty() {
                    d["description"] = json!(match d.get("description").and_then(Value::as_str) {
                        Some(base) => format!("{base}; {}", fl.doc),
                        None => fl.doc.to_string(),
                    });
                }
                if fl.nullable {
                    d["nullable"] = json!(true);
                }
                props.insert(fl.name.into(), d);
                if fl.required {
                    required.push(fl.name);
                }
            }
            out.insert(
                name.clone(),
                json!({
                    "service": format!("{:?}", k.target).to_lowercase(),
                    "what": k.what,
                    "params": { "type": "object", "properties": props, "required": required },
                }),
            );
        } else if let Some(schema) = super::falcon_actions::describe_special(name) {
            out.insert(name.clone(), schema);
        } else if let Some(schema) = maint::describe(name) {
            out.insert(name.clone(), schema);
        } else if name == "split_transaction" {
            out.insert(name.clone(), json!({
                "service": "apex",
                "what": "Split an outgoing ledger transaction into 2+ parts whose amounts sum exactly to it. Refused for loans and already-split rows.",
                "params": {
                    "type": "object",
                    "properties": {
                        "id": { "type": "integer" },
                        "parts": { "type": "array", "items": { "type": "object", "properties": {
                            "amount": Ty::Money.describe(), "category": { "type": "string" },
                            "description": { "type": "string" }, "driver_id": { "type": "integer" },
                            "employee_id": { "type": "integer" }, "paid_by": { "type": "string" },
                            "car_id": { "type": "integer" } }, "required": ["amount"] } }
                    },
                    "required": ["id", "parts"]
                }
            }));
        } else {
            out.insert(name.clone(), json!({ "error": "unknown kind" }));
        }
    }
    Value::Object(out)
}

pub(crate) fn int(params: &Value, key: &str) -> Result<i64, String> {
    params
        .get(key)
        .and_then(|v| v.as_i64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok())))
        .filter(|n| *n > 0)
        .ok_or_else(|| format!("`{key}` must be a positive integer"))
}

/// Validate a proposal's shape and build its request. Existence and the
/// before-state are checked by the tool, against each service's data.
pub fn plan(kind: &str, params: &Value) -> Result<Planned, String> {
    if let Some(k) = find(kind) {
        return plan_catalog(k, params);
    }
    if kind == "split_transaction" {
        return plan_split(params);
    }
    if let Some(p) = super::falcon_actions::plan_special(kind, params)? {
        return Ok(p);
    }
    if let Some(p) = maint::plan(kind, params)? {
        return Ok(p);
    }
    Err(format!(
        "unknown change kind `{kind}`; call describe_actions with one of: {}",
        kind_names().join(", ")
    ))
}

fn plan_catalog(k: &'static Kind, params: &Value) -> Result<Planned, String> {
    let empty = Map::new();
    let given = params.as_object().unwrap_or(&empty);
    let id = if k.needs_id() { Some(int(params, k.id_param)?) } else { None };

    let mut body = Map::new();
    for fl in k.fields {
        match given.get(fl.name) {
            None => {
                if fl.required {
                    return Err(format!("`{}` is required for {}", fl.name, k.name));
                }
            }
            Some(Value::Null) if fl.nullable => {
                body.insert(fl.name.into(), Value::Null);
            }
            Some(Value::Null) => {
                if fl.required {
                    return Err(format!("`{}` is required for {}", fl.name, k.name));
                }
            }
            Some(v) => {
                body.insert(fl.name.into(), fl.ty.coerce(fl.name, v)?);
            }
        }
    }
    let unknown: Vec<&String> = given
        .keys()
        .filter(|key| key.as_str() != k.id_param && !k.fields.iter().any(|fl| fl.name == key.as_str()))
        .collect();
    if !unknown.is_empty() {
        return Err(format!(
            "{} does not take {}; its fields are: {}",
            k.name,
            unknown.iter().map(|s| format!("`{s}`")).collect::<Vec<_>>().join(", "),
            k.fields.iter().map(|fl| fl.name).collect::<Vec<_>>().join(", ")
        ));
    }
    if k.method == "PATCH" && body.is_empty() {
        return Err(format!("{} needs at least one field to change", k.name));
    }
    for (key, val) in k.fixed {
        body.insert((*key).into(), json!(val));
    }
    if let (Some(key), Some(id)) = (k.id_in_body, id) {
        body.insert(key.into(), json!(id));
    }
    let body = match k.nest {
        Some(outer) => {
            let mut wrapped = Map::new();
            let mut inner = Map::new();
            for (key, val) in body {
                if k.top.contains(&key.as_str()) {
                    wrapped.insert(key, val);
                } else {
                    inner.insert(key, val);
                }
            }
            wrapped.insert(outer.into(), Value::Object(inner));
            wrapped
        }
        None => body,
    };

    let path = match id {
        Some(id) => k.path.replace("{id}", &id.to_string()),
        None => k.path.to_string(),
    };
    let record = match (k.source, id) {
        (Source::Falcon(table), Some(id)) => Record::Falcon { table, id },
        (Source::ApexTransaction, Some(id)) => Record::ApexTransaction { id },
        _ => Record::None,
    };
    let mut meta = json!({ "encoding": if k.form { "form" } else { "json" } });
    if !k.merge.is_empty() {
        meta["merge"] = json!(k.merge.iter().map(|(b, c)| json!([b, c])).collect::<Vec<_>>());
        meta["nest"] = json!(k.nest);
    }
    if matches!(record, Record::ApexTransaction { .. }) {
        // Filled with the row's version when the before-state is read.
        meta["if_match"] = Value::Null;
    }
    Ok(Planned {
        kind: k.name,
        target: k.target,
        method: k.method,
        path,
        body: Value::Object(body),
        meta,
        record,
    })
}

fn plan_split(params: &Value) -> Result<Planned, String> {
    let id = int(params, "id")?;
    let parts = params
        .get("parts")
        .and_then(Value::as_array)
        .filter(|p| p.len() >= 2)
        .ok_or("`parts` must be a list of at least 2 parts")?;
    let mut out = Vec::new();
    for (n, p) in parts.iter().enumerate() {
        let mut part = Map::new();
        let amount = p.get("amount").ok_or(format!("part {} needs an amount", n + 1))?;
        part.insert("amount".into(), Ty::Money.coerce("amount", amount)?);
        for (key, ty) in [
            ("category", Ty::Str),
            ("description", Ty::Str),
            ("paid_by", Ty::Str),
            ("driver_id", Ty::Int),
            ("employee_id", Ty::Int),
            ("car_id", Ty::Int),
        ] {
            if let Some(v) = p.get(key).filter(|v| !v.is_null()) {
                part.insert(key.into(), ty.coerce(key, v)?);
            }
        }
        out.push(Value::Object(part));
    }
    Ok(Planned {
        kind: "split_transaction",
        target: APEX,
        method: "POST",
        path: format!("/api/v1/transactions/{id}/split"),
        body: json!({ "parts": out }),
        meta: json!({ "encoding": "json", "if_match": null }),
        record: Record::ApexTransaction { id },
    })
}

/// maint-rust: every write is `POST /sync/push`, a batch of operations on an
/// allowlisted entity type, each guarded by the row's `sync_version`.
pub mod maint {
    use super::*;

    /// (entity_type, primary key is the vehicle id rather than a uuid)
    pub const ENTITIES: &[(&str, bool)] = &[
        ("work_orders", false),
        ("work_order_tasks", false),
        ("tires", false),
        ("tire_assignments", false),
        ("tire_status_events", false),
        ("oil_changes", false),
        ("chassis_layouts", false),
        ("chassis_axles", false),
        ("chassis_positions", false),
        ("vehicle_class_assignments", true),
        ("vehicle_odometer_overrides", true),
    ];

    pub const KINDS: &[(&str, &str)] = &[
        ("record_service", "Log a completed maintenance service on a truck in maint-rust: a closed work order with one done task per category (oil_change, oil_filter, air_filter, fuel_filter, brakes, coolant, transmission, battery, inspection, greasing, tires). This is what resets that item on the due board."),
        ("log_oil_change", "Record an oil change in maint-rust (oil type, liters, odometer); it debits oil stock. Liters outside 34–36 get flagged."),
        ("set_odometer", "Correct a truck's odometer in maint-rust (a manual reading that outranks older automatic ones)."),
        ("maint_record", "Create, edit or delete any maint-rust record (work_orders, work_order_tasks, tires, tire_assignments, tire_status_events, oil_changes, chassis_*, vehicle_class_assignments, vehicle_odometer_overrides) with its raw fields. Prefer the specific kinds when one fits."),
        ("upsert_service_plan", "Create or change a maintenance schedule rule for a vehicle class and category (every N km or N days)."),
        ("credit_tire_stock", "Add tires to stock (a delivery)."),
        ("credit_oil_stock", "Add oil to stock (a delivery)."),
    ];

    const CATEGORIES: &[&str] = &[
        "oil_change", "air_filter", "fuel_filter", "oil_filter", "brakes", "coolant",
        "transmission", "battery", "inspection", "greasing", "tires",
    ];

    pub fn describe(name: &str) -> Option<Value> {
        let what = KINDS.iter().find(|(n, _)| *n == name)?.1;
        let params = match name {
            "record_service" => json!({
                "type": "object",
                "properties": {
                    "car_id": { "type": "integer", "description": "the truck (FalconGo car id = maint vehicle_id)" },
                    "categories": { "type": "array", "items": { "type": "string", "enum": CATEGORIES } },
                    "performed_at": Ty::DateTime.describe(),
                    "odometer": { "type": "integer", "description": "km at the service" },
                    "title": { "type": "string" },
                    "notes": { "type": "string" }
                },
                "required": ["car_id", "categories", "performed_at", "odometer"]
            }),
            "log_oil_change" => json!({
                "type": "object",
                "properties": {
                    "car_id": { "type": "integer" },
                    "oil_type": { "type": "string" },
                    "liters": { "type": "number" },
                    "odometer": { "type": "integer" },
                    "performed_at": Ty::DateTime.describe(),
                    "work_order_id": { "type": "string", "description": "uuid of a work order to attach it to, optional" },
                    "notes": { "type": "string" }
                },
                "required": ["car_id", "oil_type", "liters", "odometer", "performed_at"]
            }),
            "set_odometer" => json!({
                "type": "object",
                "properties": {
                    "car_id": { "type": "integer" },
                    "odometer": { "type": "integer", "description": "the correct km now" },
                    "notes": { "type": "string", "description": "why" }
                },
                "required": ["car_id", "odometer", "notes"]
            }),
            "maint_record" => json!({
                "type": "object",
                "properties": {
                    "entity_type": { "type": "string", "enum": ENTITIES.iter().map(|(e, _)| *e).collect::<Vec<_>>() },
                    "operation": { "type": "string", "enum": ["insert", "update", "delete"] },
                    "entity_id": { "type": "string", "description": "uuid; for vehicle_class_assignments / vehicle_odometer_overrides the vehicle (car) id. Omit on insert to get a new uuid." },
                    "fields": { "type": "object", "description": "column values; on update only what changes" }
                },
                "required": ["entity_type", "operation"]
            }),
            "upsert_service_plan" => json!({
                "type": "object",
                "properties": {
                    "class_id": { "type": "string", "description": "vehicle class, e.g. tractor, tanker_trailer" },
                    "category_id": { "type": "string", "enum": CATEGORIES },
                    "trigger_type": { "type": "string", "enum": ["mileage", "time"] },
                    "interval_km": { "type": "integer", "description": "with mileage" },
                    "interval_days": { "type": "integer", "description": "with time" },
                    "lead_warn_km": { "type": "integer" },
                    "lead_warn_days": { "type": "integer" },
                    "is_active": { "type": "boolean" }
                },
                "required": ["class_id", "category_id", "trigger_type"]
            }),
            "credit_tire_stock" => json!({
                "type": "object",
                "properties": {
                    "brand": { "type": "string" }, "model": { "type": "string" },
                    "size": { "type": "string" }, "qty": { "type": "integer" }
                },
                "required": ["brand", "qty"]
            }),
            "credit_oil_stock" => json!({
                "type": "object",
                "properties": { "oil_type": { "type": "string" }, "liters": { "type": "number" } },
                "required": ["oil_type", "liters"]
            }),
            _ => return None,
        };
        Some(json!({ "service": "maint", "what": what, "params": params }))
    }

    fn op(entity_type: &str, entity_id: &str, operation: &str, payload: Value) -> Value {
        json!({
            "entity_type": entity_type,
            "entity_id": entity_id,
            "operation": operation,
            "payload": payload,
            // 0 for an insert; update/delete get the row's version filled in
            // when the before-state is read.
            "sync_version": 0,
        })
    }

    fn push(kind: &'static str, ops: Vec<Value>, record: Record) -> Planned {
        Planned {
            kind,
            target: Target::Maint,
            method: "POST",
            path: "/sync/push".into(),
            body: json!({ "operations": ops }),
            meta: json!({ "encoding": "json", "check": "sync_push" }),
            record,
        }
    }

    fn text(params: &Value, key: &str) -> Option<String> {
        params
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    }

    fn need_text(params: &Value, key: &str) -> Result<String, String> {
        text(params, key).ok_or_else(|| format!("`{key}` is required"))
    }

    fn num(params: &Value, key: &str) -> Result<f64, String> {
        params
            .get(key)
            .and_then(|v| v.as_f64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok())))
            .filter(|n| n.is_finite() && *n > 0.0)
            .ok_or_else(|| format!("`{key}` must be a positive number"))
    }

    fn when(params: &Value, key: &str) -> Result<String, String> {
        Ty::DateTime
            .coerce(key, params.get(key).ok_or(format!("`{key}` is required"))?)
            .map(|v| v.as_str().unwrap_or_default().to_string())
    }

    /// `Ok(None)`: not a maintenance kind.
    pub fn plan(kind: &str, params: &Value) -> Result<Option<Planned>, String> {
        let planned = match kind {
            "record_service" => {
                let car = int(params, "car_id")?;
                let at = when(params, "performed_at")?;
                let odometer = int(params, "odometer")?;
                let cats: Vec<String> = params
                    .get("categories")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(|c| c.as_str().map(str::to_string)).collect())
                    .unwrap_or_default();
                if cats.is_empty() {
                    return Err("`categories` needs at least one category".into());
                }
                if let Some(bad) = cats.iter().find(|c| !CATEGORIES.contains(&c.as_str())) {
                    return Err(format!("`{bad}` is not a maintenance category; use one of {}", CATEGORIES.join(", ")));
                }
                let wo = Uuid::new_v4().to_string();
                let mut wo_payload = json!({
                    "vehicle_id": car,
                    "status": "closed",
                    "title": text(params, "title").unwrap_or_else(|| cats.join(", ")),
                    "odometer_at_open": odometer,
                    "opened_at": at,
                    "closed_at": at,
                });
                if let Some(n) = text(params, "notes") {
                    wo_payload["notes_en"] = json!(n);
                }
                let mut ops = vec![op("work_orders", &wo, "insert", wo_payload)];
                for (i, c) in cats.iter().enumerate() {
                    ops.push(op(
                        "work_order_tasks",
                        &Uuid::new_v4().to_string(),
                        "insert",
                        json!({
                            "work_order_id": wo,
                            "kind": if c == "oil_change" { "oil" } else if c == "tires" { "tire" } else { "free" },
                            "category_id": c,
                            "description_en": c.replace('_', " "),
                            "is_done": true,
                            "sort_order": i,
                        }),
                    ));
                }
                push("record_service", ops, Record::None)
            }
            "log_oil_change" => {
                let car = int(params, "car_id")?;
                let mut payload = json!({
                    "vehicle_id": car,
                    "oil_type": need_text(params, "oil_type")?,
                    "liters": num(params, "liters")?,
                    "odometer": int(params, "odometer")?,
                    "performed_at": when(params, "performed_at")?,
                });
                if let Some(w) = text(params, "work_order_id") {
                    Uuid::parse_str(&w).map_err(|_| "`work_order_id` must be a uuid".to_string())?;
                    payload["work_order_id"] = json!(w);
                }
                if let Some(n) = text(params, "notes") {
                    payload["notes"] = json!(n);
                }
                let ops = vec![op("oil_changes", &Uuid::new_v4().to_string(), "insert", payload)];
                push("log_oil_change", ops, Record::None)
            }
            "set_odometer" => {
                let car = int(params, "car_id")?;
                let payload = json!({
                    "odometer": int(params, "odometer")?,
                    "notes": need_text(params, "notes")?,
                    // superseded_km is filled from Falcon's readings when the
                    // before-state is read: the readings this override outranks.
                    "superseded_km": 0,
                    "set_at": chrono::Utc::now().to_rfc3339(),
                });
                let ops = vec![op("vehicle_odometer_overrides", &car.to_string(), "insert", payload)];
                push(
                    "set_odometer",
                    ops,
                    Record::Maint { entity_type: "vehicle_odometer_overrides".into(), entity_id: car.to_string() },
                )
            }
            "maint_record" => {
                let entity = need_text(params, "entity_type")?;
                let by_vehicle = ENTITIES
                    .iter()
                    .find(|(e, _)| *e == entity)
                    .map(|(_, v)| *v)
                    .ok_or_else(|| format!("`{entity}` is not a maint-rust entity type"))?;
                let operation = need_text(params, "operation")?;
                if !["insert", "update", "delete"].contains(&operation.as_str()) {
                    return Err("`operation` must be insert, update or delete".into());
                }
                let fields = params.get("fields").cloned().unwrap_or_else(|| json!({}));
                if !fields.is_object() {
                    return Err("`fields` must be an object".into());
                }
                if operation != "delete" && fields.as_object().is_some_and(Map::is_empty) {
                    return Err(format!("{operation} needs `fields`"));
                }
                let id = match text(params, "entity_id") {
                    Some(id) if by_vehicle => {
                        id.parse::<i64>().map_err(|_| "`entity_id` must be the vehicle id here".to_string())?;
                        id
                    }
                    Some(id) => {
                        Uuid::parse_str(&id).map_err(|_| "`entity_id` must be a uuid".to_string())?;
                        id
                    }
                    None if operation == "insert" && !by_vehicle => Uuid::new_v4().to_string(),
                    None => return Err("`entity_id` is required".into()),
                };
                let record = if operation == "insert" && !by_vehicle {
                    Record::None
                } else {
                    Record::Maint { entity_type: entity.clone(), entity_id: id.clone() }
                };
                push("maint_record", vec![op(&entity, &id, &operation, fields)], record)
            }
            "upsert_service_plan" => {
                let trigger = need_text(params, "trigger_type")?;
                let mut body = json!({
                    "class_id": need_text(params, "class_id")?,
                    "category_id": need_text(params, "category_id")?,
                    "trigger_type": trigger,
                });
                match trigger.as_str() {
                    "mileage" => body["interval_km"] = json!(int(params, "interval_km")?),
                    "time" => body["interval_days"] = json!(int(params, "interval_days")?),
                    _ => return Err("`trigger_type` must be mileage or time".into()),
                }
                for key in ["lead_warn_km", "lead_warn_days"] {
                    if params.get(key).is_some_and(|v| !v.is_null()) {
                        body[key] = json!(int(params, key)?);
                    }
                }
                if let Some(b) = params.get("is_active").and_then(Value::as_bool) {
                    body["is_active"] = json!(b);
                }
                Planned {
                    kind: "upsert_service_plan",
                    target: Target::Maint,
                    method: "POST",
                    path: "/service-plans".into(),
                    body,
                    meta: json!({ "encoding": "json" }),
                    record: Record::None,
                }
            }
            "credit_tire_stock" => {
                let qty = int(params, "qty")?;
                let mut body = json!({
                    "brand": need_text(params, "brand")?,
                    "qty": qty,
                    // Fixed at proposal time: approving twice credits once.
                    "ref_id": Uuid::new_v4().to_string(),
                });
                for key in ["model", "size"] {
                    if let Some(v) = text(params, key) {
                        body[key] = json!(v);
                    }
                }
                Planned {
                    kind: "credit_tire_stock",
                    target: Target::Maint,
                    method: "POST",
                    path: "/stock/tires/credit".into(),
                    body,
                    meta: json!({ "encoding": "json" }),
                    record: Record::None,
                }
            }
            "credit_oil_stock" => Planned {
                kind: "credit_oil_stock",
                target: Target::Maint,
                method: "POST",
                path: "/stock/oil/credit".into(),
                body: json!({
                    "oil_type": need_text(params, "oil_type")?,
                    "liters": num(params, "liters")?,
                    "ref_id": Uuid::new_v4().to_string(),
                }),
                meta: json!({ "encoding": "json" }),
                record: Record::None,
            },
            _ => return Ok(None),
        };
        Ok(Some(planned))
    }

    /// Put the current row's version on every operation that touches it.
    pub fn set_version(planned: &mut Planned, entity_id: &str, version: i64) {
        if let Some(ops) = planned.body.get_mut("operations").and_then(Value::as_array_mut) {
            for o in ops {
                if o["entity_id"] == entity_id && o["operation"] != "insert" {
                    o["sync_version"] = json!(version);
                }
            }
        }
    }

    /// maint-rust answers a batch with 200 and a result per operation; the
    /// change is applied only if every one says so.
    pub fn all_applied(body: &Value) -> Result<(), String> {
        let results = body
            .get("results")
            .and_then(Value::as_array)
            .ok_or("maint-rust returned no per-operation results")?;
        match results.iter().find(|r| r["status"] != "applied") {
            None if !results.is_empty() => Ok(()),
            None => Err("maint-rust applied nothing".into()),
            Some(r) => Err(match r["status"].as_str() {
                Some("conflict") => "the record changed in maint-rust after this was proposed; nothing was applied".into(),
                _ => format!(
                    "maint-rust refused an operation: {}",
                    r["message"].as_str().unwrap_or("unknown error")
                ),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_name_is_unique_and_described() {
        let names = kind_names();
        let mut sorted = names.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len(), "duplicate kind names");
        let all: Vec<String> = names.iter().map(|s| s.to_string()).collect();
        let d = describe(&all);
        for n in &names {
            assert!(d[*n].get("error").is_none(), "{n} has no description");
            assert_eq!(d[*n]["params"]["type"], "object", "{n}");
        }
    }

    #[test]
    fn catalog_paths_take_an_id_only_when_they_need_one() {
        for k in catalog() {
            if k.path.contains("{id}") {
                assert!(k.needs_id(), "{}", k.name);
            }
            assert!(["POST", "PUT", "PATCH", "DELETE"].contains(&k.method), "{}", k.name);
        }
    }

    #[test]
    fn fields_are_typed_and_unknown_ones_are_refused() {
        let p = plan(
            "create_transaction",
            &json!({ "direction": "out", "amount": 1250.5, "occurred_at": "2026-10-25T08:00:00+03:00", "category": " Fuel " }),
        )
        .unwrap();
        assert_eq!(p.body["amount"], "1250.5");
        assert_eq!(p.body["category"], "Fuel");
        assert_eq!(p.record, Record::None);
        assert!(plan("create_transaction", &json!({ "direction": "sideways", "amount": "1", "occurred_at": "2026-10-25T08:00:00Z" })).is_err());
        assert!(plan("create_transaction", &json!({ "direction": "out", "amount": "1", "occurred_at": "2026-10-25T08:00:00Z", "password": "x" })).is_err());
    }

    #[test]
    fn edits_need_a_change_carry_nulls_and_ask_for_the_version() {
        assert!(plan("edit_transaction", &json!({ "id": 9 })).is_err());
        let p = plan("edit_transaction", &json!({ "id": 9, "driver_id": null, "category": "Tolls" })).unwrap();
        assert_eq!(p.path, "/api/v1/transactions/9");
        assert_eq!(p.body, json!({ "driver_id": null, "category": "Tolls" }));
        assert_eq!(p.record, Record::ApexTransaction { id: 9 });
        assert!(p.meta.get("if_match").is_some());
    }

    #[test]
    fn a_service_record_is_a_closed_work_order_with_done_tasks() {
        let p = plan(
            "record_service",
            &json!({ "car_id": 12, "categories": ["oil_change", "air_filter"], "performed_at": "2026-10-20T09:00:00Z", "odometer": 412000 }),
        )
        .unwrap();
        let ops = p.body["operations"].as_array().unwrap();
        assert_eq!(ops.len(), 3);
        assert_eq!(ops[0]["entity_type"], "work_orders");
        assert_eq!(ops[0]["payload"]["status"], "closed");
        assert_eq!(ops[1]["payload"]["work_order_id"], ops[0]["entity_id"]);
        assert_eq!(ops[1]["payload"]["is_done"], true);
        assert_eq!(p.meta["check"], "sync_push");
        assert!(plan("record_service", &json!({ "car_id": 12, "categories": ["paint"], "performed_at": "2026-10-20T09:00:00Z", "odometer": 1 })).is_err());
    }

    #[test]
    fn maint_updates_carry_the_row_version() {
        let id = Uuid::new_v4().to_string();
        let mut p = plan(
            "maint_record",
            &json!({ "entity_type": "oil_changes", "operation": "update", "entity_id": id, "fields": { "liters": 35 } }),
        )
        .unwrap();
        maint::set_version(&mut p, &id, 4);
        assert_eq!(p.body["operations"][0]["sync_version"], 4);
        assert!(plan("maint_record", &json!({ "entity_type": "users", "operation": "insert", "fields": {"a": 1} })).is_err());
    }

    #[test]
    fn a_batch_is_applied_only_if_every_operation_is() {
        assert!(maint::all_applied(&json!({ "results": [{ "status": "applied" }, { "status": "applied" }] })).is_ok());
        assert!(maint::all_applied(&json!({ "results": [{ "status": "applied" }, { "status": "conflict" }] })).is_err());
        assert!(maint::all_applied(&json!({ "results": [] })).is_err());
        assert!(maint::all_applied(&json!({})).is_err());
    }

    #[test]
    fn unknown_kinds_are_refused() {
        assert!(plan("delete_car_forever", &json!({ "id": 1 })).is_err());
    }
}
