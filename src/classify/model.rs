//! Everything the rules look at, loaded in one go. A few thousand rows: the
//! rules run in memory, and the database only sees the result.

use chrono::{DateTime, NaiveDate, Utc};
use chrono_tz::Africa::Cairo;
use rust_decimal::Decimal;
use sqlx::{PgPool, Row};
use std::collections::{HashMap, HashSet};

use super::fold::fold;
use crate::errors::AppResult;
use crate::parser::{self, normalize::normalize};

#[derive(Debug, Clone)]
pub struct Txn {
    pub id: i64,
    pub direction: String,
    pub amount: Decimal,
    pub occurred_at: DateTime<Utc>,
    pub account: Option<String>,
    pub counterparty: Option<String>,
    /// `fold(counterparty)`.
    pub key: Option<String>,
    pub reference: Option<String>,
    /// The template that parsed the message, for bank rows.
    pub template: Option<String>,
    /// The message, normalized.
    pub body: Option<String>,
    pub category: Option<String>,
    pub driver_id: Option<i64>,
    pub employee_id: Option<i64>,
    pub loan_id: Option<i64>,
    pub linked_loan_id: Option<i64>,
    pub deleted: bool,
    /// A split parent: its money left the ledger for its parts.
    pub split: bool,
    /// A split part.
    pub child: bool,
    pub version: i32,
    /// The classification status, if there is one.
    pub status: Option<String>,
    /// When a person last saved it (or created it, for manual rows).
    pub decided_at: Option<DateTime<Utc>>,
}

impl Txn {
    pub fn is_out(&self) -> bool {
        self.direction == "out"
    }
    pub fn cairo_date(&self) -> NaiveDate {
        self.occurred_at.with_timezone(&Cairo).date_naive()
    }
    pub fn has_party(&self) -> bool {
        self.driver_id.is_some() || self.employee_id.is_some()
    }
    /// In the ledger at all: not deleted, not a split container.
    pub fn live(&self) -> bool {
        !self.deleted && !self.split
    }
    /// The classifier may decide this one: live, not a part, nothing a
    /// person (or an earlier run) has settled, not tied to a loan.
    pub fn open(&self) -> bool {
        self.live()
            && !self.child
            && self.category.is_none()
            && self.loan_id.is_none()
            && self.linked_loan_id.is_none()
            && matches!(self.status.as_deref(), None | Some("suggested"))
    }
    /// A category a person chose (or kept): the evidence the classifier
    /// learns from. Its own unreviewed output is not evidence.
    pub fn human_category(&self) -> Option<&str> {
        if self.deleted || self.split || self.status.as_deref() == Some("applied") {
            return None;
        }
        self.category.as_deref()
    }
}

#[derive(Debug, Clone)]
pub struct Payee {
    pub id: i64,
    pub name: String,
    pub kind: String,
    pub category: Option<String>,
    pub always: bool,
    pub driver_id: Option<i64>,
    pub employee_id: Option<i64>,
    /// When the label was last set.
    pub labelled_at: Option<DateTime<Utc>>,
}

impl Payee {
    /// The owner or family: money to and from them stays in the household
    /// of the business.
    pub fn is_own(&self) -> bool {
        matches!(self.kind.as_str(), "owner" | "family")
    }
}

#[derive(Debug, Clone)]
pub struct Loan {
    pub id: i64,
    pub driver_id: Option<i64>,
    pub employee_id: Option<i64>,
    pub amount: Decimal,
    pub date: NaiveDate,
    pub kind: String,
}

