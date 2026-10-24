//! The rules. Pure: a `Model` in, decisions out — no I/O, so every rule is
//! testable on a hand-built model.
//!
//! Order matters and runs from facts to inference:
//!
//! 1. Pairs the bank itself tells us about: refunds and reversals with the
//!    transfer they undo, and money leaving one company account to arrive in
//!    another minutes later.
//! 2. Advances already in FalconGo's loans list, matched to the payment that
//!    paid them, so they count once.
//! 3. One-row facts: cash withdrawals, the separate IPN fee, money in from
//!    the owner, a transfer to someone who pays the company's loans that is
//!    an installment falling due.
//! 4. What people have said: payee labels, then the categories they gave
//!    earlier payments to the same name.
//! 5. Weak hints, never applied: PetroApp-shaped transfers, merchant names,
//!    a masked name that fits one driver.
//!
//! A decision at 95% or more is applied; anything less waits for review.

use chrono::{Duration, NaiveDate};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use std::collections::{BTreeMap, HashMap, HashSet};

use super::fold::{has_arabic, is_masked, mask_overlap};
use super::model::{Loan, Model, Payee, Txn};

/// The bar for writing a decision without asking.
pub const APPLY_AT: f64 = 0.95;

const REFUND_TEMPLATES: [&str; 3] = ["ipn_refund", "abk_reversal", "instant_reversal"];

#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    pub txn_id: i64,
    pub rule: &'static str,
    pub confidence: f64,
    pub reason: String,
    pub category: Option<String>,
    pub driver_id: Option<i64>,
    pub employee_id: Option<i64>,
    pub linked_loan_id: Option<i64>,
    pub payee_id: Option<i64>,
    pub pair_id: Option<i64>,
    /// Write it to the transaction (set by `finalize`).
    pub apply: bool,
    /// Even if the category waits for review, the person is certain (a
    /// label names them): fill it in.
    pub fill_party: bool,
    /// Replaces the classifier's own earlier, unreviewed decision (a
    /// transfer it categorised turned out to be refunded).
    pub revise: bool,
}

impl Decision {
    fn new(t: &Txn, rule: &'static str, confidence: f64, reason: String) -> Decision {
        Decision {
            txn_id: t.id,
            rule,
            confidence,
            reason,
            category: None,
            driver_id: None,
            employee_id: None,
            linked_loan_id: None,
            payee_id: None,
            pair_id: None,
            apply: false,
            fill_party: false,
            revise: false,
        }
    }
    fn category(mut self, c: &str) -> Self {
        self.category = Some(c.to_string());
        self
    }
    fn party(mut self, driver_id: Option<i64>, employee_id: Option<i64>) -> Self {
        self.driver_id = driver_id;
        self.employee_id = if driver_id.is_some() {
            None
        } else {
            employee_id
        };
        self
    }
    fn payee(mut self, p: &Payee) -> Self {
        self.payee_id = Some(p.id);
        self
    }
    fn pair(mut self, id: i64) -> Self {
        self.pair_id = Some(id);
        self
    }
}

/// Decide what can be decided for `targets` (transaction ids; `None` means
/// every open transaction). Partners outside `targets` — the earlier half of
/// a pair, the transfer a refund undoes — are decided too when they are open.
pub fn decide(model: &Model, targets: Option<&HashSet<i64>>) -> Vec<Decision> {
    let by_id: HashMap<i64, &Txn> = model.txns.iter().map(|t| (t.id, t)).collect();
    let in_scope = |id: i64| targets.map_or(true, |s| s.contains(&id));
    let wanted = |t: &Txn| t.open() && in_scope(t.id);
    let mut out: BTreeMap<i64, Decision> = BTreeMap::new();

    // 1. Pairs.
    for d in refunds(model) {
        if in_scope(d.txn_id) || d.pair_id.is_some_and(in_scope) {
            out.entry(d.txn_id).or_insert(d);
        }
    }
    for (a, b) in internal_pairs(model, &out) {
        let (ta, tb) = (by_id[&a.txn_id], by_id[&b.txn_id]);
        if wanted(ta) || wanted(tb) {
            if ta.open() {
                out.entry(a.txn_id).or_insert(a);
            }
            if tb.open() {
                out.entry(b.txn_id).or_insert(b);
            }
        }
    }

    // 2. Advances already in the loans list.
    let candidates: Vec<&Txn> = model
        .txns
        .iter()
        .filter(|t| wanted(t) && t.is_out() && !out.contains_key(&t.id))
        .collect();
    for d in advance_links(model, &candidates) {
        out.entry(d.txn_id).or_insert(d);
    }

    // 3-5. One row at a time.
    let history = History::build(model);
    let fits = loan_fits(model, &out);
    for t in model.txns.iter().filter(|t| wanted(t)) {
        if out.contains_key(&t.id) {
            continue;
        }
        if let Some(d) = single(model, &history, &fits, t) {
            out.insert(t.id, d);
        }
    }

    out.into_values()
        .filter_map(|d| finalize(model, &by_id, d))
        .collect()
}

/// The last word on whether a decision is written: confident enough, a rule
/// people have not had to correct, a category that exists and gets what it
/// requires — and never a loan created behind anyone's back.
fn finalize(model: &Model, by_id: &HashMap<i64, &Txn>, mut d: Decision) -> Option<Decision> {
    let t = by_id[&d.txn_id];
    if let Some(key) = d.category.clone() {
        match model.category(&key) {
            Some(c) => d.category = Some(c.key.clone()),
            None => d.category = None, // not a category (any more): offer nothing
        }
    }
    // The person already on the transaction stands; a decision never moves
    // money to someone else.
    if t.has_party()
        && (d.driver_id.is_some() || d.employee_id.is_some())
        && (d.driver_id, d.employee_id) != (t.driver_id, t.employee_id)
    {
        d.driver_id = t.driver_id;
        d.employee_id = t.employee_id;
        d.fill_party = false;
    }
    if d.category.is_none() && d.driver_id.is_none() && d.employee_id.is_none() {
        return None;
    }
    let mut apply = d.confidence >= APPLY_AT && rule_trusted(model, d.rule) && d.category.is_some();
    if let Some(c) = d.category.as_deref().and_then(|k| model.category(k)) {
        let has_party = d.driver_id.is_some() || d.employee_id.is_some() || t.has_party();
        if c.needs_party() && !has_party {
            apply = false;
        }
        // Categorising into Advance/Loan registers a loan; the classifier
        // only ever points at loans that already exist.
        if c.posts() && d.linked_loan_id.is_none() {
            apply = false;
        }
    }
    // A category someone already gave stays theirs; this can only be a
    // suggestion (a refund notice for a transfer they categorised). Only the
    // classifier's own unreviewed decision may be replaced.
    if t.category.is_some() && !(d.revise && t.status.as_deref() == Some("applied")) {
        apply = false;
    }
    d.apply = apply;
    Some(d)
}

/// A rule people have corrected must earn its automatic say back: its
/// accepted decisions have to put it at 95% again. Labels and history are
/// judged per payee instead — a correction is itself new history, and one
/// payee's correction says nothing about another's.
fn rule_trusted(model: &Model, rule: &str) -> bool {
    if matches!(rule, "payee" | "history") {
        return true;
    }
    match model.rule_record.get(rule) {
        None | Some((_, 0)) => true,
        Some(&(accepted, corrected)) => succession(accepted, accepted + corrected) >= APPLY_AT,
    }
}

/// Laplace's rule of succession: after `k` of `n` agreed, the chance the
/// next one does.
pub fn succession(k: u32, n: u32) -> f64 {
    (k as f64 + 1.0) / (n as f64 + 2.0)
}

/* ----------------------------- 1. pairs ---------------------------------- */

