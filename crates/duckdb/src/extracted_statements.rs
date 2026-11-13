use std::fmt;

use super::{Connection, Result, Statement, ffi};
use crate::{error::duckdb_failure_from_message, raw_statement::RawStatement};

/// The statements of a block of SQL, extracted and not yet run.
///
/// Obtained from [`Connection::extract`]. Each statement is prepared or run by
/// its index, and each index can be used once: preparing a statement moves it
/// out of the set, so a prepared [`Statement`] outlives the set it came from.
pub struct ExtractedStatements<'conn> {
    conn: &'conn Connection,
    extracted: ffi::duckdb_extracted_statements,
    // One flag for each statement, set once the statement has left the set.
    consumed: Vec<bool>,
}

impl<'conn> ExtractedStatements<'conn> {
    /// # Safety
    ///
    /// `extracted` must be a live extracted-statements handle that holds
    /// `count` statements. The returned value takes ownership of it.
    #[inline]
    pub(crate) unsafe fn new(
        conn: &'conn Connection,
        extracted: ffi::duckdb_extracted_statements,
        count: usize,
    ) -> Self {
        Self {
            conn,
            extracted,
            consumed: vec![false; count],
        }
    }

    /// The number of statements the block holds, whether or not they have
    /// been prepared or run.
    #[inline]
    pub fn len(&self) -> usize {
        self.consumed.len()
    }

    /// Whether the block holds no statement.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.consumed.is_empty()
    }

    /// Prepare the statement at `index`, counted from zero.
    ///
    /// The statement leaves the set whether or not preparing it succeeds.
    ///
    /// # Failure
    ///
    /// Will return `Err` if `index` is out of range, if the statement at
    /// `index` has already been prepared or run, or if the underlying DuckDB
    /// call fails.
    pub fn prepare(&mut self, index: usize) -> Result<Statement<'conn>> {
        self.consume(index)?;
        let stmt = self
            .conn
            .db
            .borrow()
            .prepare_extracted_statement(self.extracted, index as ffi::idx_t)?;
        Ok(Statement::new(self.conn, unsafe { RawStatement::new(stmt) }))
    }

    /// Run the statement at `index`, counted from zero, and discard its
    /// result.
    ///
    /// Nothing decodes the discarded result, so this applies no check to the
    /// types of the result columns: a statement whose result holds a column
    /// this crate cannot decode runs here, and fails through
    /// [`Statement::execute`].
    ///
    /// # Failure
    ///
    /// Will return `Err` if `index` is out of range, if the statement at
    /// `index` has already been prepared or run, or if the underlying DuckDB
    /// call fails.
    pub fn execute_and_discard(&mut self, index: usize) -> Result<()> {
        self.consume(index)?;
        self.conn
            .db
            .borrow()
            .execute_extracted_statement(self.extracted, index as ffi::idx_t)
    }

    fn consume(&mut self, index: usize) -> Result<()> {
        let count = self.consumed.len();
        match self.consumed.get_mut(index) {
            None => Err(duckdb_failure_from_message(format!(
                "extracted statement index {index} is out of range for {count} statements"
            ))),
            Some(true) => Err(duckdb_failure_from_message(format!(
                "extracted statement {index} was already prepared"
            ))),
            Some(consumed) => {
                *consumed = true;
                Ok(())
            }
        }
    }
}

impl Drop for ExtractedStatements<'_> {
    fn drop(&mut self) {
        unsafe { ffi::duckdb_destroy_extracted(&mut self.extracted) }
    }
}

impl fmt::Debug for ExtractedStatements<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExtractedStatements").field("len", &self.len()).finish()
    }
}

#[cfg(test)]
mod test {
    use crate::{Connection, Error, Result, ffi, types::Type};

    fn failure<T>(result: Result<T>) -> (ffi::Error, String) {
        match result {
            Ok(_) => panic!("expected a failure"),
            Err(Error::DuckDBFailure(err, message)) => (err, message.unwrap_or_default()),
            Err(err) => panic!("unexpected error: {err:?}"),
        }
    }