#[derive(Debug, Clone)]
pub struct Person {
    pub id: i64,
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct CategoryInfo {
    pub key: String,
    pub posting_kind: Option<String>,
    pub required_party: String,
    pub report_group: String,
}

impl CategoryInfo {
    pub fn needs_party(&self) -> bool {
        self.required_party != "none"
    }
    pub fn posts(&self) -> bool {
        self.posting_kind.is_some()
    }
}

#[derive(Debug, Default)]
pub struct Model {
    pub txns: Vec<Txn>,
    pub payees: HashMap<i64, Payee>,
    /// Folded spelling → payee.
    pub aliases: HashMap<String, i64>,
    pub drivers: Vec<Person>,
    pub employees: Vec<Person>,
    /// Loans entered in FalconGo that no bank payment represents yet.
    pub open_loans: Vec<Loan>,
    /// By key, lower-cased (keys are matched case-insensitively everywhere).
    pub categories: HashMap<String, CategoryInfo>,
    /// Accounts that have sent PetroApp top-ups (from suppressed messages).
    pub petro_accounts: HashSet<String>,
    /// Per rule: (accepted, corrected) decisions people made.
    pub rule_record: HashMap<String, (u32, u32)>,
}

impl Model {
    pub fn category(&self, key: &str) -> Option<&CategoryInfo> {
        self.categories.get(&key.to_lowercase())
    }
    pub fn payee_for(&self, t: &Txn) -> Option<&Payee> {
        t.key
            .as_ref()
            .and_then(|k| self.aliases.get(k))
            .and_then(|id| self.payees.get(id))
    }
    pub fn person_name(&self, driver_id: Option<i64>, employee_id: Option<i64>) -> Option<&str> {
        if let Some(d) = driver_id {
            return self
                .drivers
                .iter()
                .find(|p| p.id == d)
                .map(|p| p.name.as_str());
        }
        employee_id.and_then(|e| {
            self.employees
                .iter()
                .find(|p| p.id == e)
                .map(|p| p.name.as_str())
        })
    }