fn refunds(model: &Model) -> Vec<Decision> {
    let mut out = Vec::new();
    for t in model.txns.iter().filter(|t| {
        t.live()
            && !t.is_out()
            && t.template
                .as_deref()
                .is_some_and(|n| REFUND_TEMPLATES.contains(&n))
    }) {
        let original = t.reference.as_ref().and_then(|r| {
            model
                .txns
                .iter()
                .filter(|o| {
                    o.is_out()
                        && o.reference.as_ref() == Some(r)
                        && o.amount == t.amount
                        && o.occurred_at <= t.occurred_at + Duration::days(1)
                        && o.occurred_at >= t.occurred_at - Duration::days(30)
                })
                .max_by_key(|o| o.occurred_at)
        });
        let when = fmt_date(t);
        match original {
            Some(o) => {
                if t.open() {
                    out.push(
                        Decision::new(t, "refund", 0.99, format!(
                            "The bank returned the {} transfer of {} (ref {}). It and the original cancel out.",
                            fmt_date(o), money(t.amount), o.reference.as_deref().unwrap_or("")
                        ))
                        .category("Reversed")
                        .pair(o.id),
                    );
                }
                // Open, or categorised without the refund known: by a person
                // directly (offered as a suggestion), or by the classifier
                // itself (replaced). A person's verdict on a suggestion
                // stands.
                let undecided = o.open()
                    || o.category.is_some()
                        && o.category.as_deref() != Some("Reversed")
                        && matches!(o.status.as_deref(), None | Some("suggested") | Some("applied"));
                if o.live() && !o.child && undecided {
                    let mut d = Decision::new(o, "refunded", 0.99, format!(
                        "The bank returned this transfer on {when}; the money came back."
                    ))
                    .category("Reversed")
                    .pair(t.id);
                    d.revise = o.status.as_deref() == Some("applied");
                    out.push(d);
                }
            }
            None if t.open() => out.push(
                Decision::new(t, "refund", 0.97, format!(
                    "A refund notice from the bank ({}); the transfer it returns is not in the ledger.",
                    t.reference.as_deref().unwrap_or("no reference")
                ))
                .category("Reversed"),
            ),
            None => {}
        }
    }
    out
}

/// Money leaving one account and the same amount arriving in another within
/// minutes. With the owner named on either side it is certain; between two
/// unnamed accounts it is very likely but asked.
///
/// The arrival may have been removed from the ledger already (money in is
/// reviewed and removed by hand): it still arrived, so it still pairs.
fn internal_pairs(model: &Model, taken: &BTreeMap<i64, Decision>) -> Vec<(Decision, Decision)> {
    let is_refund = |t: &Txn| {
        t.template
            .as_deref()
            .is_some_and(|n| REFUND_TEMPLATES.contains(&n))
    };
    let own = |t: &Txn| model.payee_for(t).filter(|p| p.is_own());
    // Either side naming someone else (a supplier paid, a customer paying)
    // is ordinary business that happens to share an amount.
    let usable = |t: &Txn| {
        (t.open() || !t.is_out() && t.deleted && !t.split && !t.child)
            && !taken.contains_key(&t.id)
            && !is_refund(t)
            && (t.counterparty.is_none() || own(t).is_some())
    };

    let outs: Vec<&Txn> = model
        .txns
        .iter()
        .filter(|t| t.is_out() && usable(t))
        .collect();
    let mut used: HashSet<i64> = HashSet::new();
    let mut pairs = Vec::new();

    for b in model.txns.iter().filter(|t| !t.is_out() && usable(t)) {
        let mut cands: Vec<(&Txn, i64)> = outs
            .iter()
            .copied()
            .filter(|a| {
                a.amount == b.amount
                    && !used.contains(&a.id)
                    && match (&a.account, &b.account) {
                        (Some(x), Some(y)) => x != y,
                        _ => true,
                    }
            })
            .filter_map(|a| {
                let dt = (b.occurred_at - a.occurred_at).num_minutes();
                (-10..=30).contains(&dt).then_some((a, dt.abs()))
            })
            .collect();
        if cands.is_empty() {
            continue;
        }
        cands.sort_by_key(|(a, gap)| (*gap, a.id));
        let (a, gap) = cands[0];
        let close_rivals = cands.iter().skip(1).filter(|(_, g)| *g <= gap + 5).count();
        let named = own(a).or_else(|| own(b));
        let confidence = match (named.is_some(), close_rivals == 0) {
            (true, true) => 0.97,
            (true, false) => 0.85,
            (false, true) => 0.90,
            (false, false) => 0.70,
        };
        used.insert(a.id);
        let lag = (b.occurred_at - a.occurred_at).num_minutes();
        let via = named
            .map(|p| format!(" by way of {}", p.name))
            .unwrap_or_default();
        let rivals = if close_rivals > 0 {
            format!(" {close_rivals} other transfer(s) of that amount were as close: check this is the one.")
        } else {
            String::new()
        };
        let into = b
            .account
            .as_deref()
            .map(|x| format!(" in account {x}"))
            .unwrap_or_default();
        let from = a
            .account
            .as_deref()
            .map(|x| format!(" account {x}"))
            .unwrap_or_default();
        let da = Decision::new(a, "internal_pair", confidence, format!(
            "The same {} arrived{into} {}{via}: money moving between the company's own accounts.{rivals}",
            money(a.amount), relative(lag)
        ))
        .category("InternalTransfer")
        .pair(b.id);
        let db = Decision::new(b, "internal_pair", confidence, format!(
            "The same {} left{from} {}{via}: money moving between the company's own accounts.{rivals}",
            money(b.amount), relative(-lag)
        ))
        .category("InternalTransfer")
        .pair(a.id);
        let (da, db) = match named {
            Some(p) => (da.payee(p), db.payee(p)),
            None => (da, db),
        };
        pairs.push((da, db));
    }
    pairs
}

/* ------------------------- 2. advances in loans -------------------------- */

#[derive(Clone, Copy, PartialEq)]
enum PartyFrom {
    Label,
    Name,
}

/// Who a payment went to, when that is known: already on the transaction, a
/// labelled payee, or a name that fits exactly one driver on at least
/// `min_words` words (a short name on the books — "احمد محمد" — fits many
/// people on two).
fn party_of(
    model: &Model,
    t: &Txn,
    min_words: usize,
) -> Option<(Option<i64>, Option<i64>, PartyFrom)> {
    if t.has_party() {
        return Some((t.driver_id, t.employee_id, PartyFrom::Label));
    }
    if let Some(p) = model.payee_for(t) {
        if p.driver_id.is_some() || p.employee_id.is_some() {
            return Some((p.driver_id, p.employee_id, PartyFrom::Label));
        }
        return None; // a labelled payee who is not a person on our books
    }
    let cp = t.counterparty.as_deref()?;
    if !has_arabic(cp) {
        return None; // a Latin spelling cannot be checked against Arabic names
    }
    let fits: Vec<i64> = model
        .drivers
        .iter()
        .filter(|d| mask_overlap(cp, &d.name).is_some_and(|n| n >= min_words))
        .map(|d| d.id)
        .collect();
    match fits.as_slice() {
        [one] => Some((Some(*one), None, PartyFrom::Name)),
        _ => None,
    }
}