    fn count(db: &Connection, table: &str) -> Result<i64> {
        db.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| row.get(0))
    }

    #[test]
    fn extract_counts_statements_and_runs_none() -> Result<()> {
        let db = Connection::open_in_memory()?;

        let statements = db.extract("CREATE TABLE t(i INTEGER); INSERT INTO t VALUES (1); SELECT i FROM t")?;

        assert_eq!(statements.len(), 3);
        assert!(!statements.is_empty());
        drop(statements);
        assert!(db.prepare("SELECT i FROM t").is_err());
        Ok(())
    }

    #[test]
    fn extract_returns_an_empty_set_for_a_block_without_statements() -> Result<()> {
        let db = Connection::open_in_memory()?;

        for sql in ["", "   ", ";", "-- a comment"] {
            let mut statements = db.extract(sql)?;
            assert_eq!(statements.len(), 0, "{sql:?}");
            assert!(statements.is_empty(), "{sql:?}");
            assert!(statements.prepare(0).is_err(), "{sql:?}");
            assert!(statements.execute_and_discard(0).is_err(), "{sql:?}");
        }

        // `prepare` keeps reporting such a block as an error.
        assert!(db.prepare("").is_err());
        Ok(())
    }

    #[test]
    fn extract_fails_on_a_parse_error_before_any_statement_runs() -> Result<()> {
        let db = Connection::open_in_memory()?;

        let (err, message) = failure(db.extract("CREATE TABLE t(i INTEGER); SELEC 1"));

        assert_eq!(err.error_type, ffi::duckdb_error_type_DUCKDB_ERROR_PARSER);
        assert!(message.contains("syntax error"), "{message}");
        assert!(db.prepare("SELECT i FROM t").is_err());
        Ok(())
    }

    #[test]
    fn caller_runs_the_block_statement_by_statement() -> Result<()> {
        let db = Connection::open_in_memory()?;
        let mut statements =
            db.extract("CREATE TABLE t(i INTEGER); INSERT INTO t VALUES (1), (2); SELECT sum(i) FROM t")?;

        statements.execute_and_discard(0)?;
        assert_eq!(count(&db, "t")?, 0);
        statements.execute_and_discard(1)?;
        assert_eq!(count(&db, "t")?, 2);

        let mut stmt = statements.prepare(2)?;
        let sum: i64 = stmt.query_row([], |row| row.get(0))?;
        assert_eq!(sum, 3);
        Ok(())
    }

    #[test]
    fn prepared_statement_outlives_the_set() -> Result<()> {
        let db = Connection::open_in_memory()?;
        let mut statements = db.extract("SELECT 1; SELECT ? + 1")?;

        let mut stmt = statements.prepare(1)?;
        drop(statements);

        let value: i64 = stmt.query_row([41], |row| row.get(0))?;
        assert_eq!(value, 42);
        let value: i64 = stmt.query_row([1], |row| row.get(0))?;
        assert_eq!(value, 2);
        Ok(())
    }

    #[test]
    fn an_index_is_used_once() -> Result<()> {
        let db = Connection::open_in_memory()?;
        db.execute_batch("CREATE TABLE t(i INTEGER)")?;
        let mut statements = db.extract("SELECT 1; INSERT INTO t VALUES (1); SELECT * FROM missing_table")?;

        let _stmt = statements.prepare(0)?;
        let (_, message) = failure(statements.prepare(0));
        assert!(message.contains("already prepared"), "{message}");
        let (_, message) = failure(statements.execute_and_discard(0));
        assert!(message.contains("already prepared"), "{message}");

        statements.execute_and_discard(1)?;
        let (_, message) = failure(statements.execute_and_discard(1));
        assert!(message.contains("already prepared"), "{message}");
        let (_, message) = failure(statements.prepare(1));
        assert!(message.contains("already prepared"), "{message}");
        assert_eq!(count(&db, "t")?, 1);

        // A statement that fails to prepare has left the set as well.
        let (err, _) = failure(statements.prepare(2));
        assert_eq!(err.error_type, ffi::duckdb_error_type_DUCKDB_ERROR_CATALOG);
        let (_, message) = failure(statements.prepare(2));
        assert!(message.contains("already prepared"), "{message}");

        let (_, message) = failure(statements.prepare(3));
        assert!(message.contains("out of range"), "{message}");
        assert_eq!(statements.len(), 3);
        Ok(())
    }

    #[test]
    fn execute_and_discard_failure_carries_error_type() -> Result<()> {
        let db = Connection::open_in_memory()?;
        db.execute_batch("CREATE TABLE t(i INTEGER PRIMARY KEY); INSERT INTO t VALUES (1)")?;
        let mut statements = db.extract("INSERT INTO t VALUES (1); SELECT * FROM missing_table; SELECT 1")?;

        let (err, message) = failure(statements.execute_and_discard(0));
        assert_eq!(err.error_type, ffi::duckdb_error_type_DUCKDB_ERROR_CONSTRAINT);
        assert!(message.contains("Constraint Error"), "{message}");

        // The statement fails while it is prepared.
        let (err, _) = failure(statements.execute_and_discard(1));
        assert_eq!(err.error_type, ffi::duckdb_error_type_DUCKDB_ERROR_CATALOG);
        Ok(())
    }

    #[test]
    fn prepare_runs_intermediate_statements_with_typed_failures() -> Result<()> {
        let db = Connection::open_in_memory()?;
        db.execute_batch("CREATE TABLE t(i INTEGER PRIMARY KEY); INSERT INTO t VALUES (1)")?;

        let (err, _) = failure(db.prepare("INSERT INTO t VALUES (1); SELECT 1"));

        assert_eq!(err.error_type, ffi::duckdb_error_type_DUCKDB_ERROR_CONSTRAINT);
        Ok(())
    }

    #[test]
    fn execute_and_discard_applies_no_variant_check() -> Result<()> {
        let db = Connection::open_in_memory()?;
        db.execute_batch("CREATE TABLE t(id INTEGER, v VARIANT)")?;
        let select = "SELECT {'a': 42}::VARIANT AS variant_col";
        let insert = "INSERT INTO t VALUES (1, {'a': 42}::VARIANT) RETURNING v";

        // Both checks of `Statement::execute` reject these statements.
        for sql in [select, insert] {
            match db.prepare(sql)?.execute([]) {
                Err(Error::FromSqlConversionFailure(_, Type::Variant, _)) => {}
                other => panic!("expected a Variant decode failure for {sql}, got {other:?}"),
            }
        }
        assert_eq!(count(&db, "t")?, 0);

        let mut statements = db.extract(&format!("{select}; {insert}"))?;
        statements.execute_and_discard(0)?;
        statements.execute_and_discard(1)?;
        assert_eq!(count(&db, "t")?, 1);
        Ok(())
    }
}