    pub async fn load(pool: &PgPool) -> AppResult<Model> {
        let mut m = Model::default();

        for r in sqlx::query(
            "SELECT t.id, t.direction, t.amount, t.occurred_at, t.account, t.counterparty,
                    t.reference, t.category, t.driver_id, t.employee_id, t.loan_id,
                    t.linked_loan_id, t.version,
                    COALESCE(t.edited_at, CASE WHEN t.source = 'manual' THEN t.created_at END) AS decided_at,
                    (t.deleted_at IS NOT NULL) AS deleted, (t.split_at IS NOT NULL) AS split,
                    (t.parent_id IS NOT NULL) AS child,
                    r.template, r.body, c.status
             FROM banksms.transactions t
             LEFT JOIN banksms.raw_messages r ON r.id = t.raw_message_id
             LEFT JOIN banksms.classifications c ON c.transaction_id = t.id
             ORDER BY t.occurred_at, t.id",
        )
        .fetch_all(pool)
        .await?
        {
            let counterparty: Option<String> = r
                .get::<Option<String>, _>("counterparty")
                .filter(|s| !s.trim().is_empty());
            m.txns.push(Txn {
                id: r.get("id"),
                direction: r.get("direction"),
                amount: r.get("amount"),
                occurred_at: r.get("occurred_at"),
                account: r.get::<Option<String>, _>("account").filter(|s| !s.is_empty()),
                key: counterparty.as_deref().map(fold),
                counterparty,
                reference: r.get::<Option<String>, _>("reference").filter(|s| !s.is_empty()),
                template: r.get("template"),
                body: r.get::<Option<String>, _>("body").map(|b| normalize(&b)),
                category: r.get::<Option<String>, _>("category").filter(|s| !s.is_empty()),
                driver_id: r.get("driver_id"),
                employee_id: r.get("employee_id"),
                loan_id: r.get("loan_id"),
                linked_loan_id: r.get("linked_loan_id"),
                deleted: r.get("deleted"),
                split: r.get("split"),
                child: r.get("child"),
                version: r.get("version"),
                status: r.get("status"),
                decided_at: r.get("decided_at"),
            });
        }

        for r in sqlx::query(
            "SELECT id, name, kind, category, consistency, driver_id, employee_id, updated_at
             FROM banksms.payees",
        )
        .fetch_all(pool)
        .await?
        {
            let id: i64 = r.get("id");
            m.payees.insert(
                id,
                Payee {
                    id,
                    name: r.get("name"),
                    kind: r.get("kind"),
                    category: r
                        .get::<Option<String>, _>("category")
                        .filter(|s| !s.is_empty()),
                    always: r.get::<String, _>("consistency") == "always",
                    driver_id: r.get("driver_id"),
                    employee_id: r.get("employee_id"),
                    labelled_at: r.get("updated_at"),
                },
            );
        }
        for r in sqlx::query("SELECT key, payee_id FROM banksms.payee_aliases")
            .fetch_all(pool)
            .await?
        {
            m.aliases.insert(r.get("key"), r.get("payee_id"));
        }

        for r in sqlx::query(
            "SELECT id::bigint AS id, COALESCE(name, '') AS name FROM public.drivers
             WHERE deleted_at IS NULL AND COALESCE(name, '') <> ''",
        )
        .fetch_all(pool)
        .await?
        {
            m.drivers.push(Person {
                id: r.get("id"),
                name: r.get("name"),
            });
        }
        for r in sqlx::query(
            "SELECT id::bigint AS id, COALESCE(name, '') AS name FROM public.employees
             WHERE deleted_at IS NULL AND COALESCE(name, '') <> ''",
        )
        .fetch_all(pool)
        .await?
        {
            m.employees.push(Person {
                id: r.get("id"),
                name: r.get("name"),
            });
        }

        // Loans entered in FalconGo that are not banksms's own and that no
        // live bank row represents yet. Salary rows are payroll visibility
        // entries, not money that left.
        for r in sqlx::query(
            r#"
            SELECT l.id::bigint AS id, l.driver_id::bigint AS driver_id,
                   l.employee_id::bigint AS employee_id, l.amount::numeric AS amount,
                   l.date, COALESCE(l.kind, '') AS kind
            FROM public.loans l
            WHERE l.deleted_at IS NULL
              AND COALESCE(l.method, '') <> 'banksms'
              AND COALESCE(l.kind, '') <> 'salary'
              AND l.amount > 0
              AND l.date ~ '^\d{4}-\d{2}-\d{2}$'
              AND NOT EXISTS (
                  SELECT 1 FROM banksms.transactions t
                  WHERE t.deleted_at IS NULL AND (t.linked_loan_id = l.id OR t.loan_id = l.id))
            "#,
        )
        .fetch_all(pool)
        .await?
        {
            let date: String = r.get("date");
            let Ok(date) = NaiveDate::parse_from_str(&date, "%Y-%m-%d") else {
                continue;
            };
            m.open_loans.push(Loan {
                id: r.get("id"),
                driver_id: r.get("driver_id"),
                employee_id: r.get("employee_id"),
                amount: r.get("amount"),
                date,
                kind: r.get("kind"),
            });
        }

        for r in sqlx::query(
            "SELECT key, posting_kind, required_party, report_group
             FROM banksms.categories WHERE enabled",
        )
        .fetch_all(pool)
        .await?
        {
            let key: String = r.get("key");
            m.categories.insert(
                key.to_lowercase(),
                CategoryInfo {
                    key,
                    posting_kind: r.get("posting_kind"),
                    required_party: r.get("required_party"),
                    report_group: r.get("report_group"),
                },
            );
        }

        // Which accounts top up PetroApp: the suppressed messages say so, and
        // their template still knows where the account number sits.
        let templates = parser::templates::load(pool).await?;
        for r in sqlx::query(
            "SELECT body, template FROM banksms.raw_messages WHERE status = 'suppressed'",
        )
        .fetch_all(pool)
        .await?
        {
            let body: String = r.get("body");
            let template: Option<String> = r.get("template");
            let Some(t) = templates
                .iter()
                .find(|t| Some(&t.name) == template.as_ref())
            else {
                continue;
            };
            if let Some(account) = t
                .regex
                .captures(&normalize(&body))
                .and_then(|c| c.name("account").map(|a| a.as_str().to_string()))
            {
                m.petro_accounts.insert(account);
            }
        }

        for r in sqlx::query(
            "SELECT rule, status, COUNT(*)::bigint AS n FROM banksms.classifications
             WHERE status IN ('accepted', 'corrected') GROUP BY rule, status",
        )
        .fetch_all(pool)
        .await?
        {
            let rule: String = r.get("rule");
            let n = r.get::<i64, _>("n") as u32;
            let e = m.rule_record.entry(rule).or_default();
            if r.get::<String, _>("status") == "accepted" {
                e.0 += n;
            } else {
                e.1 += n;
            }
        }

        Ok(m)
    }
}