fn advance_links(model: &Model, cands: &[&Txn]) -> Vec<Decision> {
    let mut out = Vec::new();
    let mut claimed: HashSet<i64> = HashSet::new();
    let same_day = |t: &Txn, l: &Loan| l.date == t.cairo_date() && l.amount == t.amount;

    // Named payments first: they claim their own person's advance.
    let mut unresolved: Vec<&Txn> = Vec::new();
    for &t in cands {
        if !model.open_loans.iter().any(|l| same_day(t, l)) {
            continue;
        }
        // A labelled supplier, the owner, a bank: not someone's advance.
        if model
            .payee_for(t)
            .is_some_and(|p| p.driver_id.is_none() && p.employee_id.is_none())
        {
            continue;
        }
        // A same-day advance of the same amount backs the name up, so two
        // words of it are enough here.
        match party_of(model, t, 2) {
            Some((driver, employee, from)) => {
                let mine: Vec<&Loan> = model
                    .open_loans
                    .iter()
                    .filter(|l| {
                        same_day(t, l)
                            && !claimed.contains(&l.id)
                            && (l.driver_id.is_some() && l.driver_id == driver
                                || l.employee_id.is_some() && l.employee_id == employee)
                    })
                    .collect();
                // Two same-day advances of one amount to one person are
                // interchangeable: take the first still unclaimed.
                if let Some(l) = mine.first() {
                    claimed.insert(l.id);
                    let who = model
                        .person_name(l.driver_id, l.employee_id)
                        .unwrap_or("them");
                    let (confidence, how) = match from {
                        PartyFrom::Label => (0.98, "the label names"),
                        PartyFrom::Name => (0.96, "the bank's name fits"),
                    };
                    out.push(
                        Decision::new(t, "advance_link", confidence, format!(
                            "The {} {} to {who} on {} is already in the loans list, and {how} {who}: this payment is it.",
                            money(l.amount), kind_word(l), fmt_date(t)
                        ))
                        .category(kind_category(l))
                        .party(l.driver_id, l.employee_id),
                    );
                    out.last_mut().unwrap().linked_loan_id = Some(l.id);
                }
            }
            None => unresolved.push(t),
        }
    }

    // Unnamed (or unplaceable) payments: same day, same amount. Never sure
    // enough to apply; one-to-one is a strong hint, a crowd only a lead.
    let mut groups: BTreeMap<(chrono::NaiveDate, Decimal), Vec<&Txn>> = BTreeMap::new();
    for t in unresolved {
        groups
            .entry((t.cairo_date(), t.amount))
            .or_default()
            .push(t);
    }
    for ((date, amount), mut ts) in groups {
        ts.sort_by_key(|t| (t.occurred_at, t.id));
        let mut loans: Vec<&Loan> = model
            .open_loans
            .iter()
            .filter(|l| l.date == date && l.amount == amount && !claimed.contains(&l.id))
            .collect();
        loans.sort_by_key(|l| l.id);
        if loans.is_empty() {
            continue;
        }
        let names: Vec<&str> = loans
            .iter()
            .filter_map(|l| model.person_name(l.driver_id, l.employee_id))
            .collect();
        let one_to_one = loans.len() == 1 && ts.len() == 1;
        for (t, l) in ts.iter().zip(loans.iter()) {
            claimed.insert(l.id);
            let who = model
                .person_name(l.driver_id, l.employee_id)
                .unwrap_or("someone");
            let reason = if one_to_one {
                format!(
                    "The loans list has a {} {} to {who} on {}, and this is the only {} payment that day.",
                    money(l.amount), kind_word(l), fmt_date(t), money(t.amount)
                )
            } else {
                format!(
                    "{} of {} on {} and {} of that amount in the loans list ({}). Check who this one paid.",
                    count(ts.len(), "payment"), money(t.amount), fmt_date(t),
                    count(loans.len(), kind_word(l)), names.join(", ")
                )
            };
            let mut d = Decision::new(
                t,
                "advance_link",
                if one_to_one { 0.90 } else { 0.60 },
                reason,
            )
            .category(kind_category(l))
            .party(l.driver_id, l.employee_id);
            d.linked_loan_id = Some(l.id);
            out.push(d);
        }
    }
    out
}

fn kind_category(l: &Loan) -> &'static str {
    if l.kind == "loan" {
        "Loan"
    } else {
        "Advance"
    }
}

fn kind_word(l: &Loan) -> &'static str {
    if l.kind == "loan" {
        "loan"
    } else {
        "advance"
    }
}

/* ------------------------- loan installments ----------------------------- */

const BANK_LOAN: &str = "BankLoan";

/// How far from its due date an installment is paid: early when it is taken
/// to the branch ahead of time, late when the 1st falls on a weekend.
const DUE_WINDOW_DAYS: i64 = 7;

/// People also simply send round sums like 20,000 or 40,000, so a transfer
/// matching an installment that small is a hint. From this size up it is the
/// installment.
const DISTINCT_INSTALLMENT: i64 = 50_000;

/// What the loan sheet says about one transfer to someone who pays loans.
#[derive(Debug, Clone, PartialEq)]
pub enum LoanFit {
    /// Exactly the installment of one loan falling due near it.
    One { loan: i64, due: NaiveDate },
    /// The sum of several installments falling due near it: the fewest that
    /// add up (any such sum would do; which loans it paid is not known).
    Several(Vec<(i64, NaiveDate)>),
    /// Nothing falling due near it adds up to it.
    Nothing,
}

/// Every transfer to a payee who pays loans, against the loan sheet, oldest
/// first. An installment is paid once: transfers that are exactly one claim
/// theirs, then the others are checked against what is left. Transfers
/// already known to be something else (refunded, moved between accounts)
/// pay nothing.
fn loan_fits(model: &Model, decided: &BTreeMap<i64, Decision>) -> HashMap<i64, LoanFit> {
    let mut fits = HashMap::new();
    if model.bank_loans.is_empty() {
        return fits; // no sheet: nothing to check against
    }
    let excluded = |c: Option<&str>| {
        c.and_then(|c| model.category(c))
            .is_some_and(|c| c.report_group == "excluded")
    };
    let payments: Vec<(&Txn, &Payee)> = model
        .txns
        .iter()
        .filter(|t| t.live() && !t.child && t.is_out() && !excluded(t.category.as_deref()))
        .filter(|t| !excluded(decided.get(&t.id).and_then(|d| d.category.as_deref())))
        .filter_map(|t| model.payee_for(t).filter(|p| p.pays_loans).map(|p| (t, p)))
        .collect();

    // (index into bank_loans, due date) for each installment due near `t`
    // that `p` may pay, largest first.
    let due_near = |t: &Txn, p: &Payee| {
        let mut v: Vec<(usize, NaiveDate)> = model
            .bank_loans
            .iter()
            .enumerate()
            .filter(|(_, l)| l.payee_id.is_none_or(|id| id == p.id))
            .flat_map(|(i, l)| {
                l.due_near(t.cairo_date(), DUE_WINDOW_DAYS)
                    .into_iter()
                    .map(move |d| (i, d))
            })
            .collect();
        v.sort_by(|a, b| {
            let (la, lb) = (&model.bank_loans[a.0], &model.bank_loans[b.0]);
            lb.installment
                .cmp(&la.installment)
                .then(la.id.cmp(&lb.id))
                .then(a.1.cmp(&b.1))
        });
        v
    };

    let mut claimed: HashSet<(usize, NaiveDate)> = HashSet::new();
    for &(t, p) in &payments {
        let exact = due_near(t, p)
            .into_iter()
            .filter(|k| model.bank_loans[k.0].installment == t.amount && !claimed.contains(k))
            .min_by_key(|(i, d)| {
                (
                    (*d - t.cairo_date()).num_days().abs(),
                    model.bank_loans[*i].id,
                )
            });
        if let Some(k) = exact {
            claimed.insert(k);
            fits.insert(
                t.id,
                LoanFit::One {
                    loan: model.bank_loans[k.0].id,
                    due: k.1,
                },
            );
        }
    }
    for &(t, p) in &payments {
        if fits.contains_key(&t.id) {
            continue;
        }
        let pool: Vec<(usize, NaiveDate)> = due_near(t, p)
            .into_iter()
            .filter(|k| !claimed.contains(k))
            .collect();
        let amounts: Vec<Decimal> = pool
            .iter()
            .map(|(i, _)| model.bank_loans[*i].installment)
            .collect();
        let fit = match fewest_adding_up(&amounts, t.amount).filter(|s| s.len() >= 2) {
            Some(parts) => {
                let parts: Vec<(usize, NaiveDate)> = parts.into_iter().map(|j| pool[j]).collect();
                claimed.extend(parts.iter().copied());
                LoanFit::Several(
                    parts
                        .into_iter()
                        .map(|(i, d)| (model.bank_loans[i].id, d))
                        .collect(),
                )
            }
            None => LoanFit::Nothing,
        };
        fits.insert(t.id, fit);
    }
    fits
}

/// The decision for a transfer the loan sheet explains; `None` when it
/// doesn't (the payee's label decides then).
fn installment(model: &Model, t: &Txn, p: &Payee, fit: &LoanFit) -> Option<Decision> {
    let loan = |id: i64| model.bank_loans.iter().find(|l| l.id == id);
    let day = |d: &NaiveDate| d.format("%-d %b").to_string();
    match fit {
        LoanFit::One { loan: id, due } => {
            let l = loan(*id)?;
            let (confidence, why) = if t.amount >= Decimal::from(DISTINCT_INSTALLMENT) {
                (0.97, format!("and {} pays the company's loans.", p.name))
            } else {
                (
                    0.85,
                    format!(
                        "but {} also gets sums this size for other things: check.",
                        p.name
                    ),
                )
            };
            Some(
                Decision::new(
                    t,
                    "loan_installment",
                    confidence,
                    format!(
                        "{} is the {} installment due {}, {why}",
                        money(t.amount),
                        l.name,
                        day(due)
                    ),
                )
                .category(BANK_LOAN)
                .payee(p),
            )
        }
        LoanFit::Several(parts) => {
            let named: Vec<String> = parts
                .iter()
                .filter_map(|(id, _)| loan(*id))
                .take(3)
                .map(|l| format!("{} {}", l.name, money(l.installment)))
                .collect();
            let more = match parts.len().saturating_sub(named.len()) {
                0 => String::new(),
                n => format!(" + {n} more"),
            };
            Some(
                Decision::new(
                    t,
                    "loan_installment",
                    0.80,
                    format!(
                        "{} adds up to {} installments due around {}: {}{more}. {} pays the company's loans; which ones this paid isn't certain.",
                        money(t.amount),
                        parts.len(),
                        fmt_date(t),
                        named.join(" + "),
                        p.name
                    ),
                )
                .category(BANK_LOAN)
                .payee(p),
            )
        }
        LoanFit::Nothing => None,
    }
}

