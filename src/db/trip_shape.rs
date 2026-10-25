//! How trips are counted and how far they went — the parts of the statistics
//! that are not money.
//!
//! These used to sit in `db::revenue` next to the rates, because distance was
//! an input to TAQA's and Petromin's revenue. Revenue is now priced and stored
//! by FalconGo (see [`crate::db::revenue`]); what is left here is shape: what
//! a "trip" is, and which of its rows carries its distance.

/// Counts LOGICAL trips over a set of rows: a multi-container trip is one trip
/// however many receipts it carries, and a standalone trip is one trip.
///
/// `parent_trip_id` is 0 rather than NULL for some standalone rows — GORM wrote
/// both over the years — so both spellings mean "not a container".
pub fn logical_trip_count_sql(parent_col: &str) -> String {
    format!(
        "COALESCE(COUNT(DISTINCT {parent_col}) \
           FILTER (WHERE {parent_col} IS NOT NULL AND {parent_col} != 0), 0) \
         + COALESCE(COUNT(*) \
           FILTER (WHERE {parent_col} IS NULL OR {parent_col} = 0), 0)"
    )
}

/// A logical trip's distance, attributed to exactly one of its rows.
///
/// One truck makes ONE journey however many containers it carries, so adding
/// each container's mapped distance counts the same road several times. This
/// yields the furthest drop's distance on the row that holds it (ties to the
/// lowest id) and zero on that trip's other rows, so `SUM()` over any grouping
/// counts each journey once — and a trip whose drops span two groups lands
/// wholly in the group it travelled furthest for.
///
/// FalconGo's pricing engine attributes per-km revenue with the same window
/// (`priced_km`), so the distance column and the money agree on which receipt
/// carried the journey.
///
/// `parent_trip_id` is 0 rather than NULL on some standalone rows, so a
/// standalone trip keys on its own negated id — always negative, and therefore
/// never colliding with a real parent id.
pub fn trip_distance_sql(trips: &str, fees: &str) -> String {
    format!(
        "CASE WHEN ROW_NUMBER() OVER ( \
             PARTITION BY COALESCE(NULLIF({trips}.parent_trip_id, 0), -{trips}.id) \
             ORDER BY COALESCE({fees}.distance, 0.0) DESC, {trips}.id \
           ) = 1 \
         THEN COALESCE({fees}.distance, 0.0) ELSE 0.0 END::float8"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_standalone_row_counts_as_its_own_trip() {
        let sql = logical_trip_count_sql("parent_trip_id");
        assert!(sql.contains("parent_trip_id IS NULL OR parent_trip_id = 0"));
        assert!(sql.contains("COUNT(DISTINCT parent_trip_id)"));
    }

    #[test]
    fn distance_keys_standalone_rows_on_their_negated_id() {
        let sql = trip_distance_sql("t", "fm");
        assert!(sql.contains("COALESCE(NULLIF(t.parent_trip_id, 0), -t.id)"));
        assert!(sql.contains("ORDER BY COALESCE(fm.distance, 0.0) DESC, t.id"));
    }
}