/// The fewest of `amounts` (each used at most once) that add up to `target`
/// exactly, as indices; ties go to the subset found first, so earlier
/// amounts win. `None` when no subset does, or the search is too big to be
/// worth an answer.
fn fewest_adding_up(amounts: &[Decimal], target: Decimal) -> Option<Vec<usize>> {
    const MAX_STATES: usize = 250_000;
    let cents = |d: Decimal| (d * Decimal::from(100)).round().to_i64();
    let target = cents(target)?;
    let items: Vec<i64> = amounts.iter().map(|a| cents(*a)).collect::<Option<_>>()?;
    if target <= 0 || items.iter().sum::<i64>() < target {
        return None;
    }
    // Reachable sum → the fewest items reaching it.
    let mut best: BTreeMap<i64, Vec<usize>> = BTreeMap::from([(0, Vec::new())]);
    for (i, &a) in items.iter().enumerate() {
        if a <= 0 {
            continue;
        }
        let grown: Vec<(i64, Vec<usize>)> = best
            .range(..=target - a)
            .map(|(s, set)| {
                let mut set = set.clone();
                set.push(i);
                (s + a, set)
            })
            .collect();
        for (s, set) in grown {
            match best.get(&s) {
                Some(have) if have.len() <= set.len() => {}
                _ => {
                    best.insert(s, set);
                }
            }
        }
        if best.len() > MAX_STATES {
            return None;
        }
    }
    best.remove(&target)
}

/* --------------------------- 3-5. one row ------------------------------- */

/// What people have categorised before, per payee (or per spelling when no
/// label groups the spellings) and direction.
struct History {
    by_name: HashMap<(String, bool), Tally>,
}

#[derive(Default)]
struct Tally {
    n: u32,
    by_category: HashMap<String, (u32, HashMap<(Option<i64>, Option<i64>), u32>)>,
}

impl History {
    fn name_of(model: &Model, t: &Txn) -> Option<String> {
        match model.payee_for(t) {
            Some(p) => Some(format!("payee:{}", p.id)),
            None => t.key.as_ref().map(|k| format!("name:{k}")),
        }
    }

    fn build(model: &Model) -> History {
        let mut by_name: HashMap<(String, bool), Tally> = HashMap::new();
        for t in &model.txns {
            let Some(cat) = t.human_category() else {
                continue;
            };
            let Some(name) = History::name_of(model, t) else {
                continue;
            };
            let tally = by_name.entry((name, t.is_out())).or_default();
            tally.n += 1;
            let e = tally.by_category.entry(cat.to_string()).or_default();
            e.0 += 1;
            *e.1.entry((t.driver_id, t.employee_id)).or_default() += 1;
        }
        History { by_name }
    }

    /// The most common category (ties broken by name, for stability), how
    /// often, out of how many, and the person when every one of them had the
    /// same. Categories `skip` names are left out of both counts.
    fn top(
        &self,
        model: &Model,
        t: &Txn,
        skip: &dyn Fn(&str) -> bool,
    ) -> Option<(String, u32, u32, Option<(Option<i64>, Option<i64>)>)> {
        let tally = self
            .by_name
            .get(&(History::name_of(model, t)?, t.is_out()))?;
        let kept = || tally.by_category.iter().filter(|(c, _)| !skip(c));
        let n = kept().map(|(_, (k, _))| k).sum();
        let (cat, (k, parties)) =
            kept().max_by(|a, b| a.1 .0.cmp(&b.1 .0).then_with(|| b.0.cmp(a.0)))?;
        let party = (parties.len() == 1)
            .then(|| *parties.keys().next().unwrap())
            .filter(|(d, e)| d.is_some() || e.is_some());
        Some((cat.clone(), *k, n, party))
    }
}

fn single(
    model: &Model,
    history: &History,
    fits: &HashMap<i64, LoanFit>,
    t: &Txn,
) -> Option<Decision> {
    let body = t.body.as_deref().unwrap_or("");
    let cp = t.counterparty.as_deref().unwrap_or("");
    let payee = model.payee_for(t);

    if crate::parser::petroapp_pattern().is_match(cp) {
        return Some(
            Decision::new(
                t,
                "petroapp",
                0.99,
                "A PetroApp top-up: the fuel is counted from PetroApp's own bills.".into(),
            )
            .category("PetroApp"),
        );
    }

    if t.is_out() {
        // Cash out of an ATM is a fact, whatever the cash was for.
        if body.starts_with("تم سحب") || atm_word().is_match(cp) {
            return Some(
                Decision::new(
                    t,
                    "cash_withdrawal",
                    0.98,
                    format!("A cash withdrawal ({cp})."),
                )
                .category("CashWithdrawal"),
            );
        }
        // The 19555 bank charges the 0.1% IPN fee as its own message with an
        // empty reference.
        if matches!(
            t.template.as_deref(),
            Some("ref_balance" | "ref_balance_v2")
        ) && t.reference.is_none()
            && t.counterparty.is_none()
            && t.amount <= Decimal::from(5)
        {
            return Some(
                Decision::new(
                    t,
                    "bank_fee",
                    0.97,
                    format!(
                        "The {} IPN fee the bank charges separately.",
                        money(t.amount)
                    ),
                )
                .category("BankFees"),
            );
        }
    } else if let Some(p) = payee.filter(|p| p.is_own()) {
        return Some(
            Decision::new(
                t,
                "owner_inflow",
                0.96,
                format!(
                    "Money in from {} ({}): funding, not income.",
                    p.name,
                    if p.kind == "owner" {
                        "the owner"
                    } else {
                        "family"
                    }
                ),
            )
            .category("InternalTransfer")
            .payee(p),
        );
    }

    // Someone who pays the company's loans: the loan sheet says whether this
    // was an installment falling due.
    let fit = payee
        .filter(|p| t.is_out() && p.pays_loans)
        .and_then(|_| fits.get(&t.id));
    if let (Some(p), Some(fit)) = (payee, fit) {
        if let Some(d) = installment(model, t, p, fit) {
            return Some(d);
        }
    }

    // A label says what payments TO someone are; money in from them is
    // another matter.
    if let Some(p) = payee.filter(|_| t.is_out()) {
        let not_installment = matches!(fit, Some(LoanFit::Nothing));
        return Some(from_payee(model, history, t, p, not_installment));
    }

    if let Some((cat, k, n, party)) = history.top(model, t, &|_| false) {
        let p = succession(k, n);
        if p >= APPLY_AT || k * 10 >= n * 6 {
            let (d, e) = party.unwrap_or((None, None));
            return Some(
                Decision::new(
                    t,
                    "history",
                    p,
                    format!(
                        "{k} of {n} earlier payments {} {cp} were {}.",
                        if t.is_out() { "to" } else { "from" },
                        label_of(model, &cat)
                    ),
                )
                .category(&cat)
                .party(d, e),
            );
        }
    }

    if !t.is_out() {
        return None;
    }

    if t.counterparty.is_none()
        && t.amount >= Decimal::from(20_000)
        && t.account
            .as_ref()
            .is_some_and(|a| model.petro_accounts.contains(a))
    {
        return Some(
            Decision::new(t, "petroapp_topup", 0.85, format!(
                "An unnamed {} from account {}, which tops up PetroApp; its later top-ups name PetroApp, these don't.",
                money(t.amount), t.account.as_deref().unwrap_or("")
            ))
            .category("PetroApp"),
        );
    }

    if let Some((cat, why)) = merchant_hint(cp) {
        return Some(Decision::new(t, "merchant", 0.70, format!("{cp}: {why}.")).category(cat));
    }
    if cp.contains("AAIB NULL") {
        return Some(
            Decision::new(
                t,
                "aaib_atm",
                0.80,
                "AAIB with no merchant: how its ATM withdrawals appear.".into(),
            )
            .category("CashWithdrawal"),
        );
    }

    if let Some((Some(driver), None, PartyFrom::Name)) = party_of(model, t, 3) {
        let name = model.person_name(Some(driver), None).unwrap_or("");
        let how = if is_masked(cp) {
            "The bank's masked name fits"
        } else {
            "The name matches"
        };
        return Some(
            Decision::new(
                t,
                "name_fits",
                0.80,
                format!("{how} driver {name}, and no other driver."),
            )
            .party(Some(driver), None),
        );
    }
    None
}

/// What a payee's label (and what people chose since it) says. For someone
/// who pays loans, a transfer the loan sheet has ruled out as an installment
/// (`not_installment`) is judged by their other payments only.
fn from_payee(
    model: &Model,
    history: &History,
    t: &Txn,
    p: &Payee,
    not_installment: bool,
) -> Decision {
    let skip = |c: &str| not_installment && c.eq_ignore_ascii_case(BANK_LOAN);
    let top = history.top(model, t, &skip);
    // People's later decisions outrank the label: one since the label that
    // disagrees with it takes the label's automatic say away. Earlier ones
    // don't — the label is the newer word (and often names a category that
    // did not exist yet, filed under Other meanwhile).
    let since_label: Vec<&str> = model
        .txns
        .iter()
        .filter(|o| {
            o.is_out() == t.is_out()
                && o.decided_at.zip(p.labelled_at).is_some_and(|(d, l)| d > l)
                && model.payee_for(o).is_some_and(|q| q.id == p.id)
        })
        .filter_map(|o| o.human_category())
        .filter(|c| !skip(c))
        .collect();
    let contrary = p.category.as_deref().and_then(|c| {
        since_label
            .iter()
            .find(|h| !h.eq_ignore_ascii_case(c))
            .copied()
    });

    let mut d = match (&p.category, p.always, contrary) {
        (Some(cat), true, None) => Decision::new(
            t,
            "payee",
            0.98,
            format!(
                "Payments to {} are always {} (label).",
                p.name,
                label_of(model, cat)
            ),
        )
        .category(cat),
        (Some(cat), true, Some(other)) => Decision::new(
            t,
            "payee",
            0.80,
            format!(
                "The label says payments to {} are always {}, but one since was {}.",
                p.name,
                label_of(model, cat),
                label_of(model, other)
            ),
        )
        .category(cat),
        (cat, _, _) => {
            // What to offer: what people have chosen since the label, then
            // the label itself. Choices from before the label only speak for
            // a label that names no category.
            let recent = most_common(&since_label);
            let (cat, why) = match (recent, cat, &top) {
                (Some((h, k)), _, _) if since_label.len() >= 2 => (
                    Some(h.to_string()),
                    format!(
                        "{k} of {} since the label were {}",
                        since_label.len(),
                        label_of(model, h)
                    ),
                ),
                (_, Some(c), _) => (
                    Some(c.clone()),
                    format!("usually {} (label), but it varies", label_of(model, c)),
                ),
                (_, None, Some((h, k, n, _))) if *n >= 2 => (
                    Some(h.clone()),
                    format!("{k} of {n} earlier ones were {}", label_of(model, h)),
                ),
                _ => (None, "no usual category".to_string()),
            };
            let not_loan = if not_installment {
                format!(
                    " {} is no loan installment due around {}.",
                    money(t.amount),
                    fmt_date(t)
                )
            } else {
                String::new()
            };
            let mut d = Decision::new(
                t,
                "payee",
                if p.always { 0.80 } else { 0.75 },
                format!("{}: {why}.{not_loan}", p.name),
            );
            d.category = cat;
            d
        }
    }
    .payee(p);
    if p.driver_id.is_some() || p.employee_id.is_some() {
        d = d.party(p.driver_id, p.employee_id);
        d.fill_party = true;
    }
    d
}

/// The most frequent entry and its count; ties go to the first in name order.
fn most_common<'a>(xs: &[&'a str]) -> Option<(&'a str, usize)> {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for x in xs {
        *counts.entry(*x).or_default() += 1;
    }
    counts.into_iter().fold(None, |best, (k, n)| match best {
        Some((_, m)) if m >= n => best,
        _ => Some((k, n)),
    })
}

fn atm_word() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| regex::Regex::new(r"(?i)\bATM\d*\b").unwrap())
}

/// Merchant names that say what they sell. Hints only.
fn merchant_hint(cp: &str) -> Option<(&'static str, &'static str)> {
    static R: std::sync::OnceLock<[(regex::Regex, &str, &str); 3]> = std::sync::OnceLock::new();
    let rules = R.get_or_init(|| {
        [
            (
                // Wataniya's stations run into the place name ("WATANIYASAFAGA"),
                // so it is matched as a prefix; the rest as whole words.
                regex::Regex::new(
                    r"(?i)\bWATAN(I?YA|EYA)|\b(MOBIL|TOTAL ?ENERGIES|OLA ENERGY|PETRO ?TRADE|MISR PETROLEUM|EMARAT MISR|CIRCLE K)\b",
                )
                .unwrap(),
                "Fuel",
                "a fuel station",
            ),
            (
                regex::Regex::new(r"(?i)^(Mobile Recharge|Mobile|WE FBB.*)$").unwrap(),
                "Utilities",
                "phone or internet credit",
            ),
            (regex::Regex::new(r"(?i)\bBGOV\b").unwrap(), "GovFees", "a government payment"),
        ]
    });
    rules
        .iter()
        .find(|(r, _, _)| r.is_match(cp))
        .map(|(_, c, w)| (*c, *w))
}

fn label_of(model: &Model, key: &str) -> String {
    model
        .category(key)
        .map(|c| c.key.clone())
        .unwrap_or_else(|| key.to_string())
}

/* ------------------------------ wording ---------------------------------- */

/// 95000.0000 → "95,000"; 1250.5 → "1,250.50".
pub fn money(d: Decimal) -> String {
    let d = d.round_dp(2).normalize();
    let s = format!("{d:.2}");
    let (int, frac) = s.split_once('.').unwrap_or((&s, "00"));
    let mut grouped = String::new();
    for (i, c) in int.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(c);
    }
    let int: String = grouped.chars().rev().collect();
    if frac == "00" {
        int
    } else {
        format!("{int}.{frac}")
    }
}

fn fmt_date(t: &Txn) -> String {
    t.cairo_date().format("%-d %b").to_string()
}

/// "1 payment", "3 payments".
fn count(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("1 {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

/// "2 minutes later", "a minute earlier", "the same minute".
fn relative(minutes: i64) -> String {
    let word = if minutes < 0 { "earlier" } else { "later" };
    match minutes.abs() {
        0 => "the same minute".to_string(),
        1 => format!("a minute {word}"),
        n => format!("{n} minutes {word}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::fold::fold;
    use crate::classify::model::{BankLoan, CategoryInfo, Person};
    use chrono::{NaiveDateTime, TimeZone, Utc};
    use chrono_tz::Africa::Cairo;
    use std::str::FromStr;

    fn at(s: &str) -> chrono::DateTime<Utc> {
        Cairo
            .from_local_datetime(&NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap())
            .unwrap()
            .with_timezone(&Utc)
    }

    fn txn(id: i64, dir: &str, amount: &str, when: &str) -> Txn {
        Txn {
            id,
            direction: dir.into(),
            amount: Decimal::from_str(amount).unwrap(),
            occurred_at: at(when),
            account: None,
            counterparty: None,
            key: None,
            reference: None,
            template: None,
            body: None,
            category: None,
            driver_id: None,
            employee_id: None,
            loan_id: None,
            linked_loan_id: None,
            deleted: false,
            split: false,
            child: false,
            version: 1,
            status: None,
            decided_at: None,
        }
    }

    trait With {
        fn to(self, cp: &str) -> Self;
        fn acct(self, a: &str) -> Self;
        fn tpl(self, t: &str) -> Self;
        fn reference(self, r: &str) -> Self;
        fn cat(self, c: &str) -> Self;
    }
    impl With for Txn {
        fn to(mut self, cp: &str) -> Self {
            self.counterparty = Some(cp.into());
            self.key = Some(fold(cp));
            self
        }
        fn acct(mut self, a: &str) -> Self {
            self.account = Some(a.into());
            self
        }
        fn tpl(mut self, t: &str) -> Self {
            self.template = Some(t.into());
            self
        }
        fn reference(mut self, r: &str) -> Self {
            self.reference = Some(r.into());
            self
        }
        fn cat(mut self, c: &str) -> Self {
            self.category = Some(c.into());
            self
        }
    }

    fn model(txns: Vec<Txn>) -> Model {
        let mut m = Model {
            txns,
            ..Model::default()
        };
        for (key, posting, party, group) in [
            ("Advance", Some("advance"), "either", "operating"),
            ("Loan", Some("loan"), "either", "operating"),
            ("DriverCustody", None, "either", "operating"),
            ("Tires", None, "none", "operating"),
            ("Parts", None, "none", "operating"),
            ("Labor", None, "none", "operating"),
            ("Other", None, "none", "operating"),
            ("Fuel", None, "none", "operating"),
            ("Utilities", None, "none", "operating"),
            ("CashWithdrawal", None, "none", "operating"),
            ("BankFees", None, "none", "operating"),
            ("BankLoan", None, "none", "capital"),
            ("InternalTransfer", None, "none", "excluded"),
            ("PetroApp", None, "none", "excluded"),
            ("Reversed", None, "none", "excluded"),
        ] {
            m.categories.insert(
                key.to_lowercase(),
                CategoryInfo {
                    key: key.into(),
                    posting_kind: posting.map(String::from),
                    required_party: party.into(),
                    report_group: group.into(),
                },
            );
        }
        m.drivers = vec![
            Person {
                id: 16,
                name: "مينا عادل جرجس فهيم".into(),
            },
            // The books often keep fewer names than the bank does.
            Person {
                id: 22,
                name: "حسام فوزي".into(),
            },
            Person {
                id: 27,
                name: "كريم حسن ابراهيم".into(),
            },
        ];
        m
    }

    fn payee(
        m: &mut Model,
        id: i64,
        name: &str,
        kind: &str,
        cat: Option<&str>,
        always: bool,
        driver: Option<i64>,
        spellings: &[&str],
    ) {
        m.payees.insert(
            id,
            Payee {
                id,
                name: name.into(),
                kind: kind.into(),
                category: cat.map(String::from),
                always,
                driver_id: driver,
                employee_id: None,
                labelled_at: Some(at("2026-10-24 10:00")),
                pays_loans: false,
            },
        );
        for s in spellings {
            m.aliases.insert(fold(s), id);
        }
    }

    /// A bank loan on the sheet: `installment` on `due_day`, through
    /// `last_due`.
    fn bank_loan(id: i64, name: &str, installment: &str, due_day: u32, last_due: &str) -> BankLoan {
        BankLoan {
            id,
            name: name.into(),
            installment: Decimal::from_str(installment).unwrap(),
            due_day,
            first_due: None,
            last_due: chrono::NaiveDate::parse_from_str(last_due, "%Y-%m-%d").unwrap(),
            payee_id: None,
        }
    }

    fn loan(id: i64, driver: i64, amount: &str, date: &str) -> Loan {
        Loan {
            id,
            driver_id: Some(driver),
            employee_id: None,
            amount: Decimal::from_str(amount).unwrap(),
            date: chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").unwrap(),
            kind: "advance".into(),
        }
    }

    fn by_id(ds: &[Decision], id: i64) -> &Decision {
        ds.iter()
            .find(|d| d.txn_id == id)
            .unwrap_or_else(|| panic!("no decision for {id}: {ds:#?}"))
    }

    #[test]
    fn a_refund_cancels_its_transfer() {
        let m = model(vec![
            txn(1, "out", "850", "2026-08-13 18:34")
                .reference("FT26225ABCDE")
                .tpl("arabic_ipn")
                .to("Shop"),
            txn(2, "in", "850", "2026-08-13 18:34")
                .reference("FT26225ABCDE")
                .tpl("ipn_refund")
                .to("Shop"),
            // Its original is gone (deleted by hand): the refund alone.
            txn(4, "in", "85000", "2026-10-01 16:28")
                .reference("FT0000GONE")
                .tpl("ipn_refund"),
        ]);
        let ds = decide(&m, None);
        for id in [1, 2] {
            let d = by_id(&ds, id);
            assert_eq!(d.category.as_deref(), Some("Reversed"));
            assert!(d.apply, "{d:?}");
        }
        assert_eq!(by_id(&ds, 2).pair_id, Some(1));
        assert!(by_id(&ds, 4).apply);
    }

    #[test]
    fn a_refund_never_overwrites_a_category_someone_gave() {
        let m = model(vec![
            txn(1, "out", "850", "2026-08-13 18:34")
                .reference("FT1")
                .cat("Parts"),
            txn(2, "in", "850", "2026-08-13 18:40")
                .reference("FT1")
                .tpl("ipn_refund"),
        ]);
        let ds = decide(&m, None);
        let d = by_id(&ds, 1);
        assert_eq!(d.category.as_deref(), Some("Reversed"));
        assert!(
            !d.apply,
            "a person's category stays; this is only a suggestion"
        );
    }

    #[test]
    fn a_refund_replaces_the_classifiers_own_decision() {
        let mut original = txn(1, "out", "180000", "2026-08-13 12:00")
            .reference("FT7")
            .to("tirecorp")
            .cat("Tires");
        original.status = Some("applied".into());
        let m = model(vec![
            original,
            txn(2, "in", "180000", "2026-08-13 12:20")
                .reference("FT7")
                .tpl("ipn_refund")
                .to("tirecorp"),
        ]);
        let ds = decide(&m, None);
        let d = by_id(&ds, 1);
        assert_eq!(
            (d.category.as_deref(), d.revise, d.apply),
            (Some("Reversed"), true, true)
        );
    }

    #[test]
    fn a_third_party_on_either_side_is_not_an_internal_transfer() {
        let mut m = model(vec![
            txn(1, "out", "60000", "2026-09-03 12:00")
                .acct("1111")
                .to("Kareem"),
            txn(2, "in", "60000", "2026-09-03 12:05")
                .acct("5555")
                .to("SOME CUSTOMER LLC"),
        ]);
        payee(
            &mut m,
            1,
            "Kareem Samir",
            "owner",
            Some("InternalTransfer"),
            false,
            None,
            &["Kareem"],
        );
        let ds = decide(&m, None);
        assert!(ds.iter().all(|d| d.rule != "internal_pair"), "{ds:#?}");
    }

    #[test]
    fn own_accounts_moving_money_is_not_spending() {
        let mut m = model(vec![
            txn(1, "out", "95000", "2026-08-20 13:05")
                .acct("1111")
                .to("Kareem S.... H....."),
            txn(2, "in", "95000", "2026-08-20 13:07").acct("6666"),
            // Two unnamed accounts: very likely the same, but asked.
            txn(3, "out", "42000", "2026-09-01 10:00").acct("1111"),
            txn(4, "in", "42000", "2026-09-01 10:03").acct("9001-01"),
        ]);
        payee(
            &mut m,
            1,
            "Kareem Samir",
            "owner",
            Some("InternalTransfer"),
            false,
            None,
            &["Kareem S.... H....."],
        );
        let ds = decide(&m, None);
        for id in [1, 2] {
            let d = by_id(&ds, id);
            assert_eq!(
                (d.category.as_deref(), d.apply),
                (Some("InternalTransfer"), true),
                "{d:?}"
            );
        }
        assert_eq!(by_id(&ds, 1).pair_id, Some(2));
        for id in [3, 4] {
            let d = by_id(&ds, id);
            assert_eq!(d.category.as_deref(), Some("InternalTransfer"));
            assert!(!d.apply);
            assert!((d.confidence - 0.90).abs() < 1e-9);
        }
    }

    #[test]
    fn money_in_from_the_owner_is_funding() {
        let mut m = model(vec![txn(1, "in", "60000", "2026-10-06 14:26")
            .acct("5555")
            .to("KAREEM SAMIR HASSAN")]);
        payee(
            &mut m,
            1,
            "Kareem Samir",
            "owner",
            Some("InternalTransfer"),
            false,
            None,
            &["KAREEM SAMIR HASSAN"],
        );
        let d = &decide(&m, None)[0];
        assert_eq!(
            (d.rule, d.category.as_deref(), d.apply),
            ("owner_inflow", Some("InternalTransfer"), true)
        );
    }

    #[test]
    fn an_advance_in_the_loans_list_is_linked_not_registered_again() {
        let mut m = model(vec![
            // Named, and the name fits the driver: applied, linked.
            txn(1, "out", "1500", "2026-08-25 11:00").to("حسام ف*** ر***"),
            // Unnamed, the only 2,000 that day: a strong suggestion.
            txn(2, "out", "1500", "2026-08-26 12:00"),
            // A supplier paid the same amount the same day is not an advance.
            txn(3, "out", "1500", "2026-08-27 09:00").to("tirecorp"),
        ]);
        m.open_loans = vec![
            loan(100, 22, "1500", "2026-08-25"),
            loan(101, 27, "1500", "2026-08-26"),
            loan(102, 27, "1500", "2026-08-27"),
        ];
        payee(
            &mut m,
            1,
            "Tire Corp",
            "supplier",
            Some("Tires"),
            true,
            None,
            &["tirecorp"],
        );
        let ds = decide(&m, None);

        let d = by_id(&ds, 1);
        assert_eq!(
            (
                d.category.as_deref(),
                d.linked_loan_id,
                d.driver_id,
                d.apply
            ),
            (Some("Advance"), Some(100), Some(22), true)
        );
        let d = by_id(&ds, 2);
        assert_eq!(
            (
                d.category.as_deref(),
                d.linked_loan_id,
                d.driver_id,
                d.apply
            ),
            (Some("Advance"), Some(101), Some(27), false)
        );
        let d = by_id(&ds, 3);
        assert_eq!((d.rule, d.category.as_deref()), ("payee", Some("Tires")));
    }

    #[test]
    fn a_crowd_of_same_day_advances_is_only_a_lead() {
        let m = {
            let mut m = model(vec![
                txn(1, "out", "1500", "2026-08-14 10:00"),
                txn(2, "out", "1500", "2026-08-14 11:00"),
                txn(3, "out", "1500", "2026-08-14 12:00"),
            ]);
            m.open_loans = vec![
                loan(100, 22, "1500", "2026-08-14"),
                loan(101, 27, "1500", "2026-08-14"),
            ];
            m
        };
        let ds = decide(&m, None);
        let linked: Vec<_> = ds.iter().filter_map(|d| d.linked_loan_id).collect();
        assert_eq!(linked, vec![100, 101], "each loan offered once");
        assert!(ds.iter().all(|d| !d.apply && d.confidence < 0.7));
    }

    #[test]
    fn labels_always_apply_unless_someone_has_since_disagreed() {
        let mut m = model(vec![
            txn(1, "out", "180000", "2026-08-13 12:00").to("tirecorp"),
            txn(2, "out", "3000", "2026-08-14 12:00").to("مينا ع... ج... ف.."),
        ]);
        payee(
            &mut m,
            1,
            "Tire Corp",
            "supplier",
            Some("Tires"),
            true,
            None,
            &["tirecorp"],
        );
        payee(
            &mut m,
            2,
            "Mina Adel",
            "driver",
            Some("DriverCustody"),
            false,
            Some(16),
            &["مينا ع... ج... ف.."],
        );
        let ds = decide(&m, None);
        let d = by_id(&ds, 1);
        assert_eq!((d.category.as_deref(), d.apply), (Some("Tires"), true));
        // Varies: the person is filled in, the category waits.
        let d = by_id(&ds, 2);
        assert_eq!(
            (d.category.as_deref(), d.driver_id, d.apply, d.fill_party),
            (Some("DriverCustody"), Some(16), false, true)
        );

        // Before the label, filed under Other: the label still wins.
        let mut old = txn(3, "out", "260000", "2026-10-01 12:00")
            .to("tirecorp")
            .cat("Other");
        old.decided_at = Some(at("2026-10-01 13:00"));
        m.txns.push(old);
        assert!(by_id(&decide(&m, None), 1).apply);

        // After the label, someone chose Parts: ask from now on.
        let mut since = txn(4, "out", "100", "2026-10-25 12:00")
            .to("tirecorp")
            .cat("Parts");
        since.decided_at = Some(at("2026-10-25 13:00"));
        m.txns.push(since);
        let d = by_id(&decide(&m, None), 1).clone();
        assert_eq!((d.category.as_deref(), d.apply), (Some("Tires"), false));
        assert!(d.reason.contains("Parts"), "{}", d.reason);
    }

    #[test]
    fn choices_from_before_the_label_do_not_outrank_it() {
        let mut m = model(vec![
            txn(1, "out", "90000", "2026-10-25 12:00").to("family acct")
        ]);
        payee(
            &mut m,
            1,
            "Samira Adel",
            "family",
            Some("Tires"),
            false,
            None,
            &["family acct"],
        );
        for (id, when) in [(10, "2026-10-09 22:00"), (11, "2026-10-09 22:10")] {
            let mut t = txn(id, "out", "50000", "2026-10-01 12:00")
                .to("family acct")
                .cat("Other");
            t.decided_at = Some(at(when));
            m.txns.push(t);
        }
        let d = by_id(&decide(&m, None), 1).clone();
        assert_eq!(
            (d.category.as_deref(), d.apply),
            (Some("Tires"), false),
            "{}",
            d.reason
        );

        // Two choices since the label do speak.
        for id in [12, 13] {
            let mut t = txn(id, "out", "50000", "2026-10-25 09:00")
                .to("family acct")
                .cat("Other");
            t.decided_at = Some(at("2026-10-25 10:00"));
            m.txns.push(t);
        }
        let d = by_id(&decide(&m, None), 1).clone();
        assert_eq!(d.category.as_deref(), Some("Other"), "{}", d.reason);
    }

    /// A relative who pays the company's loans at the branch, and the sheet.
    fn loan_payer(txns: Vec<Txn>) -> Model {
        let mut m = model(txns);
        payee(
            &mut m,
            1,
            "Samira Adel",
            "family",
            Some("InternalTransfer"),
            false,
            None,
            &["samira", "سميرة عادل"],
        );
        m.payees.get_mut(&1).unwrap().pays_loans = true;
        payee(
            &mut m,
            2,
            "Loan account",
            "bank",
            Some("BankLoan"),
            true,
            None,
            &["loanacct"],
        );
        let mut via_account = bank_loan(5, "Bank E", "80000", 1, "2029-01-01");
        via_account.payee_id = Some(2);
        m.bank_loans = vec![
            bank_loan(1, "Bank A", "120000", 1, "2027-09-01"),
            bank_loan(2, "Bank B", "64000", 1, "2028-01-01"),
            bank_loan(3, "Bank C", "31000", 3, "2028-01-03"),
            bank_loan(4, "Bank D", "22000", 1, "2027-01-01"),
            via_account,
            bank_loan(6, "Bank F", "90000", 1, "2026-06-01"), // repaid in June
        ];
        m
    }

    #[test]
    fn an_installment_falling_due_is_a_loan_payment() {
        let ds = decide(
            &loan_payer(vec![
                txn(1, "out", "120000", "2026-09-01 11:15").to("samira"),
                // Bank A's September installment is taken: a second
                // 120,000 is not it again, and nothing else adds up to it.
                txn(2, "out", "120000", "2026-09-02 10:00").to("samira"),
                // Bank B + Bank C, both due in the first days of September.
                txn(3, "out", "95000", "2026-09-01 11:25").to("samira"),
                // Small, mid-month: savings, as the label says.
                txn(4, "out", "12500", "2026-09-09 09:00").to("samira"),
                // Bank D's, but people send 22,000 for other things too.
                txn(5, "out", "22000", "2026-10-02 15:30").to("samira"),
                // Bank E is only ever paid through its own account, and
                // Bank F was repaid in June.
                txn(6, "out", "80000", "2026-10-01 12:00").to("samira"),
                txn(7, "out", "90000", "2026-09-01 12:00").to("سميرة عادل"),
            ]),
            None,
        );
        let got = |id| {
            let d = by_id(&ds, id);
            (d.rule, d.category.as_deref(), d.apply)
        };
        assert_eq!(got(1), ("loan_installment", Some("BankLoan"), true));
        assert!(
            by_id(&ds, 1).reason.contains("Bank A"),
            "{}",
            by_id(&ds, 1).reason
        );
        assert_eq!(got(2), ("payee", Some("InternalTransfer"), false));
        assert_eq!(got(3), ("loan_installment", Some("BankLoan"), false));
        assert!(
            by_id(&ds, 3).reason.contains("2 installments"),
            "{}",
            by_id(&ds, 3).reason
        );
        assert_eq!(got(4), ("payee", Some("InternalTransfer"), false));
        assert!(
            by_id(&ds, 4).reason.contains("no loan installment"),
            "{}",
            by_id(&ds, 4).reason
        );
        assert_eq!(got(5), ("loan_installment", Some("BankLoan"), false));
        assert_eq!(got(6), ("payee", Some("InternalTransfer"), false));
        assert_eq!(got(7), ("payee", Some("InternalTransfer"), false));
    }

    #[test]
    fn loan_payments_people_accepted_do_not_make_savings_look_like_loans() {
        let mut m = loan_payer(vec![txn(1, "out", "12500", "2026-11-09 09:00").to("samira")]);
        // Since the label, people filed two of Samira's transfers as loan
        // payments.
        for (id, day) in [(10, "2026-10-01 12:00"), (11, "2026-10-02 12:00")] {
            let mut t = txn(id, "out", "120000", day).to("samira").cat("BankLoan");
            t.decided_at = Some(at("2026-10-25 09:00"));
            m.txns.push(t);
        }
        let d = by_id(&decide(&m, None), 1).clone();
        assert_eq!(
            (d.rule, d.category.as_deref()),
            ("payee", Some("InternalTransfer")),
            "{}",
            d.reason
        );
    }

    #[test]
    fn without_a_loan_sheet_the_label_decides_alone() {
        let mut m = loan_payer(vec![
            txn(1, "out", "120000", "2026-09-01 11:15").to("samira")
        ]);
        m.bank_loans.clear();
        let d = by_id(&decide(&m, None), 1).clone();
        assert_eq!(
            (d.rule, d.category.as_deref()),
            ("payee", Some("InternalTransfer"))
        );
        assert!(!d.reason.contains("installment"), "{}", d.reason);
    }

    #[test]
    fn money_passing_through_family_is_moved_not_spent() {
        // Out of one account to Samira, back from her into another minutes
        // later. The arrival was reviewed and removed from the ledger.
        let mut back = txn(2, "in", "184000", "2026-08-27 16:14")
            .acct("5555")
            .to("سميرة عادل");
        back.deleted = true;
        let ds = decide(
            &loan_payer(vec![
                txn(1, "out", "184000", "2026-08-27 16:10")
                    .acct("1111")
                    .to("samira"),
                back,
            ]),
            None,
        );
        assert_eq!(ds.len(), 1, "{ds:#?}");
        let d = by_id(&ds, 1);
        assert_eq!(
            (d.rule, d.category.as_deref(), d.apply, d.pair_id),
            ("internal_pair", Some("InternalTransfer"), true, Some(2))
        );
    }

    #[test]
    fn the_fewest_installments_that_add_up() {
        let d = |v: &[i64]| v.iter().map(|x| Decimal::from(*x)).collect::<Vec<_>>();
        let amounts = d(&[50, 40, 30, 20, 10]);
        let pick = fewest_adding_up(&amounts, Decimal::from(60)).unwrap();
        assert_eq!(pick.len(), 2);
        assert_eq!(
            pick.iter().map(|&i| amounts[i]).sum::<Decimal>(),
            Decimal::from(60)
        );
        assert_eq!(
            fewest_adding_up(&amounts, Decimal::from(150))
                .unwrap()
                .len(),
            5
        );
        assert_eq!(fewest_adding_up(&amounts, Decimal::from(55)), None);
        assert_eq!(fewest_adding_up(&amounts, Decimal::from(151)), None);
        let odd = vec![
            Decimal::from_str("17430").unwrap(),
            Decimal::from_str("26110.50").unwrap(),
        ];
        assert_eq!(
            fewest_adding_up(&odd, Decimal::from_str("43540.50").unwrap()),
            Some(vec![0, 1])
        );
    }

    #[test]
    fn history_applies_only_with_enough_agreement() {
        let mut txns = vec![txn(1, "out", "500", "2026-10-20 12:00").to("OILS CO")];
        let mut id = 10;
        for _ in 0..18 {
            txns.push(
                txn(id, "out", "400", "2026-09-01 12:00")
                    .to("OILS CO")
                    .cat("Parts"),
            );
            id += 1;
        }
        txns.push(txn(2, "out", "500", "2026-10-20 12:00").to("MASTER WORKSHOP"));
        txns.push(
            txn(id, "out", "300", "2026-09-01 12:00")
                .to("MASTER WORKSHOP")
                .cat("Labor"),
        );
        let ds = decide(&model(txns), None);
        let d = by_id(&ds, 1);
        assert_eq!(
            (d.rule, d.category.as_deref(), d.apply),
            ("history", Some("Parts"), true)
        );
        let d = by_id(&ds, 2);
        assert_eq!((d.category.as_deref(), d.apply), (Some("Labor"), false));
        assert!((d.confidence - 2.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn its_own_output_is_not_evidence() {
        let mut txns = vec![txn(1, "out", "500", "2026-10-20 12:00").to("X")];
        for id in 10..40 {
            let mut t = txn(id, "out", "400", "2026-09-01 12:00")
                .to("X")
                .cat("Parts");
            t.status = Some("applied".into());
            txns.push(t);
        }
        assert!(decide(&model(txns), None).is_empty());
    }

    #[test]
    fn a_corrected_rule_loses_its_automatic_say() {
        let mut m = model(vec![
            txn(1, "out", "6000", "2026-09-02 09:53").to("NBE ATM101")
        ]);
        assert!(decide(&m, None)[0].apply);
        m.rule_record.insert("cash_withdrawal".into(), (3, 1));
        assert!(!decide(&m, None)[0].apply);
        m.rule_record.insert("cash_withdrawal".into(), (60, 1));
        assert!(decide(&m, None)[0].apply);
    }

    #[test]
    fn a_category_that_needs_a_person_waits_without_one() {
        let mut m = model(vec![
            txn(1, "out", "5000", "2026-09-02 10:00").to("CUSTODY GUY")
        ]);
        payee(
            &mut m,
            1,
            "Someone",
            "other",
            Some("DriverCustody"),
            true,
            None,
            &["CUSTODY GUY"],
        );
        let d = &decide(&m, None)[0];
        assert_eq!(
            (d.category.as_deref(), d.apply),
            (Some("DriverCustody"), false)
        );
    }

    #[test]
    fn hints_never_apply() {
        let mut m = model(vec![
            txn(1, "out", "60000", "2026-08-20 13:08").acct("6666"),
            txn(2, "out", "610", "2026-08-16 14:10").to("WATANIYA STATION 1"),
            txn(3, "out", "95.00", "2026-09-21 21:01").to("Mobile Recharge"),
        ]);
        m.petro_accounts.insert("6666".into());
        let ds = decide(&m, None);
        assert_eq!(by_id(&ds, 1).category.as_deref(), Some("PetroApp"));
        assert_eq!(by_id(&ds, 2).category.as_deref(), Some("Fuel"));
        assert_eq!(by_id(&ds, 3).category.as_deref(), Some("Utilities"));
        assert!(ds.iter().all(|d| !d.apply));
    }

    #[test]
    fn the_separate_ipn_fee_and_atm_cash() {
        let m = model(vec![
            txn(1, "out", "0.5", "2026-09-24 08:43")
                .tpl("ref_balance_v2")
                .acct("9001-01"),
            {
                let mut t = txn(2, "out", "6000", "2026-08-08 10:32")
                    .tpl("cib_card")
                    .to("CIB BRANCH 7");
                t.body = Some("تم سحب مبلغ EGP 6000.00 من بطاقه الخصم المباشر".into());
                t
            },
        ]);
        let ds = decide(&m, None);
        assert_eq!(
            (by_id(&ds, 1).category.as_deref(), by_id(&ds, 1).apply),
            (Some("BankFees"), true)
        );
        assert_eq!(
            (by_id(&ds, 2).category.as_deref(), by_id(&ds, 2).apply),
            (Some("CashWithdrawal"), true)
        );
    }

    #[test]
    fn targets_limit_the_run_but_partners_come_along() {
        let mut m = model(vec![
            txn(1, "out", "95000", "2026-08-20 13:05")
                .acct("1111")
                .to("Kareem"),
            txn(2, "in", "95000", "2026-08-20 13:07").acct("6666"),
            txn(3, "out", "6000", "2026-08-21 09:00").to("NBE ATM1"),
        ]);
        payee(
            &mut m,
            1,
            "Kareem Samir",
            "owner",
            Some("InternalTransfer"),
            false,
            None,
            &["Kareem"],
        );
        let only_new: HashSet<i64> = [2].into_iter().collect();
        let ids: Vec<i64> = decide(&m, Some(&only_new))
            .iter()
            .map(|d| d.txn_id)
            .collect();
        assert_eq!(ids, vec![1, 2]);
    }

    #[test]
    fn money_reads_like_money() {
        assert_eq!(money(Decimal::from_str("95000.0000").unwrap()), "95,000");
        assert_eq!(money(Decimal::from_str("1250.5").unwrap()), "1,250.50");
        assert_eq!(money(Decimal::from_str("0.5").unwrap()), "0.50");
        assert_eq!(money(Decimal::from_str("999").unwrap()), "999");
    }
}
